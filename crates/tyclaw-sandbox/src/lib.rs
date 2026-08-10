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

pub use docker::{sanitize_container_name, DockerConfig, DockerPool, DockerSandbox};
pub use noop::{NoopPool, NoopSandbox};
pub use types::*;

use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use tyclaw_types::TyclawError;

pub(crate) fn validate_workspace_relative_path(path: &str) -> Result<PathBuf, TyclawError> {
    let path = Path::new(path);
    if path.as_os_str().is_empty() || path.is_absolute() {
        return Err(workspace_read_error(
            "Path must be relative to the workspace work directory",
        ));
    }

    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Normal(part) => normalized.push(part),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(workspace_read_error(
                    "Path must stay within the workspace work directory",
                ));
            }
        }
    }
    if normalized.as_os_str().is_empty() {
        return Err(workspace_read_error("Path must identify a workspace file"));
    }
    Ok(normalized)
}

pub(crate) fn workspace_read_error(message: impl Into<String>) -> TyclawError {
    TyclawError::Tool {
        tool: "sandbox_read_workspace".into(),
        message: message.into(),
    }
}

tokio::task_local! {
    /// 当前请求关联的 Sandbox 实例（per-request，通过 .scope() 注入）。
    pub static CURRENT_SANDBOX: Arc<dyn tyclaw_tool_abi::Sandbox>;
}

/// 检查当前上下文是否有关联的 Sandbox。
pub fn current_sandbox() -> Option<Arc<dyn tyclaw_tool_abi::Sandbox>> {
    CURRENT_SANDBOX.try_with(|s| s.clone()).ok()
}
