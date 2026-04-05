//! 记忆合并器 —— 基于 token 预算的自动合并策略。
//!
//! 当会话的 token 数量超过上下文窗口的 50% 时，
//! 自动将较早的消息交给 LLM 合并为 MEMORY.md + HISTORY.md。

use serde_json::Value;
use std::collections::HashMap;
use std::path::Path;
use tracing::{info, warn};

use tyclaw_provider::LLMProvider;
use tyclaw_types::tokens::estimate_message_tokens;

use crate::memory_store::{consolidate_with_provider, MemoryStore};

/// 最大合并轮次（防止无限循环）。
const MAX_ROUNDS: usize = 5;

/// 记忆合并器。
pub struct MemoryConsolidator {
    pub store: MemoryStore,
    context_window_tokens: usize,
}

impl MemoryConsolidator {
    /// 创建合并器。`memory_dir` 是记忆存储目录（如 `workspaces/{key}/memory`）。
    pub fn new(memory_dir: &Path, context_window_tokens: usize) -> Self {
        Self {
            store: MemoryStore::new(memory_dir),
            context_window_tokens,
        }
    }

    /// 找到合并边界：从 last_consolidated 开始往后扫描，
    /// 在 user 消息处设置边界，直到移除的 token 数量达到目标。
    ///
    /// 返回 (boundary_idx, removed_tokens)。
    pub fn pick_consolidation_boundary(
        &self,
        messages: &[HashMap<String, Value>],
        last_consolidated: usize,
        tokens_to_remove: usize,
    ) -> Option<(usize, usize)> {
        if last_consolidated >= messages.len() || tokens_to_remove == 0 {
            return None;
        }

        let mut removed_tokens = 0usize;
        let mut last_boundary: Option<(usize, usize)> = None;

        for idx in last_consolidated..messages.len() {
            let msg = &messages[idx];
            if idx > last_consolidated {
                let role = msg.get("role").and_then(|v| v.as_str()).unwrap_or("");
                if role == "user" {
                    last_boundary = Some((idx, removed_tokens));
                    if removed_tokens >= tokens_to_remove {
                        return last_boundary;
                    }
                }
            }
            removed_tokens += estimate_message_tokens(msg);
        }

        last_boundary
    }

    /// 归档所有未合并的消息（用于 /new 命令）。
    pub async fn archive_unconsolidated(
        &self,
        messages: &[HashMap<String, Value>],
        last_consolidated: usize,
        provider: &dyn LLMProvider,
        model: &str,
    ) -> bool {
        let snapshot = &messages[last_consolidated..];
        if snapshot.is_empty() {
            return true;
        }
        consolidate_with_provider(&self.store, snapshot, provider, model).await
    }

    /// 基于 token 预算的自动合并。
    ///
    /// 返回新的 last_consolidated 值（如果发生了合并）。
    pub async fn maybe_consolidate_by_tokens(
        &self,
        messages: &[HashMap<String, Value>],
        last_consolidated: usize,
        build_messages_fn: &(dyn Fn(&[HashMap<String, Value>], &str) -> Vec<HashMap<String, Value>>
              + Send
              + Sync),
        get_tool_defs_fn: &(dyn Fn() -> Vec<Value> + Send + Sync),
        provider: &dyn LLMProvider,
        model: &str,
    ) -> usize {
        if messages.is_empty() || self.context_window_tokens == 0 {
            return last_consolidated;
        }

        let target = self.context_window_tokens / 2;
        let mut current_last_consolidated = last_consolidated;

        for round_num in 0..MAX_ROUNDS {
            // 估算当前 prompt tokens
            let history: Vec<HashMap<String, Value>> =
                messages[current_last_consolidated..].to_vec();
            let probe = build_messages_fn(&history, "[token-probe]");
            let tool_defs = get_tool_defs_fn();
            let (estimated, _source) =
                tyclaw_types::tokens::estimate_prompt_tokens_chain(&probe, Some(&tool_defs));

            if estimated == 0 || estimated < self.context_window_tokens {
                return current_last_consolidated;
            }

            if estimated <= target {
                return current_last_consolidated;
            }

            let tokens_to_remove = std::cmp::max(1, estimated - target);
            let boundary = self.pick_consolidation_boundary(
                messages,
                current_last_consolidated,
                tokens_to_remove,
            );

            let Some((end_idx, _)) = boundary else {
                return current_last_consolidated;
            };

            let chunk = &messages[current_last_consolidated..end_idx];
            if chunk.is_empty() {
                return current_last_consolidated;
            }

            info!(
                round = round_num,
                estimated,
                context_window = self.context_window_tokens,
                chunk_len = chunk.len(),
                "Consolidation round"
            );

            if !consolidate_with_provider(&self.store, chunk, provider, model).await {
                warn!("Consolidation failed at round {}", round_num);
                return current_last_consolidated;
            }

            current_last_consolidated = end_idx;
        }

        current_last_consolidated
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_pick_boundary() {
        let tmp = tempfile::TempDir::new().unwrap();
        let consolidator = MemoryConsolidator::new(tmp.path(), 200_000);

        let messages: Vec<HashMap<String, Value>> = vec![
            {
                let mut m = HashMap::new();
                m.insert("role".into(), json!("user"));
                m.insert("content".into(), json!("first question"));
                m
            },
            {
                let mut m = HashMap::new();
                m.insert("role".into(), json!("assistant"));
                m.insert("content".into(), json!("first answer"));
                m
            },
            {
                let mut m = HashMap::new();
                m.insert("role".into(), json!("user"));
                m.insert("content".into(), json!("second question"));
                m
            },
        ];

        let result = consolidator.pick_consolidation_boundary(&messages, 0, 1);
        assert!(result.is_some());
        let (idx, _) = result.unwrap();
        assert_eq!(idx, 2); // 第二个 user 消息处
    }

    #[test]
    fn test_pick_boundary_empty() {
        let tmp = tempfile::TempDir::new().unwrap();
        let consolidator = MemoryConsolidator::new(tmp.path(), 200_000);

        let messages: Vec<HashMap<String, Value>> = vec![];
        let result = consolidator.pick_consolidation_boundary(&messages, 0, 100);
        assert!(result.is_none());
    }
}
