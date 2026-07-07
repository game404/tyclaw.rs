//! 有状态的持久化服务层，Orchestrator 独占。

use std::sync::Arc;

use tyclaw_control::{AuditLog, RateLimiter, WorkspaceManager};
use tyclaw_memory::CaseStore;

use crate::session_manager::SessionManager;
use crate::skill_manager::SkillManager;

/// 有状态的持久化服务聚合，Orchestrator 独占使用。
pub struct PersistenceLayer {
    pub workspace_mgr: WorkspaceManager,
    /// 审计日志（Arc 以便移入 `spawn_blocking` 做非阻塞落盘）。
    pub audit: Arc<AuditLog>,
    pub case_store: CaseStore,
    pub sessions: SessionManager,
    pub skills: SkillManager,
    pub rate_limiter: RateLimiter,
}
