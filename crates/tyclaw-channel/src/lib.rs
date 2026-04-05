//! 通道层：CLI 和其他输入输出适配器。
//!
//! 本 crate 负责与用户的交互界面，当前包含：
//! - CLI 交互通道
//! - DingTalk Stream 通道
//! 未来可继续扩展 WeChat / Feishu / HTTP API 等渠道。

/// CLI 交互通道 —— 基于 stdin/stdout 的交互式命令行
pub mod cli;
/// DingTalk 通道 —— WebSocket Stream + 机器人处理
pub mod dingtalk;

// 重新导出
pub use cli::CliChannel;
pub use dingtalk::{
    AckMessage, CallbackMessage, ChatbotHandler, ChatbotMessage, Credential, DingTalkBot,
    DingTalkStreamClient, GatewayClient,
};
