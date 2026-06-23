//! 稳定前缀缓存指纹（Stable Prefix Cache Fingerprint）。
//!
//! 实现 Requirement 11.4：当稳定前缀的三元组指纹
//! `(identity, bootstrap files, MEMORY.md)` 未发生变化时，复用已构建的前缀，
//! 而不重复拼接（并在 Provider 侧复用现有 `cache_scope` / `cache_breakpoint_idx` 前缀）。
//!
//! 本模块只承载**纯函数**逻辑，便于属性测试（见 task 15.6 / Property 32）：
//! - [`PrefixFingerprint`]：三元组指纹值类型，派生 `PartialEq`，逐字段比较。
//! - [`compute_fingerprint`]：从三段内容计算指纹。
//! - [`prefix_cache_reusable`]：当且仅当三段全等时返回 `true`。
//!
//! 设计取舍：指纹直接保留三段内容的拥有副本（`String`），因此比较是
//! **精确**的，不存在哈希碰撞导致的误命中。这保证 Property 32 的「当且仅当」
//! 语义严格成立。

/// 稳定前缀的三元组指纹：`(identity, bootstrap files, MEMORY.md)`。
///
/// 三个字段分别捕获前缀缓存的三类输入：
/// - `identity`：Agent 身份段（含运行时环境）渲染后的文本。
/// - `bootstrap_files`：workspace 下所有 `*.md` 引导文件拼接后的文本。
/// - `memory_md`：`memory/MEMORY.md` 长期记忆段文本。
///
/// 派生 `PartialEq`/`Eq` 后，`==` 即为「三者全等」判定。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrefixFingerprint {
    /// 身份段内容指纹（精确内容副本）。
    pub identity: String,
    /// 引导文件段内容指纹（精确内容副本）。
    pub bootstrap_files: String,
    /// MEMORY.md 段内容指纹（精确内容副本）。
    pub memory_md: String,
}

impl PrefixFingerprint {
    /// 从三段内容构造指纹。
    pub fn new(
        identity: impl Into<String>,
        bootstrap_files: impl Into<String>,
        memory_md: impl Into<String>,
    ) -> Self {
        Self {
            identity: identity.into(),
            bootstrap_files: bootstrap_files.into(),
            memory_md: memory_md.into(),
        }
    }

    /// 判断当前指纹是否与 `other` 完全一致（三段全等）。
    ///
    /// 等价于 `self == other`，提供具名方法以表达意图。
    pub fn matches(&self, other: &PrefixFingerprint) -> bool {
        self == other
    }
}

/// 从三段内容计算稳定前缀指纹。
///
/// 这是 [`PrefixFingerprint::new`] 的自由函数形式，作为模块的公开入口。
pub fn compute_fingerprint(
    identity: &str,
    bootstrap_files: &str,
    memory_md: &str,
) -> PrefixFingerprint {
    PrefixFingerprint::new(identity, bootstrap_files, memory_md)
}

/// 判断缓存前缀是否可复用。
///
/// **当且仅当** `prev` 与 `curr` 的三段（identity / bootstrap files / MEMORY.md）
/// 全部相等时返回 `true`；任意一段不同即返回 `false`。这是 Property 32 针对的函数。
pub fn prefix_cache_reusable(prev: &PrefixFingerprint, curr: &PrefixFingerprint) -> bool {
    prev == curr
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    // 生成一个「小集合」字符串：固定字符串与短随机字符串混合，
    // 使两个三元组在各分量上有较高概率相等，从而让「全等」与「不等」两类
    // 分支都被充分覆盖（避免随机 String 几乎总是不等，使 IFF 退化为单边验证）。
    fn small_segment() -> impl Strategy<Value = String> {
        prop_oneof![
            Just("a".to_string()),
            Just("b".to_string()),
            Just(String::new()),
            "[a-z]{0,5}".prop_map(|s| s),
        ]
    }

    proptest! {
        #![proptest_config(ProptestConfig { cases: 100, ..ProptestConfig::default() })]

        // Feature: execution-performance-optimization, Property 32: 稳定前缀缓存命中当且仅当指纹全等
        #[test]
        fn prefix_cache_reusable_iff_all_three_equal(
            id1 in small_segment(),
            boot1 in small_segment(),
            mem1 in small_segment(),
            id2 in small_segment(),
            boot2 in small_segment(),
            mem2 in small_segment(),
        ) {
            let fp1 = compute_fingerprint(&id1, &boot1, &mem1);
            let fp2 = compute_fingerprint(&id2, &boot2, &mem2);

            let all_equal = id1 == id2 && boot1 == boot2 && mem1 == mem2;
            prop_assert_eq!(prefix_cache_reusable(&fp1, &fp2), all_equal);
        }
    }

    #[test]
    fn identical_triples_are_reusable() {
        let a = compute_fingerprint("id", "boot", "mem");
        let b = compute_fingerprint("id", "boot", "mem");
        assert!(prefix_cache_reusable(&a, &b));
        assert!(a.matches(&b));
    }

    #[test]
    fn differing_identity_breaks_reuse() {
        let a = compute_fingerprint("id-1", "boot", "mem");
        let b = compute_fingerprint("id-2", "boot", "mem");
        assert!(!prefix_cache_reusable(&a, &b));
    }

    #[test]
    fn differing_bootstrap_breaks_reuse() {
        let a = compute_fingerprint("id", "boot-1", "mem");
        let b = compute_fingerprint("id", "boot-2", "mem");
        assert!(!prefix_cache_reusable(&a, &b));
    }

    #[test]
    fn differing_memory_breaks_reuse() {
        let a = compute_fingerprint("id", "boot", "mem-1");
        let b = compute_fingerprint("id", "boot", "mem-2");
        assert!(!prefix_cache_reusable(&a, &b));
    }

    #[test]
    fn empty_segments_are_handled() {
        let a = compute_fingerprint("", "", "");
        let b = compute_fingerprint("", "", "");
        assert!(prefix_cache_reusable(&a, &b));
        let c = compute_fingerprint("", "", "x");
        assert!(!prefix_cache_reusable(&a, &c));
    }

    #[test]
    fn reuse_is_symmetric() {
        let a = compute_fingerprint("id", "boot", "mem");
        let b = compute_fingerprint("id", "boot", "mem");
        assert_eq!(
            prefix_cache_reusable(&a, &b),
            prefix_cache_reusable(&b, &a)
        );
    }
}
