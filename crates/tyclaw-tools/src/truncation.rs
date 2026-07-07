//! 工具输出截断限制配置（R5）。
//!
//! tyclaw-tools 是底层 crate，不依赖 `tyclaw-orchestration::config`。
//! 因此截断上限以本 crate 内的运行期单例承载，启动时由上层（任务 19.1）
//! 通过 [`init_truncation_limits`] 注入 `PerformanceConfig.truncation`；
//! 未注入时惰性初始化为需求默认值（20000/20000/0.25）。
//!
//! 该机制镜像 `tyclaw-provider::openai_compat` 的 `SseConfig`
//! （`OnceLock<RwLock<_>>` + `init_*` + `current_*` 快照）模式，便于运行期调参。

use std::sync::{OnceLock, RwLock};

/// 截断上限的可配置下限：任何配置值都不得低于此值（R5.1 / R5.2）。
pub const TRUNCATE_FLOOR_CHARS: usize = 8_000;

/// exec / grep_search 截断上限默认值（R5.1 / R5.2）。
pub const DEFAULT_TRUNCATE_CHARS: usize = 20_000;

/// 头尾双段截断的尾段比例默认值（R5.3：尾段 >= 上限的 25%）。
pub const DEFAULT_TAIL_RATIO: f64 = 0.25;

/// 工具输出截断限制（R5）。
///
/// - `exec_truncate_chars`：exec 工具输出截断上限（默认 20000，下限 8000）。
/// - `grep_truncate_chars`：grep_search 工具输出截断上限（默认 20000，下限 8000）。
/// - `tail_ratio`：头尾双段截断中尾段所占比例（默认 0.25）。
///
/// 注意：`read_file` 的 `MAX_READ_CHARS = 128_000` 不受此配置影响（R5.5）。
#[derive(Debug, Clone, PartialEq)]
pub struct TruncationLimits {
    /// exec 工具输出截断上限（字符）。
    pub exec_truncate_chars: usize,
    /// grep_search 工具输出截断上限（字符）。
    pub grep_truncate_chars: usize,
    /// 头尾双段截断的尾段比例。
    pub tail_ratio: f64,
}

impl Default for TruncationLimits {
    fn default() -> Self {
        Self {
            exec_truncate_chars: DEFAULT_TRUNCATE_CHARS,
            grep_truncate_chars: DEFAULT_TRUNCATE_CHARS,
            tail_ratio: DEFAULT_TAIL_RATIO,
        }
    }
}

impl TruncationLimits {
    /// 对越界字段做 clamp：
    /// - 截断上限强制 `>= TRUNCATE_FLOOR_CHARS`（8000，R5.1 / R5.2）。
    /// - `tail_ratio` 强制落入 `[0.0, 1.0]`，非有限值回退默认 0.25。
    #[must_use]
    pub fn sanitized(self) -> Self {
        Self {
            exec_truncate_chars: self.exec_truncate_chars.max(TRUNCATE_FLOOR_CHARS),
            grep_truncate_chars: self.grep_truncate_chars.max(TRUNCATE_FLOOR_CHARS),
            tail_ratio: if self.tail_ratio.is_finite() {
                self.tail_ratio.clamp(0.0, 1.0)
            } else {
                DEFAULT_TAIL_RATIO
            },
        }
    }
}

/// 运行期截断配置单例。上层启动时通过 [`init_truncation_limits`] 注入，
/// 未注入时惰性初始化为 [`TruncationLimits::default`]，保持向后兼容（20000/20000/0.25）。
static TRUNCATION_LIMITS: OnceLock<RwLock<TruncationLimits>> = OnceLock::new();

fn truncation_limits_cell() -> &'static RwLock<TruncationLimits> {
    TRUNCATION_LIMITS.get_or_init(|| RwLock::new(TruncationLimits::default()))
}

/// 注入运行期截断配置（启动时由上层调用一次）。
///
/// 供任务 19.1 将 `PerformanceConfig.truncation` 注入工具层。
/// 越界字段会被 [`TruncationLimits::sanitized`] 自动 clamp（下限 8000）。
pub fn init_truncation_limits(limits: TruncationLimits) {
    *truncation_limits_cell()
        .write()
        .unwrap_or_else(|e| e.into_inner()) = limits.sanitized();
}

/// 读取当前截断配置快照。
pub fn current_truncation_limits() -> TruncationLimits {
    truncation_limits_cell()
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 默认值即需求默认值（20000/20000/0.25）。
    #[test]
    fn test_default_limits() {
        let l = TruncationLimits::default();
        assert_eq!(l.exec_truncate_chars, 20_000);
        assert_eq!(l.grep_truncate_chars, 20_000);
        assert_eq!(l.tail_ratio, 0.25);
    }

    /// 越界配置被 clamp 到下限 8000（R5.1 / R5.2）。
    #[test]
    fn test_sanitized_clamps_floor() {
        let l = TruncationLimits {
            exec_truncate_chars: 100,
            grep_truncate_chars: 0,
            tail_ratio: 0.25,
        }
        .sanitized();
        assert_eq!(l.exec_truncate_chars, TRUNCATE_FLOOR_CHARS);
        assert_eq!(l.grep_truncate_chars, TRUNCATE_FLOOR_CHARS);
    }

    /// tail_ratio 被 clamp 到 [0,1]，非有限值回退默认。
    #[test]
    fn test_sanitized_clamps_ratio() {
        assert_eq!(
            TruncationLimits {
                exec_truncate_chars: 20_000,
                grep_truncate_chars: 20_000,
                tail_ratio: 2.0,
            }
            .sanitized()
            .tail_ratio,
            1.0
        );
        assert_eq!(
            TruncationLimits {
                exec_truncate_chars: 20_000,
                grep_truncate_chars: 20_000,
                tail_ratio: f64::NAN,
            }
            .sanitized()
            .tail_ratio,
            DEFAULT_TAIL_RATIO
        );
    }

    /// init 后 current 返回 sanitized 后的快照。
    #[test]
    fn test_init_and_current_roundtrip() {
        init_truncation_limits(TruncationLimits {
            exec_truncate_chars: 30_000,
            grep_truncate_chars: 50,
            tail_ratio: 0.3,
        });
        let cur = current_truncation_limits();
        assert_eq!(cur.exec_truncate_chars, 30_000);
        // 越界值被 clamp 到下限
        assert_eq!(cur.grep_truncate_chars, TRUNCATE_FLOOR_CHARS);
        assert_eq!(cur.tail_ratio, 0.3);
        // 还原默认，避免污染其他用例（单例进程内共享）
        init_truncation_limits(TruncationLimits::default());
    }
}
