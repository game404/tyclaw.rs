//! 执行门禁（ExecutionGate）—— 判断工具调用是否应该被允许执行。
//!
//! 门禁是权限系统的核心组件，在每次工具调用前进行权限检查，
//! 根据工具的风险等级和用户角色做出判定。

use crate::rbac::RBACManager;

/// 门禁判定动作枚举。
///
/// - `Allow`: 允许执行
/// - `Deny`: 拒绝执行（权限不足）
/// - `Confirm`: 需要用户确认（危险操作）
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JudgmentAction {
    Allow,   // 允许
    Deny,    // 拒绝
    Confirm, // 需确认
}

/// 门禁判定结果 —— 包含判定动作和原因说明。
#[derive(Debug, Clone)]
pub struct Judgment {
    pub action: JudgmentAction, // 判定动作
    pub reason: String,         // 判定原因（用于日志和错误信息）
}

/// 执行门禁 —— 在工具调用前进行权限检查。
///
/// 内部持有 RBACManager 实例，用于查询角色权限。
pub struct ExecutionGate {
    rbac: RBACManager,
}

impl ExecutionGate {
    pub fn new() -> Self {
        Self {
            rbac: RBACManager::new(),
        }
    }

    /// 对工具调用进行权限判定。
    ///
    /// 判定规则（按优先级执行）：
    ///
    /// 规则1: 只读工具（risk_level="read"）始终允许
    ///   → 任何角色都可以执行读取操作
    ///
    /// 规则2: 角色权限不足时拒绝
    ///   → 检查 RBAC 中该角色是否有对应风险等级的权限
    ///   → 例如 Guest 不允许 Write 操作
    ///
    /// 规则3: 危险操作（risk_level="dangerous"）的特殊处理
    ///   → Admin 角色：返回 Confirm（需要用户确认）
    ///   → 其他角色：直接 Deny（即使有 Write 权限也不能执行危险操作）
    ///
    /// 规则4: 默认允许
    ///   → 通过了以上所有检查的工具调用，允许执行
    pub fn judge(
        &self,
        _tool_name: &str,
        _tool_args: &serde_json::Value,
        risk_level: &str,
        user_role: &str,
    ) -> Judgment {
        // 规则1: 只读工具始终允许
        if risk_level == "read" {
            return Judgment {
                action: JudgmentAction::Allow,
                reason: "Read-only tool".into(),
            };
        }

        // 规则2: 角色权限不足时拒绝
        if !self.rbac.is_risk_level_allowed(user_role, risk_level) {
            return Judgment {
                action: JudgmentAction::Deny,
                reason: format!("Role '{user_role}' not allowed for risk level '{risk_level}'"),
            };
        }

        // 规则3: 危险操作需要 Admin 确认
        if risk_level == "dangerous" {
            if self.rbac.is_role_at_least(user_role, "admin") {
                return Judgment {
                    action: JudgmentAction::Confirm,
                    reason: "Dangerous operation requires confirmation".into(),
                };
            } else {
                return Judgment {
                    action: JudgmentAction::Deny,
                    reason: format!("Role '{user_role}' cannot execute dangerous tools"),
                };
            }
        }

        // 规则4: 默认允许
        Judgment {
            action: JudgmentAction::Allow,
            reason: "Allowed".into(),
        }
    }
}

impl Default for ExecutionGate {
    fn default() -> Self {
        Self::new()
    }
}

/// 实现 tool-abi 的 GatePolicy trait，使 ExecutionGate 可注入到 ToolExecutor。
impl tyclaw_tool_abi::GatePolicy for ExecutionGate {
    fn judge(
        &self,
        tool_name: &str,
        risk_level: &str,
        user_role: &str,
    ) -> tyclaw_tool_abi::GateJudgment {
        let j = self.judge(tool_name, &serde_json::json!({}), risk_level, user_role);
        tyclaw_tool_abi::GateJudgment {
            action: match j.action {
                JudgmentAction::Allow => tyclaw_tool_abi::GateAction::Allow,
                JudgmentAction::Deny => tyclaw_tool_abi::GateAction::Deny,
                JudgmentAction::Confirm => tyclaw_tool_abi::GateAction::Confirm,
            },
            reason: j.reason,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// 测试：只读操作对所有角色都允许（包括 Guest）
    #[test]
    fn test_read_always_allowed() {
        let gate = ExecutionGate::new();
        let j = gate.judge("read_file", &json!({}), "read", "guest");
        assert_eq!(j.action, JudgmentAction::Allow);
    }

    /// 测试：Guest 不能执行写入操作
    #[test]
    fn test_guest_denied_write() {
        let gate = ExecutionGate::new();
        let j = gate.judge("write_file", &json!({}), "write", "guest");
        assert_eq!(j.action, JudgmentAction::Deny);
    }

    /// 测试：Member 可以执行写入操作
    #[test]
    fn test_member_allowed_write() {
        let gate = ExecutionGate::new();
        let j = gate.judge("write_file", &json!({}), "write", "member");
        assert_eq!(j.action, JudgmentAction::Allow);
    }

    /// 测试：Admin 执行危险操作需要确认
    #[test]
    fn test_admin_dangerous_confirm() {
        let gate = ExecutionGate::new();
        let j = gate.judge("nuke", &json!({}), "dangerous", "admin");
        assert_eq!(j.action, JudgmentAction::Confirm);
    }

    /// 测试：Member 不能执行危险操作
    #[test]
    fn test_member_dangerous_denied() {
        let gate = ExecutionGate::new();
        let j = gate.judge("nuke", &json!({}), "dangerous", "member");
        assert_eq!(j.action, JudgmentAction::Deny);
    }
}
