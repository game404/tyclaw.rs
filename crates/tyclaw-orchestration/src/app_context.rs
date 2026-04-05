//! 不可变的应用级上下文，通过 Arc 在所有层之间共享。

use std::path::PathBuf;
use std::sync::Arc;

use crate::types::OrchestratorFeatures;

/// 不可变的应用级配置，通过 Arc 在所有层之间共享。
///
/// 设计准则：只放那些在整个进程生命周期内不会改变、
/// 且被 2 个以上组件读取的值。
#[derive(Debug, Clone)]
pub struct AppContext {
    /// 工作区根路径
    pub workspace: PathBuf,
    /// 当前使用的 LLM 模型名称
    pub model: String,
    /// 是否写 snapshot（调试用）
    pub write_snapshot: bool,
    /// 上下文窗口大小（token 数）
    pub context_window_tokens: usize,
    /// 功能开关（审计/记忆/RBAC/限流）
    pub features: OrchestratorFeatures,
}

impl AppContext {
    pub fn new(
        workspace: PathBuf,
        model: String,
        write_snapshot: bool,
        context_window_tokens: usize,
        features: OrchestratorFeatures,
    ) -> Arc<Self> {
        Arc::new(Self {
            workspace,
            model,
            write_snapshot,
            context_window_tokens,
            features,
        })
    }
}
