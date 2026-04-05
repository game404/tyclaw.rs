//! 权限控制配置（对应 config/control.yaml）。

use serde::Deserialize;

/// 权限控制根配置。
#[derive(Debug, Clone, Deserialize)]
pub struct ControlConfig {
    #[serde(default)]
    pub rbac: RbacConfig,
    #[serde(default)]
    pub rate_limit: RateLimitConfig,
    #[serde(default)]
    pub audit: AuditConfig,
}

impl Default for ControlConfig {
    fn default() -> Self {
        Self {
            rbac: RbacConfig::default(),
            rate_limit: RateLimitConfig::default(),
            audit: AuditConfig::default(),
        }
    }
}

/// RBAC 配置。
#[derive(Debug, Clone, Deserialize)]
pub struct RbacConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_role")]
    pub default_role: String,
}

impl Default for RbacConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            default_role: "admin".into(),
        }
    }
}

/// 速率限制配置。
#[derive(Debug, Clone, Deserialize)]
pub struct RateLimitConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_per_user")]
    pub per_user: usize,
    #[serde(default = "default_global")]
    pub global: usize,
    #[serde(default = "default_window_secs")]
    pub window_secs: u64,
}

impl Default for RateLimitConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            per_user: default_per_user(),
            global: default_global(),
            window_secs: default_window_secs(),
        }
    }
}

/// 审计日志配置。
#[derive(Debug, Clone, Deserialize)]
pub struct AuditConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
}

impl Default for AuditConfig {
    fn default() -> Self {
        Self { enabled: true }
    }
}

fn default_true() -> bool {
    true
}
fn default_role() -> String {
    "admin".into()
}
fn default_per_user() -> usize {
    5
}
fn default_global() -> usize {
    20
}
fn default_window_secs() -> u64 {
    60
}
