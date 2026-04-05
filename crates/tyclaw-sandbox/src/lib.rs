//! 沙箱执行环境 —— 为 Task Session 提供隔离的工具执行能力。
//!
//! 核心抽象（trait 定义在 tyclaw-tool-abi）：
//! - `Sandbox` trait：单个沙箱实例的工具执行接口
//! - `SandboxPool` trait：沙箱池管理（acquire/release）
//!
//! 本 crate 提供具体实现：
//! - `DockerSandbox`：基于 Docker 容器的实现
//! - `NoopSandbox`：无隔离，直接在 host 执行（调试/fallback）

pub mod docker;
pub mod noop;
pub mod types;

pub use docker::{DockerConfig, DockerPool, DockerSandbox};
pub use noop::{NoopPool, NoopSandbox};
pub use types::*;

use std::sync::Arc;

tokio::task_local! {
    /// 当前请求关联的 Sandbox 实例（per-request，通过 .scope() 注入）。
    pub static CURRENT_SANDBOX: Arc<dyn tyclaw_tool_abi::Sandbox>;
}

/// 检查当前上下文是否有关联的 Sandbox。
pub fn current_sandbox() -> Option<Arc<dyn tyclaw_tool_abi::Sandbox>> {
    CURRENT_SANDBOX.try_with(|s| s.clone()).ok()
}
