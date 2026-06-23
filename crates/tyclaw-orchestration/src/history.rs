//! 历史消息处理：去重、裁剪、tool_call 配对修复。

use serde_json::Value;
use std::collections::{HashMap, HashSet};

/// 统一去重归一化策略：
/// 1) 压缩连续空白；2) 全部转小写。
/// 这样可以把"仅大小写或空白差异"的文本视为同一条。
pub(crate) fn normalize_text_for_dedupe(s: &str) -> String {
    s.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

/// 构造消息签名，用于 history 去重。
/// 当前按 role/name/content 维度做轻量去重，不依赖外部哈希库。
pub(crate) fn message_signature(message: &HashMap<String, Value>) -> String {
    let role = message.get("role").and_then(|v| v.as_str()).unwrap_or("");
    let content = message
        .get("content")
        .and_then(|v| v.as_str())
        .map(normalize_text_for_dedupe)
        .unwrap_or_default();
    let name = message.get("name").and_then(|v| v.as_str()).unwrap_or("");
    let tool_call_id = message
        .get("tool_call_id")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let tool_calls = message
        .get("tool_calls")
        .map(|v| serde_json::to_string(v).unwrap_or_default())
        .unwrap_or_default();
    format!("{role}|{name}|{tool_call_id}|{content}|{tool_calls}")
}

/// 对历史消息做"保守去重"：
/// - 仅去除连续重复消息
/// - 不做全局去重，避免打断 assistant(tool_calls) 与 tool(tool_call_id) 的配对关系
pub(crate) fn dedupe_history(history: &[HashMap<String, Value>]) -> Vec<HashMap<String, Value>> {
    let mut result = Vec::with_capacity(history.len());
    let mut last_sig = String::new();

    for msg in history {
        let sig = message_signature(msg);
        if sig == last_sig {
            continue;
        }
        last_sig = sig;
        result.push(msg.clone());
    }
    result
}

/// 按 token 预算裁剪历史：
/// 从最近消息向前回溯，尽量保留最新上下文。
///
/// **不变式（Property 31 / R11.2）**：
/// - 结果的累计估算 token 量（按 `estimate_message_tokens` 逐条求和）恒 `≤ budget_tokens`；
/// - 当预算允许时（即最近一条消息自身的估算 token 量 `≤ budget_tokens`），
///   结果至少保留该最近消息；
/// - 当预算连最近一条消息都容纳不下，或 `budget_tokens == 0`，或历史为空时，
///   返回空结果（空结果的估算 token 量为 0，依然满足 `≤ budget_tokens`）。
///
/// 注意：裁剪后可能留下 tool_result 没有对应 tool_call 的孤立消息，
/// 也可能留下 assistant(tool_calls) 而对应的 tool_result 被裁掉。
/// 前者由调用方随后执行的 `enforce_tool_call_pairing()` 清理；
/// 后者（孤立的 tool_calls）由 provider 层的 `ensure_tool_call_pairs`
/// 添加占位 tool_result 来修复。这是有意为之的分层设计。
pub fn trim_history_by_token_budget(
    history: &[HashMap<String, Value>],
    budget_tokens: usize,
) -> Vec<HashMap<String, Value>> {
    if history.is_empty() || budget_tokens == 0 {
        return Vec::new();
    }
    let mut total = 0usize;
    let mut selected_reversed: Vec<HashMap<String, Value>> = Vec::new();

    for msg in history.iter().rev() {
        let t = tyclaw_types::tokens::estimate_message_tokens(msg);
        // 一旦纳入当前消息会超出预算，则停止回溯。
        // 这保证结果累计估算 token 量恒 ≤ budget_tokens，包括最近一条
        // 消息自身就超预算的情形（此时返回空结果）。
        if total + t > budget_tokens {
            break;
        }
        total += t;
        selected_reversed.push(msg.clone());
    }

    selected_reversed.reverse();
    selected_reversed
}

/// 保障 tool 消息配对关系：
/// - tool 消息必须能在"紧邻之前的 assistant.tool_calls"里找到对应 id
/// - 不满足条件的 tool 消息会被丢弃，避免上游 provider（如 Anthropic）400
///
/// 注意：`add_tool_result` 处理图片时会在 tool 消息之间插入 user 消息（携带
/// image blocks），因此 user 消息不应清空 expected_tool_ids，否则同一轮后续
/// 的 tool result 会被误判为孤立消息而丢弃。
pub(crate) fn enforce_tool_call_pairing(
    history: &[HashMap<String, Value>],
) -> Vec<HashMap<String, Value>> {
    let mut cleaned = Vec::with_capacity(history.len());
    let mut expected_tool_ids: HashSet<String> = HashSet::new();

    for msg in history {
        let role = msg.get("role").and_then(|v| v.as_str()).unwrap_or("");

        if role == "assistant" {
            // 不清空 expected_tool_ids：连续的 assistant 消息（第一条带 tool_calls）
            // 如果清空，第二条 assistant 会导致第一条的 tool_results 被误丢弃。
            // 改为 extend：将当前 assistant 的 tool_call IDs 追加到已有集合中。
            if let Some(tool_calls) = msg.get("tool_calls").and_then(|v| v.as_array()) {
                for tc in tool_calls {
                    if let Some(id) = tc.get("id").and_then(|v| v.as_str()) {
                        expected_tool_ids.insert(id.to_string());
                    }
                }
            }
            cleaned.push(msg.clone());
            continue;
        }

        if role == "tool" {
            let tool_call_id = msg.get("tool_call_id").and_then(|v| v.as_str());
            if let Some(id) = tool_call_id {
                if expected_tool_ids.contains(id) {
                    cleaned.push(msg.clone());
                }
            }
            continue;
        }

        // user 消息不清空 expected_tool_ids：图片处理会在 tool 消息间
        // 插入 user 消息，清空会导致后续同一轮的 tool result 被丢弃。
        // 只有 system 等真正的轮次分隔消息才需要清空。
        if role != "user" {
            expected_tool_ids.clear();
        }
        cleaned.push(msg.clone());
    }

    cleaned
}

/// 给定保留窗口起始下标 `start`，向更早方向调整边界，使保留窗口
/// `messages[returned..]` 不以「孤立 tool_result」开头（R3.6/R3.7）。
///
/// 「孤立 tool_result」指：保留窗口最早一条消息为 role=="tool" 的消息——
/// 由于其对应的 assistant.tool_calls 必然位于该消息之前，一旦它成为窗口
/// 的第一条，就一定缺失对应 tool_call，从而触发上游 provider 400。
///
/// 行为：
/// - 若 `messages[start]` 不是 tool 消息（即窗口不以孤立 tool_result 开头），
///   直接返回 `start`，不做调整。
/// - 否则向更早方向回退至「最近一个完整 user 回合的起点」（最近的 user 消息），
///   返回该下标。
/// - 若回退过程中找不到 user 消息，则退回到最近的非 tool 消息，以保证返回的
///   消息不是孤立 tool_result；若连非 tool 消息都不存在，则返回 0。
///
/// 不变式：返回值恒 `<= start`。
pub fn adjust_truncation_boundary(messages: &[HashMap<String, Value>], start: usize) -> usize {
    if messages.is_empty() {
        return 0;
    }
    // 将越界的 start 收敛到合法范围内。
    let start = start.min(messages.len() - 1);

    let role_at = |idx: usize| -> &str {
        messages[idx]
            .get("role")
            .and_then(|v| v.as_str())
            .unwrap_or("")
    };

    // 仅当保留窗口最早一条是孤立 tool_result（role==tool）时才需要回退。
    if role_at(start) != "tool" {
        return start;
    }

    // 向更早方向回退：优先落在最近一个完整 user 回合的起点（user 消息）。
    // 同时记录最近的非 tool 消息作为兜底，避免在不存在 user 消息时仍以
    // 孤立 tool_result 开头。
    let mut nearest_non_tool: Option<usize> = None;
    let mut idx = start;
    while idx > 0 {
        idx -= 1;
        match role_at(idx) {
            "user" => return idx,
            "tool" => {}
            _ => {
                if nearest_non_tool.is_none() {
                    nearest_non_tool = Some(idx);
                }
            }
        }
    }

    nearest_non_tool.unwrap_or(0)
}

#[cfg(test)]
mod adjust_truncation_boundary_tests {
    use super::*;
    use proptest::prelude::*;
    use serde_json::json;

    fn msg(role: &str) -> HashMap<String, Value> {
        let mut m = HashMap::new();
        m.insert("role".to_string(), json!(role));
        m
    }

    fn tool_msg(tool_call_id: &str) -> HashMap<String, Value> {
        let mut m = HashMap::new();
        m.insert("role".to_string(), json!("tool"));
        m.insert("tool_call_id".to_string(), json!(tool_call_id));
        m
    }

    #[test]
    fn returns_start_when_window_begins_with_user() {
        // user, assistant, tool  —— start 指向 user，无需调整。
        let history = vec![msg("user"), msg("assistant"), tool_msg("call_1")];
        assert_eq!(adjust_truncation_boundary(&history, 0), 0);
    }

    #[test]
    fn returns_start_when_window_begins_with_assistant() {
        // assistant 不是孤立 tool_result，直接返回 start。
        let history = vec![msg("user"), msg("assistant"), tool_msg("call_1")];
        assert_eq!(adjust_truncation_boundary(&history, 1), 1);
    }

    #[test]
    fn retreats_to_nearest_user_turn_start_when_window_begins_with_orphan_tool() {
        // 索引: 0 user, 1 assistant, 2 tool, 3 tool
        // start=3（孤立 tool_result）→ 回退至最近 user 回合起点 0。
        let history = vec![
            msg("user"),
            msg("assistant"),
            tool_msg("call_1"),
            tool_msg("call_2"),
        ];
        assert_eq!(adjust_truncation_boundary(&history, 3), 0);
        assert_eq!(adjust_truncation_boundary(&history, 2), 0);
    }

    #[test]
    fn retreats_to_latest_user_turn_among_multiple_turns() {
        // turn A: 0 user, 1 assistant, 2 tool
        // turn B: 3 user, 4 assistant, 5 tool
        let history = vec![
            msg("user"),
            msg("assistant"),
            tool_msg("a"),
            msg("user"),
            msg("assistant"),
            tool_msg("b"),
        ];
        // start=5 为 turn B 的孤立 tool_result → 落在 turn B 起点 3。
        assert_eq!(adjust_truncation_boundary(&history, 5), 3);
    }

    #[test]
    fn falls_back_to_nearest_non_tool_when_no_user_present() {
        // 无 user 消息：system, assistant, tool
        let history = vec![msg("system"), msg("assistant"), tool_msg("call_1")];
        // start=2 为孤立 tool_result，无 user → 退回最近非 tool 消息（assistant，idx 1）。
        assert_eq!(adjust_truncation_boundary(&history, 2), 1);
    }

    #[test]
    fn handles_empty_and_out_of_bounds() {
        let empty: Vec<HashMap<String, Value>> = Vec::new();
        assert_eq!(adjust_truncation_boundary(&empty, 5), 0);

        let history = vec![msg("user"), tool_msg("call_1")];
        // start 越界收敛到 len-1=1（tool）→ 回退至 user 0。
        assert_eq!(adjust_truncation_boundary(&history, 99), 0);
    }

    #[test]
    fn returned_index_never_exceeds_start() {
        let history = vec![
            msg("user"),
            msg("assistant"),
            tool_msg("a"),
            msg("user"),
            tool_msg("b"),
        ];
        for start in 0..history.len() {
            assert!(adjust_truncation_boundary(&history, start) <= start);
        }
    }

    // 生成 role ∈ {user, assistant, tool} 的随机消息序列；tool 消息可有可无
    // 对应的 tool_call，这里只关心 role 标签是否为 "tool"。
    fn role_seq_strategy() -> impl Strategy<Value = Vec<String>> {
        let role = prop_oneof![
            Just("user".to_string()),
            Just("assistant".to_string()),
            Just("tool".to_string()),
        ];
        prop::collection::vec(role, 1..12)
    }

    fn build_messages(roles: &[String]) -> Vec<HashMap<String, Value>> {
        roles
            .iter()
            .enumerate()
            .map(|(i, role)| {
                if role == "tool" {
                    tool_msg(&format!("call_{i}"))
                } else {
                    msg(role)
                }
            })
            .collect()
    }

    fn role_at(messages: &[HashMap<String, Value>], idx: usize) -> &str {
        messages[idx]
            .get("role")
            .and_then(|v| v.as_str())
            .unwrap_or("")
    }

    proptest! {
        #![proptest_config(ProptestConfig { cases: 100, ..Default::default() })]

        // Feature: execution-performance-optimization, Property 11: 截断边界调整后不以孤立 tool_result 开头
        // Validates: Requirements 3.7
        #[test]
        fn prop_boundary_not_orphan_tool_result(
            roles in role_seq_strategy(),
            raw_start in 0usize..32,
        ) {
            let msgs = build_messages(&roles);
            // 在 0..len 内取一个合法 start。
            let start = raw_start % msgs.len();

            let r = adjust_truncation_boundary(&msgs, start);

            // 不变式 1：返回值恒 <= start。
            prop_assert!(r <= start, "returned index {} exceeds start {}", r, start);

            // 不变式 2：若 [0..=start] 中存在任意非 tool 消息，则返回位置不是孤立
            // tool_result（role != "tool"）；否则（前缀全为 tool）返回 0。
            let has_non_tool_before = (0..=start).any(|i| role_at(&msgs, i) != "tool");
            if has_non_tool_before {
                prop_assert_ne!(
                    role_at(&msgs, r),
                    "tool",
                    "boundary {} is an orphan tool_result despite a non-tool message existing in prefix [0..={}]",
                    r,
                    start
                );
            } else {
                prop_assert_eq!(r, 0, "prefix is all-tool but boundary {} != 0", r);
            }
        }
    }
}

#[cfg(test)]
mod trim_history_tests {
    use super::*;
    use proptest::prelude::*;
    use serde_json::json;
    use tyclaw_types::tokens::estimate_message_tokens;

    fn msg(role: &str, content: &str) -> HashMap<String, Value> {
        let mut m = HashMap::new();
        m.insert("role".to_string(), json!(role));
        m.insert("content".to_string(), json!(content));
        m
    }

    fn total_tokens(history: &[HashMap<String, Value>]) -> usize {
        history
            .iter()
            .map(tyclaw_types::tokens::estimate_message_tokens)
            .sum()
    }

    #[test]
    fn empty_history_returns_empty() {
        let history: Vec<HashMap<String, Value>> = Vec::new();
        assert!(trim_history_by_token_budget(&history, 1000).is_empty());
    }

    #[test]
    fn zero_budget_returns_empty() {
        let history = vec![msg("user", "hello")];
        assert!(trim_history_by_token_budget(&history, 0).is_empty());
    }

    #[test]
    fn result_never_exceeds_budget() {
        let history = vec![
            msg("user", "the quick brown fox jumps over the lazy dog"),
            msg("assistant", "a moderately long assistant reply with several words"),
            msg("user", "another user turn that adds to the running token total"),
            msg("assistant", "final assistant message in the conversation history"),
        ];
        // 对一系列预算逐一验证：结果累计 token 量恒 ≤ 预算。
        for budget in 1..=200 {
            let trimmed = trim_history_by_token_budget(&history, budget);
            assert!(
                total_tokens(&trimmed) <= budget,
                "budget {budget}: result tokens {} exceeded",
                total_tokens(&trimmed)
            );
        }
    }

    #[test]
    fn keeps_most_recent_message_when_budget_allows() {
        let history = vec![
            msg("user", "first"),
            msg("assistant", "second"),
            msg("user", "third"),
        ];
        // 预算足够容纳最近一条消息时，结果非空且包含最近一条。
        let last_tokens = tyclaw_types::tokens::estimate_message_tokens(history.last().unwrap());
        let trimmed = trim_history_by_token_budget(&history, last_tokens);
        assert!(!trimmed.is_empty());
        assert_eq!(trimmed.last().unwrap()["content"], json!("third"));
        assert!(total_tokens(&trimmed) <= last_tokens);
    }

    #[test]
    fn returns_empty_when_even_latest_message_exceeds_budget() {
        // 最近一条消息估算 token 量 > 预算 → 返回空（满足 ≤ 预算的不变式）。
        let big = msg("user", "this single message has clearly more than one token in it");
        let latest_tokens = tyclaw_types::tokens::estimate_message_tokens(&big);
        assert!(latest_tokens > 1, "test precondition: message must exceed 1 token");
        let history = vec![big];
        let trimmed = trim_history_by_token_budget(&history, 1);
        assert!(trimmed.is_empty());
        assert_eq!(total_tokens(&trimmed), 0);
    }

    #[test]
    fn large_budget_keeps_all_and_preserves_order() {
        let history = vec![
            msg("user", "alpha"),
            msg("assistant", "beta"),
            msg("user", "gamma"),
        ];
        let trimmed = trim_history_by_token_budget(&history, 100_000);
        assert_eq!(trimmed.len(), history.len());
        assert_eq!(trimmed[0]["content"], json!("alpha"));
        assert_eq!(trimmed[2]["content"], json!("gamma"));
    }

    // 生成一条随机消息：role ∈ {user, assistant, tool}，content 为长度不一的随机文本。
    fn message_strategy() -> impl Strategy<Value = HashMap<String, Value>> {
        let role = prop_oneof![
            Just("user".to_string()),
            Just("assistant".to_string()),
            Just("tool".to_string()),
        ];
        // 允许空内容到较长内容，覆盖单 token 与多 token 的情形。
        let content = "[ -~]{0,120}";
        (role, content).prop_map(|(r, c)| msg(&r, &c))
    }

    fn history_strategy() -> impl Strategy<Value = Vec<HashMap<String, Value>>> {
        prop::collection::vec(message_strategy(), 0..12)
    }

    proptest! {
        #![proptest_config(ProptestConfig { cases: 100, ..Default::default() })]

        // Feature: execution-performance-optimization, Property 31: 历史裁剪结果不超 token 预算
        // Validates: Requirements 11.2
        #[test]
        fn prop_trim_history_within_token_budget(
            history in history_strategy(),
            budget in 1usize..=5000,
        ) {
            let result = trim_history_by_token_budget(&history, budget);

            // (a) 结果累计估算 token 量恒 ≤ 预算。
            let total: usize = result.iter().map(estimate_message_tokens).sum();
            prop_assert!(
                total <= budget,
                "result tokens {} exceeded budget {}",
                total,
                budget
            );

            // (b) 结果是历史的后缀（保留最近消息），即等于等长尾部。
            prop_assert!(
                result.len() <= history.len(),
                "result longer ({}) than history ({})",
                result.len(),
                history.len()
            );
            let tail = &history[history.len() - result.len()..];
            prop_assert_eq!(
                result.as_slice(),
                tail,
                "result is not a suffix of history"
            );

            // (c) 预算允许时至少保留最近一条消息，且其为结果最后一条。
            if let Some(last) = history.last() {
                if estimate_message_tokens(last) <= budget {
                    prop_assert!(
                        !result.is_empty(),
                        "budget {} fits latest message but result is empty",
                        budget
                    );
                    prop_assert_eq!(
                        result.last().unwrap(),
                        last,
                        "result's last message differs from history's last"
                    );
                }
            }
        }
    }
}
