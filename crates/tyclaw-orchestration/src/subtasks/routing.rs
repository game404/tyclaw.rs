use serde::{Deserialize, Serialize};

/// 路由策略：按 node_type 将节点映射到目标模型。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoutingPolicy {
    /// 按优先级排列的路由规则列表（先匹配先命中）。
    #[serde(default)]
    pub rules: Vec<RoutingRule>,
    /// 无规则匹配时的回退模型。
    pub default_model: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoutingRule {
    /// 支持 exact match 和末尾通配符 `*`（如 `coding*`）。
    pub node_type_pattern: String,
    pub target_model: String,
}

impl RoutingPolicy {
    /// 为给定 node_type 解析目标模型。
    /// 优先检查 `model_override`（节点级覆盖），再遍历规则，最后走 `default_model`。
    pub fn resolve(&self, node_type: &str, model_override: Option<&str>) -> String {
        if let Some(ov) = model_override {
            if !ov.is_empty() {
                return ov.to_string();
            }
        }
        for rule in &self.rules {
            if pattern_matches(&rule.node_type_pattern, node_type) {
                return rule.target_model.clone();
            }
        }
        self.default_model.clone()
    }
}

/// 简易模式匹配：exact match 或末尾 `*` 通配。
fn pattern_matches(pattern: &str, value: &str) -> bool {
    if let Some(prefix) = pattern.strip_suffix('*') {
        value.starts_with(prefix)
    } else {
        pattern == value
    }
}

impl Default for RoutingPolicy {
    fn default() -> Self {
        Self {
            rules: vec![],
            default_model: String::new(),
        }
    }
}
