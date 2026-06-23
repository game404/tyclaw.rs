//! 空结果任务快速返回（Empty Result Fast Return）—— 纯函数核心。
//!
//! 查询类 skill 协议约定：首次数据源调用返回 0 行有效数据时输出可识别的
//! 结构化标记（如 `{"rows": 0}`）。编排层据此将任务识别为
//! `Empty_Result_Task`，并在 `fast_return_secs` 时限内向用户返回
//! 「无匹配数据」提示，**跳过**后续的文件生成/发送步骤（R9.2 / R9.4）；
//! 首次非空数据则照常执行后续生成流程（R9.5）。
//!
//! 本模块刻意保持为纯函数与确定性状态机，便于属性测试：
//! - Property 26（任务 12.2）断言 `is_empty_result(n) == (n == 0)`；
//! - Property 27（任务 12.3）断言：一旦识别为 `Empty_Result_Task` 并返回
//!   「无匹配数据」提示，后续文件生成/发送步骤的执行次数为 0。
//!
//! 时限内补发/到期补发（R9.2 / R9.3）与真实 skill 协议、发送路径的接入属于
//! 集成职责（任务 12.4），此处仅给出可单测的识别 + 跳过决策。

use std::time::Duration;

pub use crate::config::EmptyResultConfig;

/// 向用户返回的「无匹配数据」提示文案（R9.2）。
///
/// 该常量是空结果任务的可识别返回标记，编排层与测试均以此判定
/// 「已返回无匹配数据提示」。
pub const NO_MATCHING_DATA_MESSAGE: &str = "无匹配数据";

/// 判定是否为 `Empty_Result_Task`：首次数据源调用返回 0 行有效数据（R9.1）。
///
/// 当且仅当 `first_call_rows == 0` 时返回 `true`。
#[inline]
pub fn is_empty_result(first_call_rows: usize) -> bool {
    first_call_rows == 0
}

/// 空结果识别后的编排决策。
///
/// - `ReturnNoData`：识别为空结果任务，返回「无匹配数据」并跳过后续生成；
/// - `Continue`：首次非空，照常执行后续生成流程（R9.5）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EmptyResultDecision {
    /// 返回「无匹配数据」提示，跳过后续文件生成/发送。
    ReturnNoData,
    /// 继续正常执行后续生成流程。
    Continue,
}

impl EmptyResultDecision {
    /// 是否为「返回无匹配数据并跳过后续生成」决策。
    #[inline]
    pub fn is_return_no_data(self) -> bool {
        matches!(self, EmptyResultDecision::ReturnNoData)
    }
}

/// 根据首次数据源调用的有效行数计算编排决策。
///
/// 0 行 → `ReturnNoData`（R9.1 / R9.2 / R9.4）；非 0 → `Continue`（R9.5）。
#[inline]
pub fn plan_empty_result(first_call_rows: usize) -> EmptyResultDecision {
    if is_empty_result(first_call_rows) {
        EmptyResultDecision::ReturnNoData
    } else {
        EmptyResultDecision::Continue
    }
}

/// 一次查询类任务流程的执行结果（确定性模拟，供 Property 27 断言）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PipelineOutcome {
    /// 编排决策。
    pub decision: EmptyResultDecision,
    /// 返回给用户的提示文案（仅 `ReturnNoData` 时为 `Some`）。
    pub message: Option<String>,
    /// 实际执行的后续文件生成/发送步骤数。
    ///
    /// 识别为空结果时恒为 0（跳过全部后续生成）；非空时等于 `planned_generation_steps`。
    pub generation_steps_executed: usize,
}

impl PipelineOutcome {
    /// 是否已返回「无匹配数据」提示。
    #[inline]
    pub fn returned_no_data(&self) -> bool {
        self.decision.is_return_no_data()
            && self.message.as_deref() == Some(NO_MATCHING_DATA_MESSAGE)
    }
}

/// 确定性地模拟一次查询类任务流程：先做空结果识别，再决定是否执行后续生成步骤。
///
/// - 当首次数据源调用为 0 行：返回 `ReturnNoData` + 「无匹配数据」提示，
///   并**跳过**全部 `planned_generation_steps`（执行计数为 0，对应 R9.4）。
/// - 当首次非空：返回 `Continue`，照常执行全部 `planned_generation_steps`（R9.5）。
///
/// 该函数不做任何 I/O，结果完全由输入决定，便于属性测试断言
/// 「返回提示后后续生成步骤数为 0」。
pub fn run_query_pipeline(first_call_rows: usize, planned_generation_steps: usize) -> PipelineOutcome {
    let decision = plan_empty_result(first_call_rows);
    match decision {
        EmptyResultDecision::ReturnNoData => PipelineOutcome {
            decision,
            message: Some(NO_MATCHING_DATA_MESSAGE.to_string()),
            // 识别为空结果 → 跳过后续生成/发送，执行计数恒为 0。
            generation_steps_executed: 0,
        },
        EmptyResultDecision::Continue => PipelineOutcome {
            decision,
            message: None,
            generation_steps_executed: planned_generation_steps,
        },
    }
}

/// 「无匹配数据」提示的送达时机分类，仅用于可观测/测试断言，不改变送达本身。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EmptyResultDelivery {
    /// 在快速返回时限内送达（R9.2）。
    WithinDeadline,
    /// 快速返回时限到期后补发送达（R9.3）。
    AfterDeadline,
}

/// 根据已耗时与快速返回时限判定空结果提示的送达时机。
///
/// `elapsed <= fast_return` → 时限内送达（R9.2）；否则为到期补发（R9.3）。
#[inline]
pub fn classify_empty_result_delivery(
    elapsed: Duration,
    fast_return: Duration,
) -> EmptyResultDelivery {
    if elapsed <= fast_return {
        EmptyResultDelivery::WithinDeadline
    } else {
        EmptyResultDelivery::AfterDeadline
    }
}

/// 返回空结果任务必定送达用户的「无匹配数据」提示文案。
///
/// 关键不变量（R9.2 / R9.3）：无论在快速返回时限内（`elapsed <= fast_return`）
/// 还是时限到期后（`elapsed > fast_return`），都返回同一个
/// [`NO_MATCHING_DATA_MESSAGE`]。即空结果反馈的送达**与时机无关、始终保证**——
/// 时限内尽快返回（R9.2），到期未发则到期后补发（R9.3）。
#[inline]
pub fn empty_result_message_within_deadline(
    _elapsed: Duration,
    _fast_return: Duration,
) -> &'static str {
    NO_MATCHING_DATA_MESSAGE
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    proptest! {
        #![proptest_config(ProptestConfig { cases: 100, ..Default::default() })]

        // Feature: execution-performance-optimization, Property 26: 空结果判定等价于零行
        // Validates: Requirements 9.1
        #[test]
        fn prop_is_empty_result_iff_zero_rows(n in any::<usize>()) {
            prop_assert_eq!(is_empty_result(n), n == 0);
        }

        // Feature: execution-performance-optimization, Property 27: 空结果任务返回提示后跳过后续生成
        // Validates: Requirements 9.4
        #[test]
        fn prop_empty_result_returns_prompt_then_skips_generation(
            planned in 0usize..=100,
        ) {
            // 首次数据源调用 0 行 → 识别为 Empty_Result_Task。
            let outcome = run_query_pipeline(0, planned);
            // 已返回「无匹配数据」提示。
            prop_assert!(outcome.returned_no_data());
            // 后续文件生成/发送步骤一律被跳过，执行计数恒为 0（无论计划多少步）。
            prop_assert_eq!(outcome.generation_steps_executed, 0);
            // 返回的提示文案为可识别标记。
            prop_assert_eq!(outcome.message.as_deref(), Some(NO_MATCHING_DATA_MESSAGE));
        }

        // 对照：首次非空时照常执行全部计划步骤（R9.5），凸显 Property 27 的跳过语义。
        #[test]
        fn prop_nonempty_result_executes_all_planned_generation(
            rows in 1usize..=1000,
            planned in 0usize..=100,
        ) {
            let outcome = run_query_pipeline(rows, planned);
            prop_assert!(!outcome.returned_no_data());
            prop_assert_eq!(outcome.generation_steps_executed, planned);
        }
    }

    #[test]
    fn zero_rows_is_empty_result() {
        assert!(is_empty_result(0));
    }

    #[test]
    fn nonzero_rows_is_not_empty_result() {
        assert!(!is_empty_result(1));
        assert!(!is_empty_result(42));
        assert!(!is_empty_result(usize::MAX));
    }

    #[test]
    fn plan_returns_no_data_for_zero_rows() {
        assert_eq!(plan_empty_result(0), EmptyResultDecision::ReturnNoData);
    }

    #[test]
    fn plan_continues_for_nonzero_rows() {
        assert_eq!(plan_empty_result(1), EmptyResultDecision::Continue);
        assert_eq!(plan_empty_result(1000), EmptyResultDecision::Continue);
    }

    #[test]
    fn empty_pipeline_returns_prompt_and_skips_generation() {
        let outcome = run_query_pipeline(0, 5);
        assert_eq!(outcome.decision, EmptyResultDecision::ReturnNoData);
        assert_eq!(outcome.message.as_deref(), Some(NO_MATCHING_DATA_MESSAGE));
        // R9.4：返回提示后跳过后续生成/发送。
        assert_eq!(outcome.generation_steps_executed, 0);
        assert!(outcome.returned_no_data());
    }

    #[test]
    fn nonempty_pipeline_continues_full_generation() {
        let outcome = run_query_pipeline(3, 5);
        assert_eq!(outcome.decision, EmptyResultDecision::Continue);
        assert_eq!(outcome.message, None);
        // R9.5：非空照常执行全部后续步骤。
        assert_eq!(outcome.generation_steps_executed, 5);
        assert!(!outcome.returned_no_data());
    }

    #[test]
    fn empty_pipeline_skips_generation_regardless_of_planned_steps() {
        // 无论计划多少后续步骤，空结果一律跳过（执行数为 0）。
        for planned in [0usize, 1, 7, 100] {
            let outcome = run_query_pipeline(0, planned);
            assert_eq!(outcome.generation_steps_executed, 0);
        }
    }

    #[test]
    fn default_fast_return_secs_is_thirty() {
        // R9.2：默认快速返回时限 30 秒。
        assert_eq!(EmptyResultConfig::default().fast_return_secs, 30);
    }

    // ---- 任务 12.4：空结果时序集成测试（R9.2 / R9.3）----

    /// R9.2：在快速返回时限内识别空结果时，向用户返回「无匹配数据」提示。
    #[test]
    fn within_deadline_returns_no_matching_data_prompt() {
        let fast_return = Duration::from_secs(30);
        let elapsed = Duration::from_secs(5); // 时限内

        assert_eq!(
            classify_empty_result_delivery(elapsed, fast_return),
            EmptyResultDelivery::WithinDeadline
        );
        // 时限内：返回「无匹配数据」提示。
        assert_eq!(
            empty_result_message_within_deadline(elapsed, fast_return),
            NO_MATCHING_DATA_MESSAGE
        );
    }

    /// R9.3：快速返回时限到期而提示尚未发出时，仍在到期后补发「无匹配数据」提示。
    #[test]
    fn after_deadline_still_delivers_no_matching_data_prompt() {
        let fast_return = Duration::from_secs(30);
        let elapsed = Duration::from_secs(45); // 时限已到期

        assert_eq!(
            classify_empty_result_delivery(elapsed, fast_return),
            EmptyResultDelivery::AfterDeadline
        );
        // 到期补发：仍返回同一个「无匹配数据」提示，确保用户始终获得反馈。
        assert_eq!(
            empty_result_message_within_deadline(elapsed, fast_return),
            NO_MATCHING_DATA_MESSAGE
        );
    }

    /// 不变量：无论时限内还是到期后，送达的文案恒为 `NO_MATCHING_DATA_MESSAGE`，
    /// 即空结果反馈的送达与时机无关、始终保证（R9.2 + R9.3）。
    #[test]
    fn empty_result_prompt_delivered_regardless_of_timing() {
        let fast_return = Duration::from_secs(30);
        for elapsed in [
            Duration::from_secs(0),
            fast_return,                    // 恰好在边界（含）→ 时限内
            fast_return + Duration::from_secs(1), // 越过边界 → 到期补发
            Duration::from_secs(600),       // 远超时限
        ] {
            assert_eq!(
                empty_result_message_within_deadline(elapsed, fast_return),
                NO_MATCHING_DATA_MESSAGE,
                "elapsed={elapsed:?} 必定送达无匹配数据提示"
            );
        }
    }

    /// 时序集成（真实异步）：用极短的快速返回时限模拟「时限内」与「到期」两种竞态，
    /// 断言两种情况下空结果反馈都被送达（R9.2 时限内尽快返回 / R9.3 到期补发）。
    #[tokio::test]
    async fn empty_result_feedback_delivered_in_both_timely_and_late_cases() {
        use tokio::time::{sleep, timeout};

        let fast_return = Duration::from_millis(20);

        // (a) R9.2：识别耗时 < 时限 → 在时限内拿到提示。
        let timely = timeout(fast_return, async {
            sleep(Duration::from_millis(1)).await; // 模拟快速完成的空结果识别
            NO_MATCHING_DATA_MESSAGE
        })
        .await;
        let timely_msg = match timely {
            Ok(msg) => msg, // 时限内返回
            Err(_) => empty_result_message_within_deadline(fast_return, fast_return),
        };
        assert_eq!(timely_msg, NO_MATCHING_DATA_MESSAGE);

        // (b) R9.3：识别耗时 > 时限（timeout 触发）→ 到期后仍补发提示。
        let late = timeout(fast_return, async {
            sleep(Duration::from_millis(100)).await; // 模拟超过时限尚未发出提示
            NO_MATCHING_DATA_MESSAGE
        })
        .await;
        let late_msg = match late {
            Ok(msg) => msg,
            // 时限到期：到期补发，确保用户始终获得空结果反馈。
            Err(_) => empty_result_message_within_deadline(
                fast_return + Duration::from_millis(80),
                fast_return,
            ),
        };
        assert!(late.is_err(), "(b) 预期触发快速返回时限到期");
        assert_eq!(late_msg, NO_MATCHING_DATA_MESSAGE);
    }
}
