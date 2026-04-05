//! 基于角色的访问控制（RBAC），支持分层权限管理。
//!
//! 角色层级（从低到高）：Guest < Member < Developer < Admin
//! 每个角色有预定义的权限集，包括：
//! - 允许的风险等级（read、write、dangerous）
//! - 管理权限（用户管理、技能管理、审计查看、知识蒸馏）

use std::collections::HashMap;

/// 内置角色层级定义（按权限从低到高排列）。
///
/// 使用数组索引作为权限等级数值，可以直接比较大小。
const ROLE_HIERARCHY: &[&str] = &["guest", "member", "developer", "admin"];

/// 各角色的默认权限配置。
///
/// | 角色        | 风险等级         | 管理用户 | 管理技能 | 查看审计 | 触发蒸馏 |
/// |------------|-----------------|---------|---------|---------|---------|
/// | Guest      | read            | ✗       | ✗       | ✗       | ✗       |
/// | Member     | read, write     | ✗       | ✓       | ✗       | ✗       |
/// | Developer  | read, write     | ✗       | ✓       | ✓       | ✗       |
/// | Admin      | read, write, dangerous | ✓ | ✓      | ✓       | ✓       |
fn default_permissions() -> HashMap<&'static str, RolePermissions> {
    let mut map = HashMap::new();
    map.insert(
        "guest",
        RolePermissions {
            allowed_risk_levels: vec!["read"], // 只能读
            can_manage_users: false,
            can_manage_skills: false,
            can_view_audit: false,
            can_trigger_distill: false,
        },
    );
    map.insert(
        "member",
        RolePermissions {
            allowed_risk_levels: vec!["read", "write"], // 读写
            can_manage_users: false,
            can_manage_skills: true, // 可管理技能
            can_view_audit: false,
            can_trigger_distill: false,
        },
    );
    map.insert(
        "developer",
        RolePermissions {
            allowed_risk_levels: vec!["read", "write"], // 读写
            can_manage_users: false,
            can_manage_skills: true,
            can_view_audit: true, // 可查看审计日志
            can_trigger_distill: false,
        },
    );
    map.insert(
        "admin",
        RolePermissions {
            allowed_risk_levels: vec!["read", "write", "dangerous"], // 全部风险等级
            can_manage_users: true,                                  // 可管理用户
            can_manage_skills: true,
            can_view_audit: true,
            can_trigger_distill: true, // 可触发知识蒸馏
        },
    );
    map
}

/// 角色权限定义结构。
///
/// 包含该角色允许的风险等级列表和各项管理权限标志。
#[derive(Debug, Clone)]
pub struct RolePermissions {
    pub allowed_risk_levels: Vec<&'static str>, // 允许的风险等级
    pub can_manage_users: bool,                 // 是否可管理用户
    pub can_manage_skills: bool,                // 是否可管理技能
    pub can_view_audit: bool,                   // 是否可查看审计日志
    pub can_trigger_distill: bool,              // 是否可触发知识蒸馏
}

/// RBAC 管理器 —— 提供角色权限查询接口。
pub struct RBACManager {
    permissions: HashMap<&'static str, RolePermissions>, // 角色 → 权限映射
}

impl RBACManager {
    /// 使用默认权限配置创建 RBAC 管理器。
    pub fn new() -> Self {
        Self {
            permissions: default_permissions(),
        }
    }

    /// 获取角色的数值等级（越高权限越大）。
    ///
    /// 基于 ROLE_HIERARCHY 数组的索引位置。
    /// 未知角色返回 0（最低权限）。
    pub fn role_rank(&self, role: &str) -> usize {
        ROLE_HIERARCHY.iter().position(|r| *r == role).unwrap_or(0)
    }

    /// 检查角色是否达到指定的最低等级。
    ///
    /// 例如：is_role_at_least("admin", "member") → true
    ///       is_role_at_least("guest", "member") → false
    pub fn is_role_at_least(&self, role: &str, minimum: &str) -> bool {
        self.role_rank(role) >= self.role_rank(minimum)
    }

    /// 检查角色是否被允许使用指定风险等级的工具。
    ///
    /// 查询该角色的 allowed_risk_levels 列表。
    pub fn is_risk_level_allowed(&self, role: &str, risk_level: &str) -> bool {
        self.permissions
            .get(role)
            .map(|p| p.allowed_risk_levels.contains(&risk_level))
            .unwrap_or(false) // 未知角色默认不允许
    }

    /// 检查角色是否具有指定的命名权限。
    ///
    /// 支持的权限名：manage_users、manage_skills、view_audit、trigger_distill
    pub fn has_permission(&self, role: &str, permission: &str) -> bool {
        let perms = match self.permissions.get(role) {
            Some(p) => p,
            None => return false,
        };
        match permission {
            "manage_users" => perms.can_manage_users,
            "manage_skills" => perms.can_manage_skills,
            "view_audit" => perms.can_view_audit,
            "trigger_distill" => perms.can_trigger_distill,
            _ => false, // 未知权限名默认不允许
        }
    }
}

impl Default for RBACManager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 测试：角色层级顺序
    #[test]
    fn test_role_hierarchy() {
        let rbac = RBACManager::new();
        assert!(rbac.role_rank("admin") > rbac.role_rank("member"));
        assert!(rbac.role_rank("developer") > rbac.role_rank("guest"));
    }

    /// 测试：各角色的风险等级权限
    #[test]
    fn test_risk_level_allowed() {
        let rbac = RBACManager::new();
        assert!(rbac.is_risk_level_allowed("member", "read")); // Member 可读
        assert!(rbac.is_risk_level_allowed("member", "write")); // Member 可写
        assert!(!rbac.is_risk_level_allowed("member", "dangerous")); // Member 不能执行危险操作
        assert!(rbac.is_risk_level_allowed("admin", "dangerous")); // Admin 可以
        assert!(!rbac.is_risk_level_allowed("guest", "write")); // Guest 不能写
    }

    /// 测试：角色等级比较
    #[test]
    fn test_is_role_at_least() {
        let rbac = RBACManager::new();
        assert!(rbac.is_role_at_least("admin", "member")); // Admin ≥ Member
        assert!(!rbac.is_role_at_least("guest", "member")); // Guest < Member
    }

    /// 测试：命名权限检查
    #[test]
    fn test_permissions() {
        let rbac = RBACManager::new();
        assert!(rbac.has_permission("admin", "manage_users")); // Admin 可管理用户
        assert!(!rbac.has_permission("member", "manage_users")); // Member 不能管理用户
        assert!(rbac.has_permission("member", "manage_skills")); // Member 可管理技能
    }
}
