//! 上下文构建层 —— 系统提示词和消息列表的组装。
//!
//! 负责将 workspace 文件、技能、案例等素材拼接为 LLM 可消费的消息列表。
//! 主控 Orchestrator 和多模型子 agent 共用同一套构建逻辑。

/// 上下文构建器 —— 系统提示词分段拼接、消息列表组装
pub mod context;

/// 全局提示词存储 —— 从 config/prompts.yaml 统一加载
pub mod prompt_store;

/// Nudge 提示词加载器（基于 prompt_store）
pub mod nudge_loader;

// 重新导出核心类型
pub use context::{
    is_user_context_message, strip_non_task_user_message, ContextBuilder, PlannedPromptContext,
    PromptContextEntry, PromptInputs, PromptMode, PromptSection, SkillContent,
    RUNTIME_CONTEXT_TAG, SYSTEM_CONTEXT_TAG, USER_CONTEXT_TAG,
};
