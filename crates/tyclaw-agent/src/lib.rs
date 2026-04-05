//! Agent 运行时层：ReAct 循环引擎。
//!
//! 本 crate 是 TyClaw 的核心执行引擎，实现了：
//! - `AgentRuntime` trait：Agent 执行引擎的统一接口
//! - `AgentLoop`：ReAct（Reasoning + Acting）循环的具体实现

/// 运行时抽象模块 —— AgentRuntime trait 和 RuntimeResult
pub mod runtime;

/// 结构化上下文状态模块 —— events/state/prompt-view
pub mod context_state;

/// ReAct 循环辅助函数与常量
pub(crate) mod loop_helpers;

/// 工具结果与参数的历史压缩
pub(crate) mod compression;

/// Agent 内部子模块（迭代预算、工具执行等）
pub(crate) mod agent;

/// ReAct 循环模块 —— 迭代式 LLM 调用 + 工具执行引擎
pub mod agent_loop;

// 重新导出核心类型
pub use agent_loop::AgentLoop;
pub use runtime::{
    chat_message, parse_thinking_prefix, AgentRuntime, RuntimeResult, RuntimeStatus,
};

// 从 tyclaw-context 重新导出（向后兼容）
pub use tyclaw_prompt::{
    nudge_loader, prompt_store, strip_non_task_user_message, ContextBuilder, PlannedPromptContext,
    PromptContextEntry, PromptInputs, PromptMode, PromptSection, SkillContent,
    USER_CONTEXT_TAG,
};
