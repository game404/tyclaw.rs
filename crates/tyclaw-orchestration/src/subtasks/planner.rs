use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;
use tracing::{info, warn};

use tyclaw_provider::LLMProvider;
use tyclaw_types::TyclawError;

use super::prompt_loader;
use super::protocol::{PlanMetadata, TaskPlan};

/// Planner trait：将用户请求转换为 TaskPlan DAG。
#[async_trait]
pub trait Planner: Send + Sync {
    async fn plan(&self, user_request: &str) -> Result<TaskPlan, TyclawError>;
}

/// 基于 LLM 的 Planner 实现。
pub struct LLMPlanner {
    provider: Arc<dyn LLMProvider>,
    model: String,
}

impl LLMPlanner {
    pub fn new(provider: Arc<dyn LLMProvider>, model: String) -> Self {
        Self { provider, model }
    }

    fn build_messages(&self, user_request: &str) -> Vec<std::collections::HashMap<String, Value>> {
        use std::collections::HashMap;

        let system_msg = {
            let mut m = HashMap::new();
            m.insert("role".into(), Value::String("system".into()));
            m.insert(
                "content".into(),
                Value::String(prompt_loader::planner_system_prompt()),
            );
            m
        };
        let user_msg = {
            let mut m = HashMap::new();
            m.insert("role".into(), Value::String("user".into()));
            m.insert("content".into(), Value::String(user_request.into()));
            m
        };
        vec![system_msg, user_msg]
    }
}

#[async_trait]
impl Planner for LLMPlanner {
    async fn plan(&self, user_request: &str) -> Result<TaskPlan, TyclawError> {
        let messages = self.build_messages(user_request);
        let response = self
            .provider
            .chat_with_retry(messages, None, Some(self.model.clone()), None)
            .await;

        if response.finish_reason == "error" {
            warn!(
                error = response.content.as_deref().unwrap_or(""),
                "Planner LLM call failed, falling back to single-node plan"
            );
            return Ok(TaskPlan::single_node_fallback(user_request.into()));
        }

        let raw = response.content.unwrap_or_default();
        match parse_plan(&raw) {
            Ok(mut plan) => {
                plan.metadata = PlanMetadata {
                    source_prompt: user_request.into(),
                    planner_model: self.model.clone(),
                };
                if let Err(e) = plan.validate() {
                    warn!(%e, "Planner output failed validation, falling back");
                    return Ok(TaskPlan::single_node_fallback(user_request.into()));
                }
                info!(plan_id = %plan.id, nodes = plan.nodes.len(), "Plan generated");
                Ok(plan)
            }
            Err(e) => {
                warn!(%e, "Failed to parse planner output, falling back");
                Ok(TaskPlan::single_node_fallback(user_request.into()))
            }
        }
    }
}

fn parse_plan(raw: &str) -> Result<TaskPlan, TyclawError> {
    let trimmed = raw.trim();
    // 尝试从 markdown code fence 提取 JSON
    let json_str = if trimmed.starts_with("```") {
        trimmed
            .strip_prefix("```json")
            .or_else(|| trimmed.strip_prefix("```"))
            .and_then(|s| s.strip_suffix("```"))
            .unwrap_or(trimmed)
            .trim()
    } else {
        trimmed
    };
    let plan: TaskPlan = serde_json::from_str(json_str)?;
    Ok(plan)
}
