use async_trait::async_trait;
use std::sync::Arc;
use std::time::Instant;
use tracing::warn;
use tyclaw_tool_abi::{
    AllowAllGate, GateAction, GatePolicy, Sandbox, ToolExecutionResult, ToolParams,
};

use crate::base::Tool;

tokio::task_local! {
    /// 当前请求的用户角色（per-request，通过 .scope() 注入）。
    pub static CURRENT_USER_ROLE: String;
}

/// 获取当前上下文的用户角色，默认 "admin"。
pub fn current_user_role() -> String {
    CURRENT_USER_ROLE
        .try_with(|r| r.clone())
        .unwrap_or_else(|_| "admin".into())
}

#[async_trait]
pub trait ToolExecutor: Send + Sync {
    async fn execute(&self, tool: &dyn Tool, name: &str, params: ToolParams)
        -> ToolExecutionResult;
}

/// 直接在 host 执行，不走沙箱，不做门禁。
pub struct DirectToolExecutor;

#[async_trait]
impl ToolExecutor for DirectToolExecutor {
    async fn execute(
        &self,
        tool: &dyn Tool,
        _name: &str,
        params: ToolParams,
    ) -> ToolExecutionResult {
        let started = Instant::now();
        ToolExecutionResult {
            output: tool.execute(params).await,
            route: "host".into(),
            status: "ok".into(),
            duration_ms: started.elapsed().as_millis() as u64,
            gate_action: "allow".into(),
            risk_level: tool.risk_level().to_string(),
            sandbox_id: None,
        }
    }
}

/// 完整执行器：门禁检查 + 沙箱路由。
pub struct FullToolExecutor {
    gate: Arc<dyn GatePolicy>,
    sandbox_provider: Option<fn() -> Option<Arc<dyn Sandbox>>>,
}

impl FullToolExecutor {
    pub fn new(
        gate: Arc<dyn GatePolicy>,
        sandbox_provider: Option<fn() -> Option<Arc<dyn Sandbox>>>,
    ) -> Self {
        Self {
            gate,
            sandbox_provider,
        }
    }

    fn get_sandbox(&self) -> Option<Arc<dyn Sandbox>> {
        self.sandbox_provider.and_then(|p| p())
    }
}

impl Default for FullToolExecutor {
    fn default() -> Self {
        Self {
            gate: Arc::new(AllowAllGate),
            sandbox_provider: None,
        }
    }
}

#[async_trait]
impl ToolExecutor for FullToolExecutor {
    async fn execute(
        &self,
        tool: &dyn Tool,
        name: &str,
        params: ToolParams,
    ) -> ToolExecutionResult {
        // 门禁检查
        let risk_level = tool.risk_level().to_string();
        let user_role = current_user_role();
        let judgment = self.gate.judge(name, &risk_level, &user_role);
        let gate_action = match judgment.action {
            GateAction::Allow => "allow",
            GateAction::Deny => "deny",
            GateAction::Confirm => "confirm",
        };

        match judgment.action {
            GateAction::Deny => {
                warn!(tool = %name, reason = %judgment.reason, "Tool denied by gate");
                return ToolExecutionResult {
                    output: format!("[DENIED] {}", judgment.reason),
                    route: "gate".into(),
                    status: "denied".into(),
                    duration_ms: 0,
                    gate_action: gate_action.into(),
                    risk_level,
                    sandbox_id: None,
                };
            }
            GateAction::Allow | GateAction::Confirm => {}
        }

        // 沙箱路由
        if tool.should_sandbox() {
            if let Some(sandbox) = self.get_sandbox() {
                let sandbox_id = sandbox.id().to_string();
                let started = Instant::now();
                return ToolExecutionResult {
                    output: tool.execute_in_sandbox(sandbox.as_ref(), params).await,
                    route: "sandbox".into(),
                    status: "ok".into(),
                    duration_ms: started.elapsed().as_millis() as u64,
                    gate_action: gate_action.into(),
                    risk_level,
                    sandbox_id: Some(sandbox_id),
                };
            }
        }
        let started = Instant::now();
        ToolExecutionResult {
            output: tool.execute(params).await,
            route: "host".into(),
            status: "ok".into(),
            duration_ms: started.elapsed().as_millis() as u64,
            gate_action: gate_action.into(),
            risk_level,
            sandbox_id: None,
        }
    }
}

// ── 向后兼容别名 ──

/// 向后兼容：SandboxAwareToolExecutor = FullToolExecutor（默认 AllowAll gate）
pub type SandboxAwareToolExecutor = FullToolExecutor;
