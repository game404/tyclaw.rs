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

/// 判断 `messages[idx]` 是否为一个 user turn 的起点（role == "user"）。
fn is_user_turn_start(messages: &[HashMap<String, Value>], idx: usize) -> bool {
    messages
        .get(idx)
        .and_then(|m| m.get("role"))
        .and_then(|v| v.as_str())
        == Some("user")
}

/// 在 `[last_consolidated, end)` 区间内按 user turn 边界切出一个不超过
/// `max_messages` 条的批次，返回该批次的结束下标（user turn 对齐）。
///
/// 「user turn」以一条 `role == "user"` 的消息为起点，延续到下一条 user
/// 消息之前。批次边界只能落在某个 user turn 的起点或序列末尾，从而保证
/// **不拆分单个 user turn**（Requirement 4.2）。
///
/// 行为细节：
/// - 在所有合法边界中，选取使批次消息数 ≤ `max_messages` 的**最大**边界，
///   以尽量减少合并轮次。
/// - 返回值始终 > `last_consolidated`（除非 `last_consolidated >= len`，
///   此时无可合并消息，原样返回），从而保证调用方能持续推进、不会卡死。
/// - **超大单 turn 的边界情况**：若从 `last_consolidated` 起的第一个 user
///   turn 本身就超过 `max_messages`（例如一个 turn 内有大量 assistant/tool
///   消息），则**不拆分该 turn**，返回其完整结束下标，即便批次大小 >
///   `max_messages`。这优先满足 Requirement 4.2「不拆分单个 user turn」，
///   并保证合并能取得进展。`max_messages == 0` 同理退化为返回第一个 turn 的
///   结束下标。
pub fn pick_batch_boundary(
    messages: &[HashMap<String, Value>],
    last_consolidated: usize,
    max_messages: usize,
) -> usize {
    let len = messages.len();
    if last_consolidated >= len {
        return last_consolidated;
    }

    // 候选边界 = (last_consolidated, len] 中每个 user turn 起点，外加 len 本身。
    // 批次大小随边界下标单调递增，因此一旦某边界超限，后续边界必然也超限。
    let mut best: Option<usize> = None;
    for idx in (last_consolidated + 1)..=len {
        let is_boundary = idx == len || is_user_turn_start(messages, idx);
        if !is_boundary {
            continue;
        }

        let batch_size = idx - last_consolidated;
        if batch_size <= max_messages {
            // 仍在上限内，记录为当前最优并尝试继续扩展到更大的边界。
            best = Some(idx);
        } else if best.is_some() {
            // 已经有一个合法（≤ 上限）边界，且当前边界超限 —— 用前一个。
            break;
        } else {
            // 第一个候选边界就已超限：单个 user turn 大于上限。
            // 不拆分该 turn，返回其完整结束下标。
            return idx;
        }
    }

    best.unwrap_or(len)
}

/// 单个分片批次的合并结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BatchOutcome {
    /// 该批次合并成功。
    Success,
    /// 该批次合并失败，附带失败原因。
    Failure(String),
}

/// 单次分批合并调用的执行摘要。
///
/// 这是 [`run_consolidation_batches`] / [`MemoryConsolidator::consolidate_in_batches`]
/// 的返回值，记录最终推进到的合并边界与统计信息，供调用方更新
/// `last_consolidated` 并写日志（Requirement 4.4/4.5/4.6）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConsolidationRunResult {
    /// 合并后新的 `last_consolidated` 下标。
    ///
    /// - 仅按**成功**合并的批次推进；失败批次及其之后保留为未合并（R4.6）。
    /// - 达批次上限后剩余消息不再推进（R4.5）。
    pub last_consolidated: usize,
    /// 本次成功处理的分片批次数（恒 ≤ `max_rounds`，R4.3）。
    pub batches_processed: usize,
    /// 本次成功合并的消息总数（R4.4）。
    pub messages_processed: usize,
    /// 若某批失败，记录 `(失败批次序号(从 1 起), 失败原因)`（R4.6）。
    /// `None` 表示没有批次失败。
    pub failed_batch: Option<(usize, String)>,
}

/// 规划下一个待合并的分片批次范围 `[start, end)`。
///
/// 这是分批合并循环的**纯**核心：仅依据消息序列、当前边界、单批上限与
/// 「已处理批次数 / 批次上限」决定是否还要继续，以及下一批的范围。它同时
/// 被纯测试函数 [`run_consolidation_batches`] 与异步实际合并
/// [`MemoryConsolidator::consolidate_in_batches`] 复用，确保两条路径的
/// 上限/边界语义完全一致。
///
/// 返回 `None` 表示无需继续：
/// - 已达 `max_rounds` 批次上限（R4.3，剩余消息保留未合并 → R4.5）；
/// - 当前边界已到达或越过序列末尾（无剩余消息）；
/// - [`pick_batch_boundary`] 无法推进（防卡死）。
pub(crate) fn plan_next_batch(
    messages: &[HashMap<String, Value>],
    current: usize,
    max_messages_per_batch: usize,
    batches_done: usize,
    max_rounds: usize,
) -> Option<(usize, usize)> {
    if batches_done >= max_rounds {
        return None;
    }
    if current >= messages.len() {
        return None;
    }
    let end = pick_batch_boundary(messages, current, max_messages_per_batch);
    if end <= current {
        return None;
    }
    Some((current, end))
}

/// 分批合并循环的纯实现（不依赖 LLM provider，便于确定性测试）。
///
/// 给定消息序列、起始合并边界、单批消息上限、批次上限，以及一个对每个批次
/// 执行实际合并的闭包 `consolidate_batch(batch_index, batch) -> BatchOutcome`
/// （`batch_index` 从 1 起），按以下规则推进并返回 [`ConsolidationRunResult`]：
///
/// - 单次调用最多处理 `max_rounds` 个批次（R4.3）。
/// - 仅成功批次推进 `last_consolidated`；达上限后剩余消息保留未合并（R4.5）。
/// - 某批失败则**立即停止**后续批次，`last_consolidated` 仅推进到失败批之前
///   （失败批及其之后保留未合并），并记录失败批次序号与原因（R4.6）。
///
/// 注意：批次边界由 [`pick_batch_boundary`] 确定，仅依赖消息内容与当前位置，
/// 与合并结果无关；失败会立即终止循环，故失败批之前的所有批次必然是成功的。
pub fn run_consolidation_batches<F>(
    messages: &[HashMap<String, Value>],
    last_consolidated: usize,
    max_messages_per_batch: usize,
    max_rounds: usize,
    mut consolidate_batch: F,
) -> ConsolidationRunResult
where
    F: FnMut(usize, &[HashMap<String, Value>]) -> BatchOutcome,
{
    let mut current = last_consolidated;
    let mut batches_processed = 0usize;
    let mut messages_processed = 0usize;
    let mut failed_batch: Option<(usize, String)> = None;

    while let Some((start, end)) = plan_next_batch(
        messages,
        current,
        max_messages_per_batch,
        batches_processed,
        max_rounds,
    ) {
        let batch = &messages[start..end];
        let batch_index = batches_processed + 1;
        match consolidate_batch(batch_index, batch) {
            BatchOutcome::Success => {
                batches_processed += 1;
                messages_processed += batch.len();
                current = end;
            }
            BatchOutcome::Failure(reason) => {
                // 失败批不推进边界：保留该批及之后为未合并（R4.6）。
                failed_batch = Some((batch_index, reason));
                break;
            }
        }
    }

    ConsolidationRunResult {
        last_consolidated: current,
        batches_processed,
        messages_processed,
        failed_batch,
    }
}

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

    /// 按消息数上限分批合并 `[last_consolidated, len)` 区间的消息（Requirement 4）。
    ///
    /// 使用 [`pick_batch_boundary`] 在不拆分单个 user turn 的前提下切出每个
    /// ≤ `max_messages_per_batch` 的批次，并通过 [`consolidate_with_provider`]
    /// 逐批交给 LLM 合并。推进/上限/失败语义与纯函数
    /// [`run_consolidation_batches`] 完全一致（复用 [`plan_next_batch`]）：
    ///
    /// - 单次调用最多处理 `max_rounds`（默认 5）个批次（R4.3）；
    /// - 达上限后剩余消息保留为未合并、不推进边界（R4.5）；
    /// - 某批失败则停止后续批次、保留失败批及之后为未合并，并记录失败批次
    ///   序号与原因（R4.6）；
    /// - 本次处理的消息数与批次数写入日志（R4.4）。
    ///
    /// 返回 [`ConsolidationRunResult`]，调用方据此更新自身的 `last_consolidated`。
    pub async fn consolidate_in_batches(
        &self,
        messages: &[HashMap<String, Value>],
        last_consolidated: usize,
        max_messages_per_batch: usize,
        max_rounds: usize,
        provider: &dyn LLMProvider,
        model: &str,
    ) -> ConsolidationRunResult {
        let mut current = last_consolidated;
        let mut batches_processed = 0usize;
        let mut messages_processed = 0usize;
        let mut failed_batch: Option<(usize, String)> = None;

        while let Some((start, end)) = plan_next_batch(
            messages,
            current,
            max_messages_per_batch,
            batches_processed,
            max_rounds,
        ) {
            let batch = &messages[start..end];
            let batch_index = batches_processed + 1;

            info!(
                batch_index,
                batch_len = batch.len(),
                start,
                end,
                "Memory consolidation batch start"
            );

            if consolidate_with_provider(&self.store, batch, provider, model).await {
                batches_processed += 1;
                messages_processed += batch.len();
                current = end;
            } else {
                // R4.6：失败批不推进边界，停止后续批次，记录序号与原因。
                let reason = "consolidate_with_provider returned false".to_string();
                warn!(
                    batch_index,
                    batch_len = batch.len(),
                    reason = %reason,
                    "Memory consolidation batch failed; stopping further batches"
                );
                failed_batch = Some((batch_index, reason));
                break;
            }
        }

        // R4.4：记录本次处理的消息数与批次数。
        info!(
            messages_processed,
            batches_processed,
            last_consolidated = current,
            failed = failed_batch.is_some(),
            "Memory consolidation run complete"
        );

        ConsolidationRunResult {
            last_consolidated: current,
            batches_processed,
            messages_processed,
            failed_batch,
        }
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
    use proptest::prelude::*;
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

    // --- pick_batch_boundary tests (Requirement 4.2) ---

    fn msg(role: &str) -> HashMap<String, Value> {
        let mut m = HashMap::new();
        m.insert("role".into(), json!(role));
        m.insert("content".into(), json!("x"));
        m
    }

    /// 构造一个消息序列：每个元素是一个 role 字符串。
    fn seq(roles: &[&str]) -> Vec<HashMap<String, Value>> {
        roles.iter().map(|r| msg(r)).collect()
    }

    #[test]
    fn test_batch_boundary_empty_or_consumed() {
        let messages = seq(&["user", "assistant"]);
        // last_consolidated 已到末尾 —— 无可合并消息，原样返回。
        assert_eq!(pick_batch_boundary(&messages, 2, 10), 2);
        // 空序列。
        assert_eq!(pick_batch_boundary(&[], 0, 10), 0);
    }

    #[test]
    fn test_batch_boundary_fits_under_limit() {
        // 两个 user turn，总共 4 条，上限 10 -> 一次性全部取到末尾。
        let messages = seq(&["user", "assistant", "user", "assistant"]);
        assert_eq!(pick_batch_boundary(&messages, 0, 10), 4);
    }

    #[test]
    fn test_batch_boundary_stops_at_user_turn() {
        // turns: [0..2), [2..4), [4..6). 上限 3 -> 第一个 turn (2条) 可，
        // 加上第二个 turn (4条) 超限，因此边界落在 idx=2。
        let messages = seq(&["user", "assistant", "user", "assistant", "user", "assistant"]);
        let b = pick_batch_boundary(&messages, 0, 3);
        assert_eq!(b, 2);
        assert!(is_user_turn_start(&messages, b));
        assert!(b - 0 <= 3);
    }

    #[test]
    fn test_batch_boundary_picks_largest_within_limit() {
        // turns: [0..2),[2..4),[4..6). 上限 4 -> 取两个 turn，边界 idx=4。
        let messages = seq(&["user", "assistant", "user", "assistant", "user", "assistant"]);
        let b = pick_batch_boundary(&messages, 0, 4);
        assert_eq!(b, 4);
        assert!(b - 0 <= 4);
    }

    #[test]
    fn test_batch_boundary_does_not_split_oversized_turn() {
        // 单个 user turn 有 5 条消息，上限 3 -> 不拆分，返回完整 turn 结束下标。
        let messages = seq(&["user", "assistant", "tool", "assistant", "tool"]);
        let b = pick_batch_boundary(&messages, 0, 3);
        assert_eq!(b, 5); // 完整 turn，即便 > max_messages
    }

    #[test]
    fn test_batch_boundary_oversized_first_turn_then_more() {
        // turn1 = [0..4) (4条) 超过上限 2；不拆分 -> 返回 4。
        let messages = seq(&["user", "assistant", "tool", "assistant", "user", "assistant"]);
        let b = pick_batch_boundary(&messages, 0, 2);
        assert_eq!(b, 4);
        assert!(is_user_turn_start(&messages, b));
    }

    #[test]
    fn test_batch_boundary_zero_max_makes_progress() {
        // max_messages == 0 不能卡死：返回第一个 turn 的结束下标。
        let messages = seq(&["user", "assistant", "user", "assistant"]);
        let b = pick_batch_boundary(&messages, 0, 0);
        assert_eq!(b, 2);
        assert!(b > 0);
    }

    #[test]
    fn test_batch_boundary_from_nonzero_last_consolidated() {
        // 从中途开始合并：last_consolidated=2，上限 10 -> 取到末尾。
        let messages = seq(&["user", "assistant", "user", "assistant", "tool"]);
        assert_eq!(pick_batch_boundary(&messages, 2, 10), 5);
    }

    // --- run_consolidation_batches tests (Requirements 4.3/4.4/4.5/4.6) ---

    /// 构造 `n_turns` 个 2 条消息的 user turn（user + assistant）。
    fn turns(n_turns: usize) -> Vec<HashMap<String, Value>> {
        let mut roles = Vec::new();
        for _ in 0..n_turns {
            roles.push("user");
            roles.push("assistant");
        }
        seq(&roles)
    }

    #[test]
    fn test_run_batches_all_success_advances_to_end() {
        // 6 个 turn（12 条），每批上限 2 条（=1 turn），上限 10 轮 -> 全部合并。
        let messages = turns(6);
        let result = run_consolidation_batches(&messages, 0, 2, 10, |_idx, _batch| {
            BatchOutcome::Success
        });
        assert_eq!(result.last_consolidated, 12);
        assert_eq!(result.batches_processed, 6);
        assert_eq!(result.messages_processed, 12);
        assert!(result.failed_batch.is_none());
    }

    #[test]
    fn test_run_batches_caps_at_max_rounds() {
        // 10 个 turn（20 条），每批 2 条（1 turn），max_rounds=5
        // -> 只处理 5 批，边界停在第 5 批末尾（idx=10），剩余保留未合并。
        let messages = turns(10);
        let result = run_consolidation_batches(&messages, 0, 2, 5, |_idx, _batch| {
            BatchOutcome::Success
        });
        assert_eq!(result.batches_processed, 5);
        assert_eq!(result.last_consolidated, 10); // 第 5 批结束下标
        assert_eq!(result.messages_processed, 10);
        assert!(result.failed_batch.is_none());
        // 剩余 10 条消息（idx 10..20）保留为未合并。
        assert!(result.last_consolidated < messages.len());
    }

    #[test]
    fn test_run_batches_never_exceeds_max_rounds() {
        // 不论消息多少，batches_processed <= max_rounds（Property 15）。
        let messages = turns(20);
        let result = run_consolidation_batches(&messages, 0, 2, 5, |_idx, _batch| {
            BatchOutcome::Success
        });
        assert!(result.batches_processed <= 5);
    }

    #[test]
    fn test_run_batches_stops_on_failure() {
        // 第 3 批失败 -> 停止，边界推进到第 3 批起点（idx=4），
        // 失败批及之后保留未合并，记录失败序号 3 与原因。
        let messages = turns(6);
        let result = run_consolidation_batches(&messages, 0, 2, 10, |idx, _batch| {
            if idx == 3 {
                BatchOutcome::Failure("boom".to_string())
            } else {
                BatchOutcome::Success
            }
        });
        assert_eq!(result.batches_processed, 2); // 前 2 批成功
        assert_eq!(result.last_consolidated, 4); // 第 3 批起点（前 2 个 turn = 4 条）
        assert_eq!(result.messages_processed, 4);
        assert_eq!(result.failed_batch, Some((3, "boom".to_string())));
    }

    #[test]
    fn test_run_batches_first_batch_failure_no_advance() {
        // 第 1 批就失败 -> 边界不推进，保留全部未合并。
        let messages = turns(4);
        let result = run_consolidation_batches(&messages, 0, 2, 10, |_idx, _batch| {
            BatchOutcome::Failure("nope".to_string())
        });
        assert_eq!(result.batches_processed, 0);
        assert_eq!(result.last_consolidated, 0);
        assert_eq!(result.messages_processed, 0);
        assert_eq!(result.failed_batch, Some((1, "nope".to_string())));
    }

    #[test]
    fn test_run_batches_empty_or_consumed() {
        // 无剩余消息 -> 不处理任何批次。
        let messages = turns(3);
        let result = run_consolidation_batches(&messages, 6, 2, 5, |_idx, _batch| {
            BatchOutcome::Success
        });
        assert_eq!(result.batches_processed, 0);
        assert_eq!(result.last_consolidated, 6);
        assert!(result.failed_batch.is_none());

        let empty: Vec<HashMap<String, Value>> = vec![];
        let r2 = run_consolidation_batches(&empty, 0, 2, 5, |_idx, _b| BatchOutcome::Success);
        assert_eq!(r2.batches_processed, 0);
        assert_eq!(r2.last_consolidated, 0);
    }

    #[test]
    fn test_run_batches_zero_max_rounds() {
        // max_rounds == 0 -> 不处理任何批次（边界不推进）。
        let messages = turns(4);
        let result = run_consolidation_batches(&messages, 0, 2, 0, |_idx, _batch| {
            BatchOutcome::Success
        });
        assert_eq!(result.batches_processed, 0);
        assert_eq!(result.last_consolidated, 0);
    }

    #[test]
    fn test_plan_next_batch_respects_round_cap() {
        let messages = turns(6);
        // 已处理 5 批，max_rounds=5 -> 不再规划。
        assert_eq!(plan_next_batch(&messages, 4, 2, 5, 5), None);
        // 还没到上限 -> 规划下一批。
        assert_eq!(plan_next_batch(&messages, 4, 2, 2, 5), Some((4, 6)));
    }

    // --- Property-based test for pick_batch_boundary (Requirement 4.2) ---

    proptest! {
        #![proptest_config(ProptestConfig { cases: 100, ..Default::default() })]

        // Feature: execution-performance-optimization, Property 14: 合并分片不超上限且不拆分 user turn
        #[test]
        fn prop_pick_batch_boundary_no_overflow_no_split(
            (turn_sizes, max_messages, lc_turn_idx) in (1usize..=6)
                // k = 单个 user turn 的最大消息数。每个 turn 的大小约束在 1..=k，
                // 并令 max_messages >= k，从而保证生成的输入永远不会触发
                // “超大单 turn 不拆分” 的边界例外（见 pick_batch_boundary 文档）。
                .prop_flat_map(|k| {
                    (
                        // 一串 user turn 的大小，每个在 1..=k 之间。
                        prop::collection::vec(1usize..=k, 1..=8),
                        // 上限至少为 k（>= 最大单 turn 大小），上界放宽到 3k。
                        k..=(k * 3),
                    )
                })
                .prop_flat_map(|(turn_sizes, max_messages)| {
                    // last_consolidated 须对齐到某个 turn 起点（或序列末尾），
                    // 用 0..=turn_sizes.len() 选择第几个 turn 起点。
                    let n = turn_sizes.len();
                    (Just(turn_sizes), Just(max_messages), 0usize..=n)
                })
        ) {
            // 根据 turn 大小构造消息序列：每个 turn = [user, (size-1) 个 assistant/tool]。
            let mut roles: Vec<&str> = Vec::new();
            let mut turn_starts: Vec<usize> = Vec::new();
            for &size in &turn_sizes {
                turn_starts.push(roles.len());
                roles.push("user");
                for i in 1..size {
                    roles.push(if i % 2 == 1 { "assistant" } else { "tool" });
                }
            }
            let msgs = seq(&roles);

            // 对齐位点 = 各 turn 起点 + 序列末尾。
            let mut aligned: Vec<usize> = turn_starts;
            aligned.push(msgs.len());
            let last_consolidated = aligned[lc_turn_idx];

            let boundary = pick_batch_boundary(&msgs, last_consolidated, max_messages);

            // (a) 批次大小不超上限。因每个 turn <= k <= max_messages，
            //     绝不会触发 “超大单 turn” 例外，故该不变量必然成立。
            prop_assert!(
                boundary - last_consolidated <= max_messages,
                "batch size {} exceeds cap {} (lc={}, boundary={})",
                boundary - last_consolidated,
                max_messages,
                last_consolidated,
                boundary
            );

            // (b) 边界落在 user turn 边界上（不拆分单个 user turn）：
            //     要么是序列末尾，要么是下一个 user turn 的起点。
            prop_assert!(
                boundary == msgs.len() || is_user_turn_start(&msgs, boundary),
                "boundary {} does not land on a user-turn start (lc={})",
                boundary,
                last_consolidated
            );
        }
    }

    // --- Property-based test for run_consolidation_batches round cap (Requirement 4.3) ---

    proptest! {
        #![proptest_config(ProptestConfig { cases: 100, ..Default::default() })]

        // Feature: execution-performance-optimization, Property 15: 单次合并调用至多处理 5 个分片批次
        #[test]
        fn prop_consolidation_caps_at_max_rounds(
            // 任意大的未合并序列：1..=30 个 user turn（每个 turn = user + assistant，2 条）。
            n_turns in 1usize..=30,
            // 单批消息上限（小到足以产生远多于 max_rounds 的潜在批次）。
            max_messages_per_batch in 1usize..=4,
            // 包含默认值 5 的批次上限范围。
            max_rounds in 0usize..=8,
        ) {
            let messages = turns(n_turns);

            // 全成功闭包：让合并尽可能多地推进，最大化批次数。
            let result = run_consolidation_batches(
                &messages,
                0,
                max_messages_per_batch,
                max_rounds,
                |_idx, _batch| BatchOutcome::Success,
            );

            // Property 15 核心不变量：无论序列多大，单次调用处理的批次数
            // 恒不超过 max_rounds。
            prop_assert!(
                result.batches_processed <= max_rounds,
                "batches_processed {} exceeds max_rounds {} (n_turns={}, max_per_batch={})",
                result.batches_processed,
                max_rounds,
                n_turns,
                max_messages_per_batch
            );

            // 显式覆盖默认 max_rounds = 5 的情形：同一个任意大序列下仍 <= 5。
            let default_result = run_consolidation_batches(
                &messages,
                0,
                max_messages_per_batch,
                MAX_ROUNDS,
                |_idx, _batch| BatchOutcome::Success,
            );
            prop_assert!(
                default_result.batches_processed <= MAX_ROUNDS,
                "default-round batches_processed {} exceeds MAX_ROUNDS {} (n_turns={})",
                default_result.batches_processed,
                MAX_ROUNDS,
                n_turns
            );
        }
    }

    // --- Property-based test for cap-reached remainder retention (Requirement 4.5) ---

    proptest! {
        #![proptest_config(ProptestConfig { cases: 100, ..Default::default() })]

        // Feature: execution-performance-optimization, Property 16: 达批次上限后剩余消息保留为未合并
        #[test]
        fn prop_consolidation_cap_keeps_remainder_unconsolidated(
            // 批次上限 1..=5。
            max_rounds in 1usize..=5,
            // 比 max_rounds 能消费的更多的 user turn，确保一定会触达上限。
            // 每个 turn = user + assistant（2 条），每批上限 2 条即 1 turn。
            extra_turns in 0usize..=25,
        ) {
            // n_turns 严格大于 max_rounds，保证至少有剩余 turn 未被合并。
            let n_turns = max_rounds + 1 + extra_turns;
            let messages = turns(n_turns);
            // 每批上限 2 条 = 恰好一个 user turn，使每批推进一个 turn。
            let max_messages_per_batch = 2usize;

            // 全成功闭包：让上限（而非失败）成为唯一的限制因素。
            let result = run_consolidation_batches(
                &messages,
                0,
                max_messages_per_batch,
                max_rounds,
                |_idx, _batch| BatchOutcome::Success,
            );

            // 因 n_turns > max_rounds 且每批推进一个 turn、全部成功，
            // 处理批次数必然恰好达到上限 max_rounds。
            prop_assert_eq!(
                result.batches_processed,
                max_rounds,
                "expected cap to be reached: batches_processed={}, max_rounds={}, n_turns={}",
                result.batches_processed,
                max_rounds,
                n_turns
            );

            // 独立地从初始 last_consolidated=0 起，连续应用 pick_batch_boundary
            // max_rounds 次，得到第 max_rounds 批的结束下标作为参考边界。
            let mut reference = 0usize;
            for _ in 0..max_rounds {
                reference = pick_batch_boundary(&messages, reference, max_messages_per_batch);
            }

            // Property 16 核心：达上限后 last_consolidated 恰好等于第 max_rounds
            // 批的结束下标。
            prop_assert_eq!(
                result.last_consolidated,
                reference,
                "last_consolidated {} != reference end-of-max_rounds-th-batch {} (max_rounds={}, n_turns={})",
                result.last_consolidated,
                reference,
                max_rounds,
                n_turns
            );

            // 剩余消息保留为未合并：边界未推进到序列末尾。
            prop_assert!(
                result.last_consolidated < messages.len(),
                "remainder not kept: last_consolidated {} == len {} (max_rounds={}, n_turns={})",
                result.last_consolidated,
                messages.len(),
                max_rounds,
                n_turns
            );
        }
    }

    // --- Property-based test for batch-failure stop + remainder retention (Requirement 4.6) ---

    proptest! {
        #![proptest_config(ProptestConfig { cases: 100, ..Default::default() })]

        // Feature: execution-performance-optimization, Property 17: 分片失败停止后续并保留未合并
        #[test]
        fn prop_consolidation_failure_stops_and_keeps_remainder(
            // n_turns = 总 user turn 数（每个 turn = user + assistant，2 条）。
            // k = 失败批次序号（从 1 起），约束在 1..=n_turns 以保证第 k 批确实存在。
            (n_turns, k) in (1usize..=20)
                .prop_flat_map(|n| (Just(n), 1usize..=n)),
        ) {
            let messages = turns(n_turns);
            // 每批上限 2 条 = 恰好一个 user turn，使每批推进一个 turn、第 i 批即第 i 个 turn。
            let max_messages_per_batch = 2usize;
            // 批次上限恒大于总批次数（= n_turns），确保上限永远不是限制因素，
            // 第 k 批必然被处理到（失败才是唯一的停止原因）。
            let max_rounds = n_turns + 1;

            // 第 k 批失败，其余成功。
            let result = run_consolidation_batches(
                &messages,
                0,
                max_messages_per_batch,
                max_rounds,
                |idx, _batch| {
                    if idx == k {
                        BatchOutcome::Failure("boom".into())
                    } else {
                        BatchOutcome::Success
                    }
                },
            );

            // 仅第 k 批之前的批次成功推进。
            prop_assert_eq!(
                result.batches_processed,
                k - 1,
                "batches_processed {} != k-1 {} (n_turns={}, k={})",
                result.batches_processed,
                k - 1,
                n_turns,
                k
            );

            // 记录失败批次序号与原因（R4.6）。
            prop_assert_eq!(
                result.failed_batch.clone(),
                Some((k, "boom".to_string())),
                "failed_batch {:?} != Some(({}, \"boom\")) (n_turns={})",
                result.failed_batch,
                k,
                n_turns
            );

            // 独立计算第 k 批的起点：从 0 起连续应用 pick_batch_boundary (k-1) 次，
            // 即前 (k-1) 个成功批次的累计结束下标 = 第 k 批起点。k == 1 时保持初始 0。
            let mut reference = 0usize;
            for _ in 0..(k - 1) {
                reference = pick_batch_boundary(&messages, reference, max_messages_per_batch);
            }

            // last_consolidated 仅推进到第 k 批起点（失败批及其之后保留为未合并）。
            prop_assert_eq!(
                result.last_consolidated,
                reference,
                "last_consolidated {} != start-of-batch-k {} (n_turns={}, k={})",
                result.last_consolidated,
                reference,
                n_turns,
                k
            );

            // 失败批及其之后保留为未合并：边界未推进到序列末尾。
            prop_assert!(
                result.last_consolidated < messages.len(),
                "remainder not kept: last_consolidated {} == len {} (n_turns={}, k={})",
                result.last_consolidated,
                messages.len(),
                n_turns,
                k
            );
        }
    }
}
