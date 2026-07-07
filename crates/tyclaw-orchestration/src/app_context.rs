//! 不可变的应用级上下文，通过 Arc 在所有层之间共享。

use std::path::PathBuf;
use std::sync::Arc;

use crate::config::PerformanceConfig;
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
    /// 统一性能治理配置（污染过滤 / 会话规模 / 截断 / 并发 / 超时 等）。
    /// 编排请求生命周期的治理关卡（污染剔除 / 配对修复 / 规模上限）据此读取阈值。
    pub performance: PerformanceConfig,
}

impl AppContext {
    pub fn new(
        workspace: PathBuf,
        model: String,
        write_snapshot: bool,
        context_window_tokens: usize,
        features: OrchestratorFeatures,
        performance: PerformanceConfig,
    ) -> Arc<Self> {
        Arc::new(Self {
            workspace,
            model,
            write_snapshot,
            context_window_tokens,
            features,
            performance,
        })
    }
}
