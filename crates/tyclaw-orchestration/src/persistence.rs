//! 有状态的持久化服务层，Orchestrator 独占。

use tyclaw_control::{AuditLog, RateLimiter, WorkspaceManager};
use tyclaw_memory::CaseStore;

use crate::session_manager::SessionManager;
use crate::skill_manager::SkillManager;

/// 有状态的持久化服务聚合，Orchestrator 独占使用。
pub struct PersistenceLayer {
    pub workspace_mgr: WorkspaceManager,
    pub audit: AuditLog,
    pub case_store: CaseStore,
    pub sessions: SessionManager,
    pub skills: SkillManager,
    pub rate_limiter: RateLimiter,
}
