use std::collections::HashMap;
use std::sync::Arc;

use serde_json::Value;
use tracing::{info, warn};

use tyclaw_provider::LLMProvider;

use super::protocol::{ExecutionRecord, MergeReport, NodeStatus};

/// 从 config/prompts/reducer_prompt.md 加载（回退到内置默认值）。
fn llm_reducer_prompt() -> String {
    super::prompt_loader::reducer_prompt()
}

/// 规则归并器：按拓扑序拼接节点输出，标记冲突。
/// 可选 LLM 归并：当检测到冲突时，调用 reducer_model 做语义归并摘要。
pub struct RuleReducer {
    llm_reducer: Option<LlmReducer>,
}

struct LlmReducer {
    provider: Arc<dyn LLMProvider>,
    model: String,
}

impl RuleReducer {
    pub fn new() -> Self {
        Self { llm_reducer: None }
    }

    pub fn with_llm(provider: Arc<dyn LLMProvider>, model: String) -> Self {
        Self {
            llm_reducer: Some(LlmReducer { provider, model }),
        }
    }

    /// 将所有节点执行记录归并为最终用户回复。
    ///
    /// 策略：
    /// 1. 按节点顺序（保留原始拓扑序）拼接成功节点的输出。
    /// 2. 去除完全重复的段落。
    /// 3. 若检测到矛盾关键词，在输出中标注 `[conflict]`。
    /// 4. 若存在失败节点，附加失败摘要。
    /// 5. 若有冲突且配置了 LLM Reducer，调用 LLM 做语义归并。
    pub async fn reduce(&self, records: &[ExecutionRecord]) -> MergeReport {
        let mut segments: Vec<&str> = Vec::new();
        let mut seen_hashes = std::collections::HashSet::new();
        let mut has_conflicts = false;
        let mut partial_failure = false;
        let mut failure_notes = Vec::new();

        for rec in records {
            match rec.status {
                NodeStatus::Success => {
                    if let Some(ref text) = rec.output {
                        let trimmed = text.trim();
                        if trimmed.is_empty() {
                            continue;
                        }
                        let hash = simple_hash(trimmed);
                        if seen_hashes.insert(hash) {
                            segments.push(trimmed);
                        }
                    }
                }
                NodeStatus::Failed => {
                    partial_failure = true;
                    let err = rec.error.as_deref().unwrap_or("unknown error");
                    failure_notes.push(format!("[{}] failed: {}", rec.node_id, err));
                }
                NodeStatus::Skipped => {
                    partial_failure = true;
                    failure_notes.push(format!("[{}] skipped", rec.node_id));
                }
                _ => {}
            }
        }

        // 简易冲突检测：检查多个输出间是否有矛盾标记
        if segments.len() > 1 {
            has_conflicts = detect_contradiction(&segments);
        }

        let mut final_text = segments.join("\n\n");

        // 检测到冲突且配置了 LLM Reducer → 调用 LLM 做语义归并
        if has_conflicts {
            if let Some(ref llm) = self.llm_reducer {
                info!("Conflict detected, invoking LLM reducer for semantic merge");
                match llm.merge(&segments).await {
                    Ok(merged) => {
                        final_text = merged;
                        // LLM 归并成功后冲突标记保留，但文本已被整合
                    }
                    Err(e) => {
                        warn!(%e, "LLM reducer failed, falling back to rule-based merge");
                        final_text = format!(
                            "[conflict] Multiple outputs may contain contradictory conclusions.\n\n{final_text}"
                        );
                    }
                }
            } else {
                final_text = format!(
                    "[conflict] Multiple outputs may contain contradictory conclusions.\n\n{final_text}"
                );
            }
        }

        if !failure_notes.is_empty() {
            final_text.push_str("\n\n---\nPartial failure summary:\n");
            for note in &failure_notes {
                final_text.push_str(&format!("- {note}\n"));
            }
        }

        MergeReport {
            final_text,
            records: records.to_vec(),
            has_conflicts,
            partial_failure,
        }
    }
}

impl LlmReducer {
    async fn merge(&self, segments: &[&str]) -> Result<String, tyclaw_types::TyclawError> {
        let mut content = String::from("Here are the outputs from different sub-tasks:\n\n");
        for (i, seg) in segments.iter().enumerate() {
            content.push_str(&format!("--- Output {} ---\n{}\n\n", i + 1, seg));
        }

        let messages = vec![
            {
                let mut m = HashMap::new();
                m.insert("role".into(), Value::String("system".into()));
                m.insert("content".into(), Value::String(llm_reducer_prompt()));
                m
            },
            {
                let mut m = HashMap::new();
                m.insert("role".into(), Value::String("user".into()));
                m.insert("content".into(), Value::String(content));
                m
            },
        ];

        let response = self
            .provider
            .chat_with_retry(messages, None, Some(self.model.clone()), None)
            .await;

        if response.finish_reason == "error" {
            let err = response.content.unwrap_or_default();
            return Err(tyclaw_types::TyclawError::Provider(format!(
                "LLM reducer error: {err}"
            )));
        }

        Ok(response.content.unwrap_or_default())
    }
}

/// 简易矛盾检测：若多个段落中出现对立关键词对。
fn detect_contradiction(segments: &[&str]) -> bool {
    const CONTRADICTION_PAIRS: &[(&str, &str)] = &[
        ("yes", "no"),
        ("true", "false"),
        ("correct", "incorrect"),
        ("possible", "impossible"),
        ("recommend", "not recommend"),
    ];

    for (a, b) in CONTRADICTION_PAIRS {
        let mut found_a = false;
        let mut found_b = false;
        for seg in segments {
            let lower = seg.to_lowercase();
            if lower.contains(a) {
                found_a = true;
            }
            if lower.contains(b) {
                found_b = true;
            }
        }
        if found_a && found_b {
            return true;
        }
    }
    false
}

fn simple_hash(s: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    s.hash(&mut hasher);
    hasher.finish()
}
