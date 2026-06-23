//! LLM 提供者层：定义了 LLM 调用的 trait 抽象、类型定义，
//! 以及 OpenAI 兼容的 HTTP 实现。
//!
//! 本 crate 将具体的 LLM API 调用细节封装起来，
//! 上层只需通过 `LLMProvider` trait 即可与任意 LLM 交互。

/// 类型定义模块 —— ToolCallRequest、LLMResponse、GenerationSettings、ChatRequest
pub mod types;

/// Provider trait 模块 —— 定义 LLM 调用接口和自动重试逻辑
pub mod provider;

/// 并发控制器 —— 全局 + 单用户两级并发闸门与排队超时（R6）
pub mod concurrency;

/// OpenAI 兼容 HTTP 实现 —— 支持 OpenAI、Anthropic（通过代理）、Azure 等
pub mod openai_compat;

/// Reasoning 结构化解析器 —— 解析 thinking/reasoning 内容为结构化块
pub mod reasoning;

// 重新导出核心类型
pub use concurrency::{
    acquire_permit, controller, init_concurrency_controller, ConcurrencyConfig,
    ConcurrencyController, ConcurrencyError, LimitKind, Permit,
};
pub use openai_compat::{
    current_sse_config, effective_chunk_timeout, init_sse_config, OpenAICompatProvider, SseConfig,
};
pub use provider::{
    build_retry_later_message, init_concurrency, retry_exhausted_response, LLMProvider,
    CURRENT_USER_ID, RETRY_LATER_MESSAGE,
};
pub use reasoning::{parse_reasoning, ParsedReasoning, ReasoningBlock};
pub use types::{ChatRequest, GenerationSettings, LLMResponse, ThinkingConfig, ToolCallRequest};
