//! 沙盒环境约束预检层（可写性 / 配置可达性）。
//!
//! 本模块实现 R8 的「快速失败」预检：在写/编辑工具真正落盘前，先探测目标
//! 路径的可写性（R8.1/R8.2）；在依赖配置文件的脚本启动前，先校验约定配置
//! 路径的可达性（R8.3/R8.4）。
//!
//! 核心不变量（Property 24，R8.2/R8.4）：一旦某路径被判定为只读（或配置
//! 缺失）并记入 per-turn 已知集合，对同一路径的后续探查直接返回一致的
//! `readonly_path:<path>`（或 `config_missing:<path>`）错误，**不再触发实际
//! 文件系统探测**。
//!
//! 为了让「是否再次触达文件系统」可被确定性测试，本模块把「判定是否
//! 只读/缺失」与「缓存短路」两层解耦：
//! - 公开的 `check_writable` / `check_config_reachable` 使用真实文件系统探测；
//! - 带 `_with` 后缀的变体接受一个探测闭包，缓存命中时**不会调用**该闭包，
//!   从而属性测试可以注入带调用计数的闭包来验证短路行为。

use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// 只读路径错误前缀。错误串形如 `readonly_path:/abs/path`。
pub const READONLY_PREFIX: &str = "readonly_path:";

/// 配置缺失错误前缀。错误串形如 `config_missing:/abs/path`。
pub const CONFIG_MISSING_PREFIX: &str = "config_missing:";

/// 构造只读路径错误串。
pub fn readonly_error(path: &Path) -> String {
    format!("{READONLY_PREFIX}{}", path.display())
}

/// 构造配置缺失错误串。
pub fn config_missing_error(path: &Path) -> String {
    format!("{CONFIG_MISSING_PREFIX}{}", path.display())
}

/// per-turn 预检状态。
///
/// 用两个 `HashSet<PathBuf>` 分别记忆「已知只读」与「已知配置缺失」的路径。
/// 该状态应按回合（turn）创建一份新的实例，使记忆只在单个回合内短路。
#[derive(Debug, Default, Clone)]
pub struct PrecheckState {
    /// 已知只读路径集合（写前探测判定为不可写的路径）。
    readonly: HashSet<PathBuf>,
    /// 已知配置缺失路径集合（配置可达性探测判定为缺失的路径）。
    missing: HashSet<PathBuf>,
}

impl PrecheckState {
    /// 创建一个空的预检状态（对应一个新的 turn）。
    pub fn new() -> Self {
        Self::default()
    }

    /// 某路径是否已被记入「已知只读」集合。
    pub fn is_known_readonly(&self, path: &Path) -> bool {
        self.readonly.contains(path)
    }

    /// 某路径是否已被记入「已知配置缺失」集合。
    pub fn is_known_missing(&self, path: &Path) -> bool {
        self.missing.contains(path)
    }

    /// 已知只读路径数量。
    pub fn readonly_len(&self) -> usize {
        self.readonly.len()
    }

    /// 已知配置缺失路径数量。
    pub fn missing_len(&self) -> usize {
        self.missing.len()
    }

    /// 写前可写性预检（真实文件系统探测）。
    ///
    /// 只读返回 `Err("readonly_path:<path>")`；可写返回 `Ok(())`。
    /// 若该路径已在本回合内被判定为只读，则直接短路返回缓存错误，
    /// 不再触达文件系统。
    pub fn check_writable(&mut self, path: &Path) -> Result<(), String> {
        self.check_writable_with(path, probe_writable)
    }

    /// 写前可写性预检（注入探测闭包，便于确定性测试）。
    ///
    /// `probe(path)` 返回 `true` 表示路径可写、`false` 表示只读/不可写。
    /// **缓存命中（已知只读）时不会调用 `probe`**，从而保证「二次探查不触达
    /// 文件系统」（Property 24，R8.2）。
    pub fn check_writable_with<F>(&mut self, path: &Path, probe: F) -> Result<(), String>
    where
        F: FnOnce(&Path) -> bool,
    {
        // 短路：已知只读 → 直接返回缓存错误，不调用 probe。
        if self.readonly.contains(path) {
            return Err(readonly_error(path));
        }
        if probe(path) {
            Ok(())
        } else {
            // 记忆为已知只读，阻断后续重复探测。
            self.readonly.insert(path.to_path_buf());
            Err(readonly_error(path))
        }
    }

    /// 配置可达性预检（真实文件系统探测）。
    ///
    /// 缺失返回 `Err("config_missing:<path>")`；可达返回 `Ok(())`。
    /// 若该路径已在本回合内被判定为缺失，则直接短路返回缓存错误，
    /// 不再触达文件系统。
    pub fn check_config_reachable(&mut self, path: &Path) -> Result<(), String> {
        self.check_config_reachable_with(path, probe_reachable)
    }

    /// 配置可达性预检（注入探测闭包，便于确定性测试）。
    ///
    /// `probe(path)` 返回 `true` 表示配置可达、`false` 表示缺失。
    /// **缓存命中（已知缺失）时不会调用 `probe`**，从而保证「二次探查不触达
    /// 文件系统」（Property 24，R8.4）。
    pub fn check_config_reachable_with<F>(&mut self, path: &Path, probe: F) -> Result<(), String>
    where
        F: FnOnce(&Path) -> bool,
    {
        // 短路：已知缺失 → 直接返回缓存错误，不调用 probe。
        if self.missing.contains(path) {
            return Err(config_missing_error(path));
        }
        if probe(path) {
            Ok(())
        } else {
            // 记忆为已知缺失，阻断后续重复探查。
            self.missing.insert(path.to_path_buf());
            Err(config_missing_error(path))
        }
    }
}

/// 真实文件系统可写性探测。
///
/// 判定规则：
/// - 若目标路径已存在为文件/目录：其权限不是只读即视为可写；
/// - 若目标路径尚不存在：其父目录须存在且非只读（视为可在其中创建）。
///
/// 任何无法取得元数据的情况一律视为不可写（保守失败）。
fn probe_writable(path: &Path) -> bool {
    if let Ok(meta) = std::fs::metadata(path) {
        // 已存在：非只读即可写。
        return !meta.permissions().readonly();
    }
    // 不存在：检查父目录是否存在且可写。
    let parent = match path.parent() {
        // 空父路径（如裸文件名）按当前目录处理。
        Some(p) if p.as_os_str().is_empty() => Path::new("."),
        Some(p) => p,
        None => return false,
    };
    match std::fs::metadata(parent) {
        Ok(meta) => meta.is_dir() && !meta.permissions().readonly(),
        Err(_) => false,
    }
}

/// 真实文件系统配置可达性探测：约定路径存在即视为可达。
fn probe_reachable(path: &Path) -> bool {
    path.exists()
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use std::cell::Cell;
    use std::path::PathBuf;

    #[test]
    fn writable_ok_when_probe_true() {
        let mut state = PrecheckState::new();
        let p = PathBuf::from("/some/writable/path");
        assert_eq!(state.check_writable_with(&p, |_| true), Ok(()));
        // 可写不记入只读集合。
        assert!(!state.is_known_readonly(&p));
    }

    #[test]
    fn readonly_returns_prefixed_error_and_records() {
        let mut state = PrecheckState::new();
        let p = PathBuf::from("/readonly/skill/dir");
        let err = state.check_writable_with(&p, |_| false).unwrap_err();
        assert_eq!(err, format!("readonly_path:{}", p.display()));
        assert!(err.starts_with(READONLY_PREFIX));
        // 判定只读后记入集合。
        assert!(state.is_known_readonly(&p));
    }

    #[test]
    fn second_writable_probe_short_circuits_without_touching_fs() {
        let mut state = PrecheckState::new();
        let p = PathBuf::from("/readonly/path");
        let calls = Cell::new(0usize);

        // 首次探测：probe 被调用一次，判定只读。
        let first = state.check_writable_with(&p, |_| {
            calls.set(calls.get() + 1);
            false
        });
        assert!(first.is_err());
        assert_eq!(calls.get(), 1);

        // 二次探测：缓存短路，probe 不应再被调用，错误一致。
        let second = state.check_writable_with(&p, |_| {
            calls.set(calls.get() + 1);
            panic!("probe must not be called on cached readonly path");
        });
        assert_eq!(second, first);
        assert_eq!(calls.get(), 1, "probe should not run on second probe");
    }

    #[test]
    fn config_reachable_ok_when_probe_true() {
        let mut state = PrecheckState::new();
        let p = PathBuf::from("/.config/ty.config.toml");
        assert_eq!(state.check_config_reachable_with(&p, |_| true), Ok(()));
        assert!(!state.is_known_missing(&p));
    }

    #[test]
    fn config_missing_returns_prefixed_error_and_records() {
        let mut state = PrecheckState::new();
        let p = PathBuf::from("/.config/ty.config.toml");
        let err = state.check_config_reachable_with(&p, |_| false).unwrap_err();
        assert_eq!(err, format!("config_missing:{}", p.display()));
        assert!(err.starts_with(CONFIG_MISSING_PREFIX));
        assert!(state.is_known_missing(&p));
    }

    #[test]
    fn second_config_probe_short_circuits_without_touching_fs() {
        let mut state = PrecheckState::new();
        let p = PathBuf::from("/.config/missing.toml");
        let calls = Cell::new(0usize);

        let first = state.check_config_reachable_with(&p, |_| {
            calls.set(calls.get() + 1);
            false
        });
        assert!(first.is_err());
        assert_eq!(calls.get(), 1);

        let second = state.check_config_reachable_with(&p, |_| {
            calls.set(calls.get() + 1);
            panic!("probe must not be called on cached missing path");
        });
        assert_eq!(second, first);
        assert_eq!(calls.get(), 1, "probe should not run on second probe");
    }

    #[test]
    fn distinct_paths_are_tracked_independently() {
        let mut state = PrecheckState::new();
        let a = PathBuf::from("/readonly/a");
        let b = PathBuf::from("/writable/b");
        assert!(state.check_writable_with(&a, |_| false).is_err());
        assert!(state.check_writable_with(&b, |_| true).is_ok());
        assert!(state.is_known_readonly(&a));
        assert!(!state.is_known_readonly(&b));
        assert_eq!(state.readonly_len(), 1);
    }

    #[test]
    fn real_probe_detects_existing_writable_dir() {
        // 临时目录可写：真实 FS 探测应返回 Ok。
        let dir = std::env::temp_dir();
        let mut state = PrecheckState::new();
        // 目录本身（已存在且可写）。
        assert_eq!(state.check_writable(&dir), Ok(()));
    }

    #[test]
    fn real_probe_config_missing_for_nonexistent_path() {
        let mut state = PrecheckState::new();
        let p = PathBuf::from("/definitely/not/here/ty.config.toml");
        let err = state.check_config_reachable(&p).unwrap_err();
        assert!(err.starts_with(CONFIG_MISSING_PREFIX));
    }

    // ---- 集成测试（任务 11.5）：针对真实文件系统的首次探测行为 ----
    //
    // 这些测试使用 `tempfile::TempDir` 在真实文件系统上创建目录/文件，
    // 验证 `check_writable` / `check_config_reachable` 的首次探测语义：
    //   - R8.1：可写目录与可创建文件路径 → Ok；只读文件 → readonly_path 错误。
    //   - R8.3：约定配置文件存在 → Ok；缺失 → config_missing 错误。

    #[test]
    fn integration_writable_dir_and_creatable_file_first_probe_ok() {
        // R8.1：临时目录可写，目录内尚不存在的文件路径也可创建。
        let dir = tempfile::TempDir::new().expect("create tempdir");
        let mut state = PrecheckState::new();

        // 1) 目录本身（已存在且可写）→ Ok。
        assert_eq!(state.check_writable(dir.path()), Ok(()));

        // 2) 目录内尚不存在的文件路径（父目录可写）→ Ok（可创建）。
        let creatable = dir.path().join("new_output.txt");
        assert_eq!(state.check_writable(&creatable), Ok(()));

        // 可写路径不应被记入只读集合。
        assert!(!state.is_known_readonly(dir.path()));
        assert!(!state.is_known_readonly(&creatable));
    }

    #[test]
    fn integration_readonly_file_first_probe_returns_readonly_error() {
        // R8.1/R8.2：对真实只读文件的首次探测返回 readonly_path 错误并记忆。
        let dir = tempfile::TempDir::new().expect("create tempdir");
        let readonly_file = dir.path().join("locked.txt");
        std::fs::write(&readonly_file, b"locked").expect("write file");

        // 将文件设为只读（跨平台 API）。
        let mut perms = std::fs::metadata(&readonly_file)
            .expect("read metadata")
            .permissions();
        perms.set_readonly(true);
        std::fs::set_permissions(&readonly_file, perms).expect("set readonly");

        let mut state = PrecheckState::new();
        let err = state
            .check_writable(&readonly_file)
            .expect_err("readonly file must not be writable");
        assert!(
            err.starts_with(READONLY_PREFIX),
            "expected readonly_path prefix, got: {err}"
        );
        // 首次探测后记入已知只读集合，阻断重复写入尝试。
        assert!(state.is_known_readonly(&readonly_file));

        // 恢复写权限以便 TempDir 清理（部分平台需要可写才能删除）。
        let mut restore = std::fs::metadata(&readonly_file)
            .expect("read metadata")
            .permissions();
        restore.set_readonly(false);
        let _ = std::fs::set_permissions(&readonly_file, restore);
    }

    #[test]
    fn integration_config_reachable_first_probe_ok_and_missing_errors() {
        // R8.3：约定配置文件存在 → Ok；同一回合内缺失路径 → config_missing 错误。
        let dir = tempfile::TempDir::new().expect("create tempdir");
        let config_file = dir.path().join("ty.config.toml");
        std::fs::write(&config_file, b"# config").expect("write config");

        let mut state = PrecheckState::new();

        // 存在的配置文件 → Ok，不记入缺失集合。
        assert_eq!(state.check_config_reachable(&config_file), Ok(()));
        assert!(!state.is_known_missing(&config_file));

        // 同目录下不存在的配置文件 → config_missing 错误并记忆。
        let missing = dir.path().join("absent.toml");
        let err = state
            .check_config_reachable(&missing)
            .expect_err("missing config must error");
        assert!(
            err.starts_with(CONFIG_MISSING_PREFIX),
            "expected config_missing prefix, got: {err}"
        );
        assert!(state.is_known_missing(&missing));
    }

    proptest! {
        #![proptest_config(ProptestConfig { cases: 100, ..ProptestConfig::default() })]

        // Feature: execution-performance-optimization, Property 24: 已知只读/缺失路径短路重复探查
        //
        // 对任意路径与任意次数的重复探查：一旦某路径被判定为只读（或配置缺失）
        // 并记入 per-turn 已知集合，对同一路径的后续探查都返回一致的
        // `readonly_path:<path>`（或 `config_missing:<path>`）错误，且**不再触发
        // 实际文件系统探测**（注入的 probe 闭包不被再次调用）。
        // Validates: Requirements 8.2, 8.4
        #[test]
        fn prop24_known_path_short_circuits_repeat_probes(
            seg in "[a-zA-Z0-9_.-]{1,16}",
            repeats in 2usize..=8,
        ) {
            let path = PathBuf::from(format!("/{seg}"));

            // ---- 只读分支（R8.2）----
            {
                let mut state = PrecheckState::new();
                let calls = Cell::new(0usize);

                // 首次探查：probe 被调用一次，判定只读并记入已知集合。
                let first = state.check_writable_with(&path, |_| {
                    calls.set(calls.get() + 1);
                    false
                });
                prop_assert_eq!(first.clone(), Err(readonly_error(&path)));
                prop_assert_eq!(calls.get(), 1);
                prop_assert!(state.is_known_readonly(&path));

                // 后续重复探查：缓存短路，probe 绝不被调用，错误保持一致。
                for _ in 0..repeats {
                    let again = state.check_writable_with(&path, |_| {
                        panic!("probe must not be called on cached readonly path");
                    });
                    prop_assert_eq!(again, Err(readonly_error(&path)));
                    prop_assert_eq!(calls.get(), 1, "no further FS probe on cached readonly path");
                }
            }

            // ---- 配置缺失分支（R8.4）----
            {
                let mut state = PrecheckState::new();
                let calls = Cell::new(0usize);

                let first = state.check_config_reachable_with(&path, |_| {
                    calls.set(calls.get() + 1);
                    false
                });
                prop_assert_eq!(first.clone(), Err(config_missing_error(&path)));
                prop_assert_eq!(calls.get(), 1);
                prop_assert!(state.is_known_missing(&path));

                for _ in 0..repeats {
                    let again = state.check_config_reachable_with(&path, |_| {
                        panic!("probe must not be called on cached missing path");
                    });
                    prop_assert_eq!(again, Err(config_missing_error(&path)));
                    prop_assert_eq!(calls.get(), 1, "no further FS probe on cached missing path");
                }
            }
        }
    }
}
