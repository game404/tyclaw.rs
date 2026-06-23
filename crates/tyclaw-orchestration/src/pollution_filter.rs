//! 会话历史污染识别（Pollution Filter）—— 纯函数核心。
//!
//! 在 fresh user turn 开始时，识别并剔除上一轮残留的失败兜底消息
//! （如 `I cannot make progress`），避免 LLM 复读失败文案造成无效请求和缓慢。
//!
//! 本模块刻意保持为纯函数，便于属性测试（见任务 2.2/2.4/2.5/2.7）。
//!
//! 关键词匹配语义（R1.7）：「完整短语」指关键词作为一个连续子串出现
//! （不区分大小写），而非要求整条消息等于关键词。例如关键词 `"blocked"`
//! 命中 `"task blocked by readonly fs"`。通过「短消息长度上限」与
//! 「连续子串匹配」双重约束控制误杀。
//!
//! 污染判定配置统一收敛到 `crate::config::PollutionConfig`
//! （隶属 `PerformanceConfig`），本模块直接复用该结构以保证单一事实来源。

use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};

pub use crate::config::PollutionConfig;

/// 占位 tool_result 内容中的可识别标识（R1.4）。
///
/// 凡因污染剔除而失去对应结果的 tool_call，其补齐的占位 tool_result
/// 正文均以该标识开头，便于上游与测试识别「已因污染剔除」。
pub const POLLUTION_PLACEHOLDER_MARKER: &str = "[pollution-removed]";

/// 提取消息正文文本（仅当 `content` 为字符串时返回其内容，否则空串）。
fn message_content_text(msg: &HashMap<String, Value>) -> &str {
    msg.get("content").and_then(|v| v.as_str()).unwrap_or("")
}

/// 判定文本是否以「不区分大小写的完整短语（连续子串）」方式命中任一污染关键词。
///
/// 关键词在比较前做 `trim`；空白关键词被忽略。
/// 供污染候选判定与子任务状态校正（任务 3.2）复用。
pub fn contains_pollution_phrase(text: &str, cfg: &PollutionConfig) -> bool {
    let lower = text.to_lowercase();
    cfg.keywords.iter().any(|kw| {
        let needle = kw.trim().to_lowercase();
        !needle.is_empty() && lower.contains(&needle)
    })
}

/// 判定单条消息是否为污染候选。
///
/// 当且仅当三条件**同时**满足时返回 `true`（R1.1 / R1.7）：
/// 1. `role == "tool"`；
/// 2. 正文字符长度 `<= short_message_max_chars`；
/// 3. 以不区分大小写的完整短语方式命中至少一个 `Pollution_Keyword`。
pub fn is_pollution_candidate(msg: &HashMap<String, Value>, cfg: &PollutionConfig) -> bool {
    // 条件一：role == tool
    let role = msg.get("role").and_then(|v| v.as_str()).unwrap_or("");
    if role != "tool" {
        return false;
    }

    // 条件二：字符长度（按 Unicode 字符计数）不超过短消息上限
    let content = message_content_text(msg);
    if content.chars().count() > cfg.short_message_max_chars {
        return false;
    }

    // 条件三：完整短语匹配关键词
    contains_pollution_phrase(content, cfg)
}

/// 污染剔除结果。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PollutionFilterResult {
    /// 剔除污染并补齐占位后的消息序列。
    pub cleaned: Vec<HashMap<String, Value>>,
    /// 被剔除的污染消息数量（含 0；供审计记录，见 R1.5）。
    pub removed_count: usize,
    /// 被补齐占位 tool_result 的 tool_call_id 列表（按出现顺序）。
    pub placeholder_ids: Vec<String>,
}

/// 构建一条占位 tool_result（保持与被剔除污染消息相同的 `tool_call_id` 与 `name`）。
///
/// 正文以 [`POLLUTION_PLACEHOLDER_MARKER`] 开头，标识该结果因污染被剔除。
fn build_placeholder(tool_call_id: &str, name: &str) -> HashMap<String, Value> {
    let mut placeholder = HashMap::new();
    placeholder.insert("role".to_string(), json!("tool"));
    placeholder.insert("tool_call_id".to_string(), json!(tool_call_id));
    placeholder.insert("name".to_string(), json!(name));
    placeholder.insert(
        "content".to_string(),
        json!(format!(
            "{POLLUTION_PLACEHOLDER_MARKER} 原工具结果因包含失败兜底文案，已在本回合被剔除。"
        )),
    );
    placeholder
}

/// 扫描历史并剔除污染 tool 消息，剔除后补齐占位 tool_result 以维持配对完整性。
///
/// 行为契约（R1.2 / R1.4 / R1.6）：
/// - 对每条满足 [`is_pollution_candidate`] 的 tool 消息执行剔除；
/// - 若被剔除消息的 `tool_call_id` 对应历史中某个 assistant 声明的 tool_call，
///   则在原位置补齐含 [`POLLUTION_PLACEHOLDER_MARKER`] 标识的占位 tool_result
///   （沿用相同 `tool_call_id` 与 `name`），从而该 tool_call 不会失去结果；
/// - 若被剔除消息本身为孤立 tool_result（无 `tool_call_id` 或其 id 未被任何
///   assistant tool_call 声明），则直接移除而不补齐（避免引入新的孤立结果）；
/// - 所有非污染消息原样保留，内容与相对顺序不变。
///
/// 返回 [`PollutionFilterResult`]，其 `removed_count` 计入全部被剔除的污染消息
/// （含 0），`placeholder_ids` 为被补齐占位的 tool_call_id 列表。
pub fn filter_pollution(
    history: &[HashMap<String, Value>],
    cfg: &PollutionConfig,
) -> PollutionFilterResult {
    // 收集历史中所有 assistant 声明的 tool_call id，用于判定被剔除的污染
    // tool_result 是否会令某个真实 tool_call 失去结果。
    let mut declared_ids: HashSet<String> = HashSet::new();
    for msg in history {
        if msg.get("role").and_then(|v| v.as_str()) == Some("assistant") {
            if let Some(tcs) = msg.get("tool_calls").and_then(|v| v.as_array()) {
                for tc in tcs {
                    if let Some(id) = tc.get("id").and_then(|v| v.as_str()) {
                        declared_ids.insert(id.to_string());
                    }
                }
            }
        }
    }

    let mut cleaned: Vec<HashMap<String, Value>> = Vec::with_capacity(history.len());
    let mut removed_count = 0usize;
    let mut placeholder_ids: Vec<String> = Vec::new();

    for msg in history {
        if !is_pollution_candidate(msg, cfg) {
            // 非污染消息：内容与相对顺序原样保留（R1.6）。
            cleaned.push(msg.clone());
            continue;
        }

        // 命中污染：剔除该消息。
        removed_count += 1;

        // 若该污染 tool_result 对应一个真实存在的 tool_call，则补齐占位，
        // 避免该 tool_call 失去结果（R1.4）。
        if let Some(id) = msg.get("tool_call_id").and_then(|v| v.as_str()) {
            if declared_ids.contains(id) {
                let name = msg.get("name").and_then(|v| v.as_str()).unwrap_or("unknown");
                cleaned.push(build_placeholder(id, name));
                placeholder_ids.push(id.to_string());
            }
        }
        // 否则（无 id 或为孤立 tool_result）：直接移除，不补齐占位。
    }

    PollutionFilterResult {
        cleaned,
        removed_count,
        placeholder_ids,
    }
}

/// 污染剔除审计记录（R1.5）。
///
/// 每次污染剔除恰好对应一条审计记录，携带本回合被剔除的污染消息数量
/// （`removed_count`，含 0）与所属 Workspace 标识（`workspace_key`），
/// 供可观测性与排障使用。
#[derive(Debug, Clone, PartialEq)]
pub struct PollutionAuditRecord {
    /// 所属 Workspace 标识。
    pub workspace_key: String,
    /// 本回合被剔除的污染消息数量（含 0）。
    pub removed_count: usize,
}

/// 由一次 [`filter_pollution`] 的结果构建恰好一条审计记录（R1.5）。
///
/// 返回的记录 `removed_count` 等于 `result.removed_count`（无污染时为 0 且仍产出记录），
/// 并携带传入的 `workspace_key`。该函数为纯函数：相同输入恒得相同记录，便于属性测试。
pub fn build_pollution_audit(
    workspace_key: &str,
    result: &PollutionFilterResult,
) -> PollutionAuditRecord {
    PollutionAuditRecord {
        workspace_key: workspace_key.to_string(),
        removed_count: result.removed_count,
    }
}

/// 便捷封装：对一段历史执行一次 [`filter_pollution`]，并产出恰好一条审计记录。
///
/// 「每次污染剔除恰好一条审计记录」这一不变式由本函数显式表达：
/// 它调用 `filter_pollution` 一次，再据其结果构建单条 [`PollutionAuditRecord`]。
/// 审计记录的 `removed_count` 与过滤结果完全一致（含 0），并携带 `workspace_key`。
pub fn filter_pollution_audited(
    history: &[HashMap<String, Value>],
    cfg: &PollutionConfig,
    workspace_key: &str,
) -> (PollutionFilterResult, PollutionAuditRecord) {
    let result = filter_pollution(history, cfg);
    let audit = build_pollution_audit(workspace_key, &result);
    (result, audit)
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use serde_json::json;

    fn tool_msg(content: &str) -> HashMap<String, Value> {
        let mut m = HashMap::new();
        m.insert("role".to_string(), json!("tool"));
        m.insert("content".to_string(), json!(content));
        m
    }

    fn msg(role: &str, content: &str) -> HashMap<String, Value> {
        let mut m = HashMap::new();
        m.insert("role".to_string(), json!(role));
        m.insert("content".to_string(), json!(content));
        m
    }

    #[test]
    fn default_config_matches_requirements() {
        let cfg = PollutionConfig::default();
        assert_eq!(
            cfg.keywords,
            vec![
                "I cannot make progress".to_string(),
                "error".to_string(),
                "blocked".to_string()
            ]
        );
        assert_eq!(cfg.short_message_max_chars, 512);
    }

    #[test]
    fn contains_phrase_is_case_insensitive() {
        let cfg = PollutionConfig::default();
        assert!(contains_pollution_phrase("I CANNOT make Progress now", &cfg));
        assert!(contains_pollution_phrase("task BLOCKED by readonly fs", &cfg));
        assert!(!contains_pollution_phrase("everything is fine", &cfg));
    }

    #[test]
    fn candidate_requires_tool_role() {
        let cfg = PollutionConfig::default();
        // 同样的污染正文，非 tool 角色不应判为候选
        assert!(is_pollution_candidate(&tool_msg("error occurred"), &cfg));
        assert!(!is_pollution_candidate(&msg("assistant", "error occurred"), &cfg));
        assert!(!is_pollution_candidate(&msg("user", "error occurred"), &cfg));
    }

    #[test]
    fn candidate_requires_short_message() {
        let cfg = PollutionConfig {
            keywords: vec!["error".to_string()],
            short_message_max_chars: 10,
        };
        assert!(is_pollution_candidate(&tool_msg("error"), &cfg));
        // 超过长度上限：即便含关键词也不判为候选
        let long = format!("error {}", "x".repeat(20));
        assert!(!is_pollution_candidate(&tool_msg(&long), &cfg));
    }

    #[test]
    fn candidate_requires_keyword_match() {
        let cfg = PollutionConfig::default();
        assert!(!is_pollution_candidate(&tool_msg("all good, finished"), &cfg));
    }

    #[test]
    fn length_counts_unicode_chars() {
        // 9 个中文字符，上限 10 时应允许；含关键词则判为候选
        let cfg = PollutionConfig {
            keywords: vec!["错误".to_string()],
            short_message_max_chars: 10,
        };
        assert!(is_pollution_candidate(&tool_msg("发生了一个错误的情况"), &cfg));
    }

    #[test]
    fn non_string_content_is_not_candidate() {
        let cfg = PollutionConfig::default();
        let mut m = HashMap::new();
        m.insert("role".to_string(), json!("tool"));
        m.insert("content".to_string(), json!([{"type": "image"}]));
        assert!(!is_pollution_candidate(&m, &cfg));
    }

    // ---- filter_pollution ----

    fn assistant_with_call(id: &str, name: &str) -> HashMap<String, Value> {
        let mut m = msg("assistant", "");
        m.insert(
            "tool_calls".to_string(),
            json!([{"id": id, "type": "function", "function": {"name": name, "arguments": "{}"}}]),
        );
        m
    }

    fn tool_result(id: &str, name: &str, content: &str) -> HashMap<String, Value> {
        let mut m = tool_msg(content);
        m.insert("tool_call_id".to_string(), json!(id));
        m.insert("name".to_string(), json!(name));
        m
    }

    #[test]
    fn pollution_tool_result_is_replaced_with_placeholder() {
        let cfg = PollutionConfig::default();
        let history = vec![
            msg("user", "do something"),
            assistant_with_call("call_1", "exec"),
            tool_result("call_1", "exec", "I cannot make progress on this task"),
        ];

        let res = filter_pollution(&history, &cfg);

        assert_eq!(res.removed_count, 1);
        assert_eq!(res.placeholder_ids, vec!["call_1".to_string()]);
        assert_eq!(res.cleaned.len(), 3);

        // 占位 tool_result 保留同一 tool_call_id 与 name，正文含标识。
        let placeholder = &res.cleaned[2];
        assert_eq!(placeholder["role"], "tool");
        assert_eq!(placeholder["tool_call_id"], "call_1");
        assert_eq!(placeholder["name"], "exec");
        let content = placeholder["content"].as_str().unwrap();
        assert!(content.contains(POLLUTION_PLACEHOLDER_MARKER));
        // 原污染文案不得保留。
        assert!(!content.contains("I cannot make progress"));
    }

    #[test]
    fn non_pollution_messages_preserved_in_order() {
        let cfg = PollutionConfig::default();
        let history = vec![
            msg("user", "hi"),
            assistant_with_call("call_ok", "read_file"),
            tool_result("call_ok", "read_file", "file contents are fine"),
            msg("assistant", "all good"),
        ];

        let res = filter_pollution(&history, &cfg);

        assert_eq!(res.removed_count, 0);
        assert!(res.placeholder_ids.is_empty());
        // 无污染：输出与输入逐项相等。
        assert_eq!(res.cleaned, history);
    }

    #[test]
    fn orphan_pollution_tool_result_removed_without_placeholder() {
        let cfg = PollutionConfig::default();
        // tool_call_id 没有对应的 assistant tool_call —— 孤立污染结果。
        let history = vec![
            msg("user", "hi"),
            tool_result("orphan_id", "exec", "error: something went wrong"),
        ];

        let res = filter_pollution(&history, &cfg);

        assert_eq!(res.removed_count, 1);
        assert!(res.placeholder_ids.is_empty());
        // 孤立污染结果被直接移除，不补齐占位。
        assert_eq!(res.cleaned.len(), 1);
        assert_eq!(res.cleaned[0]["role"], "user");
    }

    #[test]
    fn pairing_completeness_preserved_after_removal() {
        let cfg = PollutionConfig::default();
        let history = vec![
            msg("user", "go"),
            assistant_with_call("c1", "exec"),
            tool_result("c1", "exec", "blocked by readonly fs"),
            assistant_with_call("c2", "read_file"),
            tool_result("c2", "read_file", "valid output"),
        ];

        let res = filter_pollution(&history, &cfg);
        assert_eq!(res.removed_count, 1);

        // 每个 tool 消息都能在其之前的 assistant.tool_calls 中找到配对 id。
        let mut declared: HashSet<String> = HashSet::new();
        for m in &res.cleaned {
            match m.get("role").and_then(|v| v.as_str()) {
                Some("assistant") => {
                    if let Some(tcs) = m.get("tool_calls").and_then(|v| v.as_array()) {
                        for tc in tcs {
                            if let Some(id) = tc.get("id").and_then(|v| v.as_str()) {
                                declared.insert(id.to_string());
                            }
                        }
                    }
                }
                Some("tool") => {
                    let id = m.get("tool_call_id").and_then(|v| v.as_str()).unwrap();
                    assert!(declared.contains(id), "orphan tool_result {id} in cleaned");
                }
                _ => {}
            }
        }
    }

    #[test]
    fn removed_count_zero_when_no_pollution() {
        let cfg = PollutionConfig::default();
        let history = vec![msg("user", "hi"), msg("assistant", "hello")];
        let res = filter_pollution(&history, &cfg);
        assert_eq!(res.removed_count, 0);
        assert_eq!(res.cleaned, history);
    }

    // ---- 污染剔除审计记录（R1.5）----

    #[test]
    fn audit_record_carries_removed_count_and_workspace() {
        let cfg = PollutionConfig::default();
        let history = vec![
            msg("user", "do something"),
            assistant_with_call("call_1", "exec"),
            tool_result("call_1", "exec", "I cannot make progress on this task"),
        ];
        let res = filter_pollution(&history, &cfg);
        let audit = build_pollution_audit("ws-42", &res);

        assert_eq!(audit.removed_count, res.removed_count);
        assert_eq!(audit.removed_count, 1);
        assert_eq!(audit.workspace_key, "ws-42");
    }

    #[test]
    fn audit_record_produced_even_when_removed_count_zero() {
        let cfg = PollutionConfig::default();
        let history = vec![msg("user", "hi"), msg("assistant", "all good")];
        let res = filter_pollution(&history, &cfg);
        let audit = build_pollution_audit("ws-zero", &res);

        // 无污染时仍产出一条记录，且 removed_count == 0。
        assert_eq!(res.removed_count, 0);
        assert_eq!(audit.removed_count, 0);
        assert_eq!(audit.workspace_key, "ws-zero");
    }

    #[test]
    fn build_pollution_audit_is_deterministic() {
        let result = PollutionFilterResult {
            cleaned: vec![],
            removed_count: 3,
            placeholder_ids: vec![],
        };
        let a = build_pollution_audit("ws", &result);
        let b = build_pollution_audit("ws", &result);
        assert_eq!(a, b);
        assert_eq!(
            a,
            PollutionAuditRecord {
                workspace_key: "ws".to_string(),
                removed_count: 3,
            }
        );
    }

    #[test]
    fn filter_pollution_audited_produces_single_consistent_record() {
        let cfg = PollutionConfig::default();
        let history = vec![
            msg("user", "go"),
            assistant_with_call("c1", "exec"),
            tool_result("c1", "exec", "blocked by readonly fs"),
            assistant_with_call("c2", "read_file"),
            tool_result("c2", "read_file", "valid output"),
        ];

        let (res, audit) = filter_pollution_audited(&history, &cfg, "ws-1");

        // 便捷封装的审计记录与过滤结果完全一致。
        assert_eq!(audit.removed_count, res.removed_count);
        assert_eq!(audit.removed_count, 1);
        assert_eq!(audit.workspace_key, "ws-1");

        // 与单独调用 filter_pollution 等价。
        let standalone = filter_pollution(&history, &cfg);
        assert_eq!(res, standalone);
    }

    // ---- 属性测试 ----

    /// 角色生成器：覆盖三类有意义角色与随机角色。
    fn role_strategy() -> impl Strategy<Value = String> {
        prop_oneof![
            Just("tool".to_string()),
            Just("assistant".to_string()),
            Just("user".to_string()),
            "[a-z]{1,6}",
        ]
    }

    /// 正文生成器：混合命中关键词的片段、近似词（如 `errorless`）、
    /// 大小写变体与任意短文本，长度在短消息上限边界附近浮动。
    fn content_strategy() -> impl Strategy<Value = String> {
        let token = prop_oneof![
            Just("error".to_string()),
            Just("ERROR".to_string()),
            Just("Error".to_string()),
            Just("errorless".to_string()),   // 近似词：包含 error 子串
            Just("blocked".to_string()),
            Just("BLOCKED".to_string()),
            Just("unblocked".to_string()),    // 近似词：包含 blocked 子串
            Just("I cannot make progress".to_string()),
            Just("i CANNOT make PROGRESS".to_string()),
            Just("all good, finished".to_string()),
            Just("发生了一个错误".to_string()), // 非 ASCII，验证按字符计数
            "[a-zA-Z ]{0,16}",
        ];
        prop::collection::vec(token, 0..6).prop_map(|parts| parts.join(" "))
    }

    /// 关键词集合生成器：含空白关键词（应被忽略）、首尾空格（应被 trim）。
    fn keywords_strategy() -> impl Strategy<Value = Vec<String>> {
        let kw = prop_oneof![
            Just("error".to_string()),
            Just("blocked".to_string()),
            Just("I cannot make progress".to_string()),
            Just(" error ".to_string()),
            Just("错误".to_string()),
            Just("".to_string()),
            Just("   ".to_string()),
            Just("done".to_string()),
        ];
        prop::collection::vec(kw, 0..4)
    }

    /// 独立参考实现：三条件合取（role==tool ∧ 字符长度 ≤ 上限 ∧ 不区分大小写完整短语命中）。
    fn reference_candidate(role: &str, content: &str, cfg: &PollutionConfig) -> bool {
        let cond_role = role == "tool";
        let cond_len = content.chars().count() <= cfg.short_message_max_chars;
        let lower = content.to_lowercase();
        let cond_kw = cfg.keywords.iter().any(|kw| {
            let needle = kw.trim().to_lowercase();
            !needle.is_empty() && lower.contains(&needle)
        });
        cond_role && cond_len && cond_kw
    }

    proptest! {
        #![proptest_config(ProptestConfig { cases: 100, ..Default::default() })]

        // Feature: execution-performance-optimization, Property 1: 污染候选判定等价于三条件合取
        // Validates: Requirements 1.1, 1.7
        #[test]
        fn prop_candidate_equiv_three_condition_conjunction(
            role in role_strategy(),
            content in content_strategy(),
            keywords in keywords_strategy(),
            short_message_max_chars in 0usize..=40,
        ) {
            let cfg = PollutionConfig { keywords, short_message_max_chars };
            let mut m = HashMap::new();
            m.insert("role".to_string(), json!(role));
            m.insert("content".to_string(), json!(content));

            let actual = is_pollution_candidate(&m, &cfg);
            let expected = reference_candidate(&role, &content, &cfg);
            prop_assert_eq!(actual, expected);
        }
    }

    // ---- Property 2 配对完整性与占位补齐 ----

    /// 历史构件：用于生成「混合配对/孤立、污染/干净」的随机历史。
    #[derive(Debug, Clone)]
    enum HistItem {
        /// 普通 user 消息（不参与配对）。
        User(String),
        /// 普通 assistant 文本消息（不声明 tool_call）。
        Assistant(String),
        /// 一个完整配对：assistant 声明 tool_call + 对应 tool_result。
        /// `pollution` 决定 tool_result 正文是否命中污染关键词。
        Call { pollution: bool },
        /// 孤立污染 tool_result：其 tool_call_id 未被任何 assistant 声明，
        /// 且正文命中污染关键词（因而会被剔除，不引入新孤立结果）。
        OrphanPollution,
    }

    fn hist_item_strategy() -> impl Strategy<Value = HistItem> {
        prop_oneof![
            "[a-z ]{0,12}".prop_map(HistItem::User),
            "[a-z ]{0,12}".prop_map(HistItem::Assistant),
            any::<bool>().prop_map(|pollution| HistItem::Call { pollution }),
            Just(HistItem::OrphanPollution),
        ]
    }

    /// 将构件序列物化为消息序列；按下标分配唯一 tool_call_id，
    /// 配对组用 `call_*` 前缀、孤立结果用 `orphan_*` 前缀以保证 id 不冲突。
    fn build_history(items: &[HistItem]) -> Vec<HashMap<String, Value>> {
        let mut h = Vec::new();
        for (i, it) in items.iter().enumerate() {
            match it {
                HistItem::User(c) => h.push(msg("user", c)),
                HistItem::Assistant(c) => h.push(msg("assistant", c)),
                HistItem::Call { pollution } => {
                    let id = format!("call_{i}");
                    h.push(assistant_with_call(&id, "exec"));
                    // 污染正文命中默认关键词；干净正文不含任何关键词。
                    let content = if *pollution {
                        if i % 2 == 0 { "I cannot make progress" } else { "blocked by readonly fs" }
                    } else {
                        "valid clean output data"
                    };
                    h.push(tool_result(&id, "exec", content));
                }
                HistItem::OrphanPollution => {
                    // id 不会被任何 assistant 声明（orphan_ 前缀），且为污染 → 应被直接剔除。
                    let id = format!("orphan_{i}");
                    h.push(tool_result(&id, "exec", "error occurred"));
                }
            }
        }
        h
    }

    proptest! {
        #![proptest_config(ProptestConfig { cases: 100, ..Default::default() })]

        // Feature: execution-performance-optimization, Property 2: 污染剔除保持配对完整性并补齐占位
        // Validates: Requirements 1.2, 1.4
        #[test]
        fn prop_pairing_completeness_and_placeholder_backfill(
            items in prop::collection::vec(hist_item_strategy(), 0..12),
        ) {
            let cfg = PollutionConfig::default();
            let history = build_history(&items);

            // 独立参考：收集输入中所有 assistant 声明的 tool_call id。
            let mut declared: HashSet<String> = HashSet::new();
            for m in &history {
                if m.get("role").and_then(|v| v.as_str()) == Some("assistant") {
                    if let Some(tcs) = m.get("tool_calls").and_then(|v| v.as_array()) {
                        for tc in tcs {
                            if let Some(id) = tc.get("id").and_then(|v| v.as_str()) {
                                declared.insert(id.to_string());
                            }
                        }
                    }
                }
            }

            // 独立参考：因污染剔除且其 id 被 assistant 声明 → 应补齐占位（按出现顺序）。
            let mut expected_placeholder_ids: Vec<String> = Vec::new();
            for m in &history {
                if is_pollution_candidate(m, &cfg) {
                    if let Some(id) = m.get("tool_call_id").and_then(|v| v.as_str()) {
                        if declared.contains(id) {
                            expected_placeholder_ids.push(id.to_string());
                        }
                    }
                }
            }

            let res = filter_pollution(&history, &cfg);

            // (b) placeholder_ids 与独立参考一致。
            prop_assert_eq!(&res.placeholder_ids, &expected_placeholder_ids);

            // (b) 每个失去结果的已声明 tool_call，cleaned 中存在含标识的占位 tool_result。
            for id in &expected_placeholder_ids {
                let has_placeholder = res.cleaned.iter().any(|m| {
                    m.get("tool_call_id").and_then(|v| v.as_str()) == Some(id.as_str())
                        && m.get("content")
                            .and_then(|v| v.as_str())
                            .map(|c| c.contains(POLLUTION_PLACEHOLDER_MARKER))
                            .unwrap_or(false)
                });
                prop_assert!(has_placeholder, "missing placeholder for tool_call {}", id);
            }

            // (a) cleaned 中无孤立 tool_result：每条 tool 消息的 tool_call_id
            //     都能在其之前的 assistant.tool_calls id 中找到配对。
            let mut declared_in_cleaned: HashSet<String> = HashSet::new();
            for m in &res.cleaned {
                match m.get("role").and_then(|v| v.as_str()) {
                    Some("assistant") => {
                        if let Some(tcs) = m.get("tool_calls").and_then(|v| v.as_array()) {
                            for tc in tcs {
                                if let Some(id) = tc.get("id").and_then(|v| v.as_str()) {
                                    declared_in_cleaned.insert(id.to_string());
                                }
                            }
                        }
                    }
                    Some("tool") => {
                        let id = m
                            .get("tool_call_id")
                            .and_then(|v| v.as_str())
                            .expect("tool message must carry tool_call_id");
                        prop_assert!(
                            declared_in_cleaned.contains(id),
                            "orphan tool_result {} in cleaned",
                            id
                        );
                    }
                    _ => {}
                }
            }
        }
    }

    // ---- Property 3 非污染消息内容与顺序原样保留 ----

    proptest! {
        #![proptest_config(ProptestConfig { cases: 100, ..Default::default() })]

        // Feature: execution-performance-optimization, Property 3: 非污染消息内容与顺序原样保留
        // Validates: Requirements 1.6
        #[test]
        fn prop_non_pollution_messages_preserved_verbatim(
            items in prop::collection::vec(hist_item_strategy(), 0..12),
        ) {
            let cfg = PollutionConfig::default();
            let history = build_history(&items);

            // 参考：输入中所有「非污染候选」消息的子序列（内容与相对顺序）。
            let reference: Vec<&HashMap<String, Value>> = history
                .iter()
                .filter(|m| !is_pollution_candidate(m, &cfg))
                .collect();

            let res = filter_pollution(&history, &cfg);

            // 实际：从 cleaned 中剔除占位 tool_result（正文含污染剔除标识）后的子序列。
            let actual: Vec<&HashMap<String, Value>> = res
                .cleaned
                .iter()
                .filter(|m| {
                    let is_placeholder = m
                        .get("content")
                        .and_then(|v| v.as_str())
                        .map(|c| c.contains(POLLUTION_PLACEHOLDER_MARKER))
                        .unwrap_or(false);
                    !is_placeholder
                })
                .collect();

            // 两个子序列必须逐项相等（内容与相对顺序均不变）。
            prop_assert_eq!(actual.len(), reference.len());
            for (a, r) in actual.iter().zip(reference.iter()) {
                prop_assert_eq!(a, r);
            }
        }
    }

    // ---- Property 4 污染剔除审计计数准确（含 0）----

    proptest! {
        #![proptest_config(ProptestConfig { cases: 100, ..Default::default() })]

        // Feature: execution-performance-optimization, Property 4: 污染剔除审计计数准确（含 0）
        // Validates: Requirements 1.5
        #[test]
        fn prop_audit_count_accurate_including_zero(
            items in prop::collection::vec(hist_item_strategy(), 0..12),
            ws_key in "[a-z0-9_]{1,16}",
        ) {
            let cfg = PollutionConfig::default();
            let history = build_history(&items);

            // 独立参考：直接统计输入中满足污染候选判定的消息数量（无污染时为 0）。
            let expected_removed = history
                .iter()
                .filter(|m| is_pollution_candidate(m, &cfg))
                .count();

            // filter_pollution_audited 调用 filter_pollution 恰好一次，
            // 并据其结果构建恰好一条审计记录（其单一返回值即「恰好一条」的体现）。
            let (result, audit) = filter_pollution_audited(&history, &cfg, &ws_key);

            // 审计计数 == 过滤结果计数 == 独立统计的实际剔除数（含 0）。
            prop_assert_eq!(audit.removed_count, result.removed_count);
            prop_assert_eq!(audit.removed_count, expected_removed);

            // 审计记录携带正确的 Workspace 标识。
            prop_assert_eq!(&audit.workspace_key, &ws_key);
        }
    }
}
