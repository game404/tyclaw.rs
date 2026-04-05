//! TyClaw V2 的共享类型、错误定义和常量模块。
//!
//! 本 crate 是整个项目的基础类型层，被所有其他 crate 依赖。
//! 提供统一的消息格式、错误类型和全局常量。

/// 错误类型模块 —— 定义了 TyclawError 统一错误枚举
pub mod error;

/// 消息类型模块 —— OpenAI 兼容的消息格式、角色和风险等级
pub mod message;

/// 常量模块 —— 默认模型、上下文窗口大小、速率限制等全局常量
pub mod constants;

/// Token 估算模块 —— 使用 tiktoken 估算消息的 token 数量
pub mod tokens;

/// JSON 修复模块 —— 修复 LLM 输出中常见的 JSON 格式错误
pub mod json_repair;

// 重新导出核心类型，方便外部 crate 直接使用
pub use error::TyclawError;
pub use message::Message;
