//! 单次用户请求内的迭代上限（与 `AgentLoop::max_iterations` 对齐）。
//!
//! ## 语义（避免与「全局 2×」混淆）
//! - **`max_per_phase`**：配置项 `max_iterations`，表示**探索阶段**与**产出阶段**各自允许的 LLM 轮次上限（分阶段计数）。
//! - **`explore_max`**：探索子阶段实际上限 = `min(比例预算, 绝对上限)`。
//! - **`global_llm_cap()`**：同一用户请求内，LLM 调用总次数的**硬顶** = `2 * max_per_phase`（探索 + 产出各最多 `max_per_phase` 轮，与循环内两个 `break` 条件一致）。

use crate::loop_helpers::{EXPLORE_ABSOLUTE_CAP, EXPLORE_MAX_RATIO_PERCENT};

#[derive(Debug, Clone)]
pub(crate) struct IterationBudget {
    /// 每阶段（探索 / 产出）允许的最大轮次，等于 `AgentLoop.max_iterations`。
    pub max_per_phase: usize,
    /// 探索子阶段轮次上限。
    pub explore_max: usize,
}

impl IterationBudget {
    pub fn new(max_iterations: usize) -> Self {
        let explore_max =
            (max_iterations * EXPLORE_MAX_RATIO_PERCENT / 100).min(EXPLORE_ABSOLUTE_CAP);
        Self {
            max_per_phase: max_iterations,
            explore_max,
        }
    }

    /// 探索 + 产出合计的 LLM 轮次上限（与 `total_iterations > max_per_phase * 2` 一致）。
    #[inline]
    pub fn global_llm_cap(&self) -> usize {
        self.max_per_phase.saturating_mul(2)
    }
}
