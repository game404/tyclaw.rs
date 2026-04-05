//! Agent 子模块：迭代预算、（后续可迁入）工具执行辅助等。
//!
//! ## 重构计划（阶段性）
//! 1. **已完成**：`iteration_budget` 明确「每阶段 / 全局」LLM 轮次上限语义。
//! 2. **已完成**：`exec` 重复检测改为全量命令指纹（避免仅前 800 字符碰撞）。
//! 3. **已完成**：工具按 `tool_calls` 顺序串行执行；`ask_user` 与其它工具同轮并存时不再跳过其它工具。
//! 4. **已完成**：`read_file` 在循环内增加软上限，避免单条消息撑爆上下文。
//! 5. **已完成**：`ContextState` 中 Goal 在 snapshot 内展示长度放宽；证据卡片总量收紧。
//! 6. **已完成**：阶段 focus / nudge → `phase.rs`；单轮工具执行 → `tool_runner.rs`。

pub(crate) mod iteration_budget;
pub(crate) mod phase;
pub(crate) mod tool_runner;
