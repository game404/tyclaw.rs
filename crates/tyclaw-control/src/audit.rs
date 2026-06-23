//! 全局审计日志系统 —— 按天分文件，JSON Lines 格式。
//!
//! 所有 workspace 的审计记录写入同一个目录，按日期分文件：
//! `{audit_dir}/2026-04-04.jsonl`
//! 每条记录包含 workspace_key 和 session_id，便于按维度查询。

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

/// 审计日志条目。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditEntry {
    pub timestamp: DateTime<Utc>,
    pub workspace_key: String,
    pub session_id: String,
    pub user_id: String,
    #[serde(default)]
    pub user_name: String,
    pub channel: String,
    pub request: String,
    pub tool_calls: Vec<serde_json::Value>,
    /// 本次请求中调用的 skill 列表（从 exec 命令中自动提取）。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skills_used: Vec<serde_json::Value>,
    pub final_response: Option<String>,
    pub total_duration: Option<f64>,
    pub token_usage: Option<serde_json::Value>,
}

/// 失败原因码 —— 用于失败结果归类与可观测性统计（R14.1）。
///
/// 序列化为 snake_case 字符串，取值覆盖需求约定的集合：
/// `hit_max_iterations` / `sse_timeout` / `pollution_replay` /
/// `subtask_timeout` / `empty_result` / `readonly_path` / `config_missing`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureCode {
    HitMaxIterations,
    SseTimeout,
    PollutionReplay,
    SubtaskTimeout,
    EmptyResult,
    ReadonlyPath,
    ConfigMissing,
}

impl FailureCode {
    /// 原因码对应的 snake_case 字符串（R14.1）。
    ///
    /// 与 serde 序列化保持一致，便于在审计、告警打点等场景直接取字符串。
    pub fn to_str(self) -> &'static str {
        match self {
            FailureCode::HitMaxIterations => "hit_max_iterations",
            FailureCode::SseTimeout => "sse_timeout",
            FailureCode::PollutionReplay => "pollution_replay",
            FailureCode::SubtaskTimeout => "subtask_timeout",
            FailureCode::EmptyResult => "empty_result",
            FailureCode::ReadonlyPath => "readonly_path",
            FailureCode::ConfigMissing => "config_missing",
        }
    }

    /// 优先级序号，数值越小优先级越高。
    ///
    /// SSE 超时优先于撞迭代上限（R14.3）：同一 turn 同时命中时返回 `SseTimeout`。
    fn priority_rank(self) -> u8 {
        match self {
            FailureCode::SseTimeout => 0,
            FailureCode::PollutionReplay => 1,
            FailureCode::SubtaskTimeout => 2,
            FailureCode::ConfigMissing => 3,
            FailureCode::ReadonlyPath => 4,
            FailureCode::EmptyResult => 5,
            // 撞迭代上限往往是上述底层问题的表象，优先级最低。
            FailureCode::HitMaxIterations => 6,
        }
    }
}

impl std::fmt::Display for FailureCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.to_str())
    }
}

/// 在同一 turn 命中的多个原因码中解析出应上报的原因码。
///
/// 当集合同时包含 `SseTimeout` 与 `HitMaxIterations` 时返回 `SseTimeout`（R14.3）。
/// 更一般地，返回 `priority_rank` 最小（优先级最高）的原因码。
/// 入参为空时回退到 `HitMaxIterations`。
pub fn resolve_priority(codes: &[FailureCode]) -> FailureCode {
    codes
        .iter()
        .copied()
        .min_by_key(|c| c.priority_rank())
        .unwrap_or(FailureCode::HitMaxIterations)
}

/// 失败审计记录 —— 失败结果附带原因码写入审计（R14.1）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FailureAuditEntry {
    pub workspace_key: String,
    pub failure_code: FailureCode,
    pub turn_id: String,
    pub duration_ms: u64,
}

/// 统计慢请求的原因码分布（R14.4）。
///
/// 在所有失败审计记录中，仅统计 `duration_ms >= slow_threshold_secs * 1000` 的「慢请求」，
/// 并按 `failure_code` 聚合计数。阈值边界采用 `>=`（恰好等于阈值视为慢请求）。
///
/// 纯函数，无副作用，便于单元测试。返回各原因码出现次数的映射。
pub fn slow_request_reason_distribution(
    entries: &[FailureAuditEntry],
    slow_threshold_secs: u64,
) -> HashMap<FailureCode, usize> {
    let threshold_ms = slow_threshold_secs.saturating_mul(1000);
    let mut dist: HashMap<FailureCode, usize> = HashMap::new();
    for entry in entries {
        if entry.duration_ms >= threshold_ms {
            *dist.entry(entry.failure_code).or_insert(0) += 1;
        }
    }
    dist
}

/// 以 WARN 级别记录 `max_iterations` 重置事件并附带 Workspace 标识（R14.2）。
///
/// Agent_Loop 因命中 `max_iterations` 而重置迭代计数器时应调用本函数，
/// 以便该事件触发 WARN 级别告警打点。
pub fn warn_max_iterations_reset(workspace_key: &str) {
    tracing::warn!(
        workspace_key = %workspace_key,
        event = "max_iterations_reset",
        "Agent_Loop 命中 max_iterations，已重置迭代计数器"
    );
}

/// 全局审计日志管理器 —— 按天分文件追加写入。
///
/// 存储结构：`{audit_dir}/YYYY-MM-DD.jsonl`
pub struct AuditLog {
    audit_dir: PathBuf,
}

impl AuditLog {
    pub fn new(audit_dir: impl AsRef<Path>) -> Self {
        Self {
            audit_dir: audit_dir.as_ref().to_path_buf(),
        }
    }

    /// 当天的审计日志文件路径。
    fn today_file(&self) -> PathBuf {
        let date = Utc::now().format("%Y-%m-%d").to_string();
        self.audit_dir.join(format!("{date}.jsonl"))
    }

    /// 指定日期的审计日志文件路径。
    fn date_file(&self, date: &str) -> PathBuf {
        self.audit_dir.join(format!("{date}.jsonl"))
    }

    /// 追加一条审计日志。
    pub fn log(&self, entry: &AuditEntry) -> Result<(), std::io::Error> {
        let path = self.today_file();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut file = OpenOptions::new().create(true).append(true).open(&path)?;
        let line = serde_json::to_string(entry)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        writeln!(file, "{line}")?;
        Ok(())
    }

    /// 查询审计日志。
    ///
    /// 支持按 workspace_key、user_id 过滤，限制返回条数。
    /// `date` 指定查询哪天的日志（格式 "YYYY-MM-DD"），None 查当天。
    pub fn query(
        &self,
        date: Option<&str>,
        workspace_key: Option<&str>,
        user_id: Option<&str>,
        limit: usize,
    ) -> Vec<AuditEntry> {
        let path = match date {
            Some(d) => self.date_file(d),
            None => self.today_file(),
        };
        let content = match fs::read_to_string(&path) {
            Ok(c) => c,
            Err(_) => return Vec::new(),
        };

        let mut entries: Vec<AuditEntry> = content
            .lines()
            .filter_map(|line| serde_json::from_str(line).ok())
            .filter(|e: &AuditEntry| {
                workspace_key.map_or(true, |wk| e.workspace_key == wk)
                    && user_id.map_or(true, |uid| e.user_id == uid)
            })
            .collect();

        entries.reverse();
        entries.truncate(limit);
        entries
    }

    /// 当天的失败审计日志文件路径。
    fn today_failure_file(&self) -> PathBuf {
        let date = Utc::now().format("%Y-%m-%d").to_string();
        self.audit_dir.join(format!("{date}.failures.jsonl"))
    }

    /// 指定日期的失败审计日志文件路径。
    fn date_failure_file(&self, date: &str) -> PathBuf {
        self.audit_dir.join(format!("{date}.failures.jsonl"))
    }

    /// 追加一条失败审计记录（R14.1）。
    ///
    /// 失败结果附带原因码写入按天分文件的失败审计日志，复用与 [`log`](Self::log)
    /// 一致的逐行 JSON Lines 追加机制，但写入独立的 `*.failures.jsonl` 文件，
    /// 避免与常规审计记录混淆 schema。
    pub fn log_failure(&self, entry: &FailureAuditEntry) -> Result<(), std::io::Error> {
        let path = self.today_failure_file();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut file = OpenOptions::new().create(true).append(true).open(&path)?;
        let line = serde_json::to_string(entry)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        writeln!(file, "{line}")?;
        Ok(())
    }

    /// 查询失败审计记录。
    ///
    /// `date` 指定查询哪天的失败日志（格式 "YYYY-MM-DD"），None 查当天；
    /// 可选按 `workspace_key` 过滤。返回的记录可直接喂给
    /// [`slow_request_reason_distribution`] 计算慢请求原因码分布（R14.4）。
    pub fn query_failures(
        &self,
        date: Option<&str>,
        workspace_key: Option<&str>,
    ) -> Vec<FailureAuditEntry> {
        let path = match date {
            Some(d) => self.date_failure_file(d),
            None => self.today_failure_file(),
        };
        let content = match fs::read_to_string(&path) {
            Ok(c) => c,
            Err(_) => return Vec::new(),
        };
        content
            .lines()
            .filter_map(|line| serde_json::from_str(line).ok())
            .filter(|e: &FailureAuditEntry| {
                workspace_key.map_or(true, |wk| e.workspace_key == wk)
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn make_entry(workspace_key: &str, user_id: &str) -> AuditEntry {
        AuditEntry {
            timestamp: Utc::now(),
            workspace_key: workspace_key.into(),
            session_id: "s_test_001".into(),
            user_id: user_id.into(),
            user_name: "test_user".into(),
            channel: "cli".into(),
            request: "test request".into(),
            tool_calls: vec![],
            skills_used: vec![],
            final_response: Some("done".into()),
            total_duration: Some(1.5),
            token_usage: None,
        }
    }

    #[test]
    fn test_log_and_query() {
        let tmp = TempDir::new().unwrap();
        let log = AuditLog::new(tmp.path());
        log.log(&make_entry("alice", "user_a")).unwrap();
        log.log(&make_entry("alice", "user_b")).unwrap();
        log.log(&make_entry("bob", "user_a")).unwrap();

        // 查全部
        let all = log.query(None, None, None, 100);
        assert_eq!(all.len(), 3);

        // 按 workspace_key 过滤
        let alice = log.query(None, Some("alice"), None, 100);
        assert_eq!(alice.len(), 2);

        // 按 user_id 过滤
        let user_a = log.query(None, None, Some("user_a"), 100);
        assert_eq!(user_a.len(), 2);

        // 组合过滤
        let alice_a = log.query(None, Some("alice"), Some("user_a"), 100);
        assert_eq!(alice_a.len(), 1);
    }

    #[test]
    fn test_query_empty() {
        let tmp = TempDir::new().unwrap();
        let log = AuditLog::new(tmp.path());
        let result = log.query(None, None, None, 100);
        assert!(result.is_empty());
    }

    #[test]
    fn test_resolve_priority_sse_over_max_iterations() {
        // 同时命中 SSE 超时与撞迭代上限时，应返回 SseTimeout（R14.3）。
        let codes = [FailureCode::HitMaxIterations, FailureCode::SseTimeout];
        assert_eq!(resolve_priority(&codes), FailureCode::SseTimeout);
        // 顺序无关
        let codes_rev = [FailureCode::SseTimeout, FailureCode::HitMaxIterations];
        assert_eq!(resolve_priority(&codes_rev), FailureCode::SseTimeout);
    }

    #[test]
    fn test_resolve_priority_empty_falls_back() {
        assert_eq!(resolve_priority(&[]), FailureCode::HitMaxIterations);
    }

    use proptest::prelude::*;

    /// 预定义合法原因码集合（R14.1）。
    const VALID_FAILURE_CODES: [&str; 7] = [
        "hit_max_iterations",
        "sse_timeout",
        "pollution_replay",
        "subtask_timeout",
        "empty_result",
        "readonly_path",
        "config_missing",
    ];

    /// 生成任意 FailureCode 变体的 proptest 策略。
    fn arb_failure_code() -> impl Strategy<Value = FailureCode> {
        prop_oneof![
            Just(FailureCode::HitMaxIterations),
            Just(FailureCode::SseTimeout),
            Just(FailureCode::PollutionReplay),
            Just(FailureCode::SubtaskTimeout),
            Just(FailureCode::EmptyResult),
            Just(FailureCode::ReadonlyPath),
            Just(FailureCode::ConfigMissing),
        ]
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(100))]

        // Feature: execution-performance-optimization, Property 35: 失败审计始终携带合法原因码
        // Validates: Requirements 14.1
        #[test]
        fn prop_failure_audit_always_carries_valid_code(
            code in arb_failure_code(),
            workspace_key in "[a-z0-9_]{1,16}",
            turn_id in "[a-z0-9_]{1,16}",
            duration_ms in 0u64..1_000_000,
        ) {
            let entry = FailureAuditEntry {
                workspace_key,
                failure_code: code,
                turn_id,
                duration_ms,
            };
            let code_str = entry.failure_code.to_str();
            // 非空
            prop_assert!(!code_str.is_empty());
            // 属于预定义集合
            prop_assert!(
                VALID_FAILURE_CODES.contains(&code_str),
                "failure_code {:?} not in predefined set",
                code_str
            );
        }

        // Feature: execution-performance-optimization, Property 36: SSE 超时优先于撞顶
        // Validates: Requirements 14.3
        #[test]
        fn prop_sse_timeout_takes_priority_over_hit_max_iterations(
            mut codes in prop::collection::vec(arb_failure_code(), 0..8),
        ) {
            // 确保集合同时包含 SseTimeout 与 HitMaxIterations。
            codes.push(FailureCode::SseTimeout);
            codes.push(FailureCode::HitMaxIterations);
            // 无论其他原因码如何组合，SSE 超时优先级最高（R14.3）。
            prop_assert_eq!(resolve_priority(&codes), FailureCode::SseTimeout);
        }
    }

    #[test]
    fn test_failure_code_to_str() {
        // 字符串取值需覆盖需求约定的集合（R14.1）。
        assert_eq!(FailureCode::HitMaxIterations.to_str(), "hit_max_iterations");
        assert_eq!(FailureCode::SseTimeout.to_str(), "sse_timeout");
        assert_eq!(FailureCode::PollutionReplay.to_str(), "pollution_replay");
        assert_eq!(FailureCode::SubtaskTimeout.to_str(), "subtask_timeout");
        assert_eq!(FailureCode::EmptyResult.to_str(), "empty_result");
        assert_eq!(FailureCode::ReadonlyPath.to_str(), "readonly_path");
        assert_eq!(FailureCode::ConfigMissing.to_str(), "config_missing");
        // Display 与 to_str 一致
        assert_eq!(FailureCode::SseTimeout.to_string(), "sse_timeout");
    }

    fn make_failure(workspace_key: &str, code: FailureCode, duration_ms: u64) -> FailureAuditEntry {
        FailureAuditEntry {
            workspace_key: workspace_key.into(),
            failure_code: code,
            turn_id: "turn_test".into(),
            duration_ms,
        }
    }

    #[test]
    fn test_slow_request_distribution_counts_only_slow_entries() {
        // 阈值 120s => 120_000ms。仅统计 duration_ms >= 阈值的记录（R14.4）。
        let entries = vec![
            make_failure("ws", FailureCode::SseTimeout, 130_000), // 慢
            make_failure("ws", FailureCode::SseTimeout, 200_000), // 慢
            make_failure("ws", FailureCode::HitMaxIterations, 150_000), // 慢
            make_failure("ws", FailureCode::EmptyResult, 1_000),  // 快，忽略
        ];
        let dist = slow_request_reason_distribution(&entries, 120);
        assert_eq!(dist.get(&FailureCode::SseTimeout), Some(&2));
        assert_eq!(dist.get(&FailureCode::HitMaxIterations), Some(&1));
        // 快请求不计入分布
        assert_eq!(dist.get(&FailureCode::EmptyResult), None);
        assert_eq!(dist.values().sum::<usize>(), 3);
    }

    #[test]
    fn test_slow_request_distribution_threshold_boundary_inclusive() {
        // 边界：恰好等于阈值视为慢请求（>=）。
        let entries = vec![
            make_failure("ws", FailureCode::SubtaskTimeout, 120_000), // 恰好等于 => 慢
            make_failure("ws", FailureCode::SubtaskTimeout, 119_999), // 低于 1ms => 快
        ];
        let dist = slow_request_reason_distribution(&entries, 120);
        assert_eq!(dist.get(&FailureCode::SubtaskTimeout), Some(&1));
    }

    #[test]
    fn test_slow_request_distribution_empty() {
        let dist = slow_request_reason_distribution(&[], 120);
        assert!(dist.is_empty());
    }

    #[test]
    fn test_log_failure_writes_line_and_round_trips() {
        let tmp = TempDir::new().unwrap();
        let log = AuditLog::new(tmp.path());
        log.log_failure(&make_failure("alice", FailureCode::SseTimeout, 130_000))
            .unwrap();
        log.log_failure(&make_failure("alice", FailureCode::HitMaxIterations, 150_000))
            .unwrap();
        log.log_failure(&make_failure("bob", FailureCode::EmptyResult, 5_000))
            .unwrap();

        // 全部失败记录可回读
        let all = log.query_failures(None, None);
        assert_eq!(all.len(), 3);

        // 按 workspace 过滤
        let alice = log.query_failures(None, Some("alice"));
        assert_eq!(alice.len(), 2);

        // 回读后计算慢请求原因码分布（R14.4）
        let dist = slow_request_reason_distribution(&all, 120);
        assert_eq!(dist.get(&FailureCode::SseTimeout), Some(&1));
        assert_eq!(dist.get(&FailureCode::HitMaxIterations), Some(&1));
        assert_eq!(dist.get(&FailureCode::EmptyResult), None);
    }

    #[test]
    fn test_query_failures_empty() {
        let tmp = TempDir::new().unwrap();
        let log = AuditLog::new(tmp.path());
        assert!(log.query_failures(None, None).is_empty());
    }

    // ---------------------------------------------------------------------
    // 集成测试（Task 18.5）：迭代重置 WARN 打点 + 慢请求原因码分布每日记录。
    // ---------------------------------------------------------------------

    use std::sync::{Arc, Mutex};
    use tracing::field::{Field, Visit};
    use tracing::span::{Attributes, Id, Record};
    use tracing::{Event, Level, Metadata, Subscriber};

    /// 捕获到的单条 tracing 事件（级别 + 字段键值）。
    #[derive(Default)]
    struct CapturedEvent {
        level: Option<Level>,
        fields: HashMap<String, String>,
    }

    /// 最小 tracing Subscriber，用于在测试内捕获事件，无需引入外部 dev-dependency。
    struct CaptureSubscriber {
        events: Arc<Mutex<Vec<CapturedEvent>>>,
    }

    struct FieldVisitor<'a> {
        fields: &'a mut HashMap<String, String>,
    }

    impl Visit for FieldVisitor<'_> {
        fn record_str(&mut self, field: &Field, value: &str) {
            self.fields.insert(field.name().to_string(), value.to_string());
        }
        fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
            // `%workspace_key`(Display) 与消息体走 record_debug，DisplayValue 的 Debug
            // 直接委托给 Display，因此得到不带引号的原始字符串。
            self.fields
                .insert(field.name().to_string(), format!("{value:?}"));
        }
    }

    impl Subscriber for CaptureSubscriber {
        fn enabled(&self, _metadata: &Metadata<'_>) -> bool {
            true
        }
        fn new_span(&self, _span: &Attributes<'_>) -> Id {
            Id::from_u64(1)
        }
        fn record(&self, _span: &Id, _values: &Record<'_>) {}
        fn record_follows_from(&self, _span: &Id, _follows: &Id) {}
        fn event(&self, event: &Event<'_>) {
            let mut captured = CapturedEvent {
                level: Some(*event.metadata().level()),
                ..Default::default()
            };
            let mut visitor = FieldVisitor {
                fields: &mut captured.fields,
            };
            event.record(&mut visitor);
            self.events.lock().unwrap().push(captured);
        }
        fn enter(&self, _span: &Id) {}
        fn exit(&self, _span: &Id) {}
    }

    /// R14.2：`warn_max_iterations_reset` 以 WARN 级别记录事件并附带 Workspace 标识。
    ///
    /// 使用进程内捕获订阅者断言事件级别为 WARN、`event` 字段为 `max_iterations_reset`，
    /// 且 `workspace_key` 字段携带传入的 Workspace 标识。
    #[test]
    fn test_warn_max_iterations_reset_emits_warn_with_workspace() {
        let events = Arc::new(Mutex::new(Vec::<CapturedEvent>::new()));
        let subscriber = CaptureSubscriber {
            events: Arc::clone(&events),
        };

        tracing::subscriber::with_default(subscriber, || {
            warn_max_iterations_reset("ws_capture_001");
        });

        let captured = events.lock().unwrap();
        assert_eq!(captured.len(), 1, "应恰好发出一条事件");
        let ev = &captured[0];
        assert_eq!(ev.level, Some(Level::WARN), "事件级别必须为 WARN（R14.2）");
        assert_eq!(
            ev.fields.get("event").map(String::as_str),
            Some("max_iterations_reset"),
            "事件应携带 event=max_iterations_reset 字段"
        );
        assert_eq!(
            ev.fields.get("workspace_key").map(String::as_str),
            Some("ws_capture_001"),
            "事件应携带传入的 Workspace 标识（R14.2）"
        );
    }

    /// R14.4：慢请求原因码分布每日记录的端到端往返。
    ///
    /// 通过 `log_failure` 写入混合快/慢请求的失败记录 → 经 `query_failures` 按
    /// Workspace 过滤回读当天文件 → 用 `slow_request_reason_distribution` 计算分布，
    /// 断言仅统计慢请求且按原因码正确聚合，覆盖每日文件往返。
    #[test]
    fn test_daily_slow_request_distribution_end_to_end() {
        let tmp = TempDir::new().unwrap();
        let log = AuditLog::new(tmp.path());

        // 目标 workspace：3 条慢（>=120s）+ 1 条快。
        log.log_failure(&make_failure("ws_slow", FailureCode::SseTimeout, 130_000))
            .unwrap();
        log.log_failure(&make_failure("ws_slow", FailureCode::SseTimeout, 240_000))
            .unwrap();
        log.log_failure(&make_failure(
            "ws_slow",
            FailureCode::HitMaxIterations,
            120_000, // 恰好等于阈值 => 慢
        ))
        .unwrap();
        log.log_failure(&make_failure("ws_slow", FailureCode::EmptyResult, 3_000)) // 快，忽略
            .unwrap();
        // 其他 workspace 的记录不应混入目标 workspace 的统计。
        log.log_failure(&make_failure(
            "ws_other",
            FailureCode::PollutionReplay,
            300_000,
        ))
        .unwrap();

        // 回读当天文件并按 workspace 过滤。
        let entries = log.query_failures(None, Some("ws_slow"));
        assert_eq!(entries.len(), 4, "应回读到目标 workspace 的 4 条记录");

        // 计算慢请求原因码分布（阈值 120s）。
        let dist = slow_request_reason_distribution(&entries, 120);
        assert_eq!(dist.get(&FailureCode::SseTimeout), Some(&2));
        assert_eq!(dist.get(&FailureCode::HitMaxIterations), Some(&1));
        // 快请求与其他 workspace 的原因码均不计入。
        assert_eq!(dist.get(&FailureCode::EmptyResult), None);
        assert_eq!(dist.get(&FailureCode::PollutionReplay), None);
        assert_eq!(dist.values().sum::<usize>(), 3, "慢请求总计数应为 3");
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(100))]

        // 慢请求分布的不变式：分布总计数 == 慢请求条数（duration_ms >= 阈值）。
        // Validates: Requirements 14.4
        #[test]
        fn prop_slow_distribution_total_matches_slow_count(
            samples in prop::collection::vec(
                (arb_failure_code(), 0u64..600_000),
                0..32,
            ),
            threshold_secs in 0u64..300,
        ) {
            let entries: Vec<FailureAuditEntry> = samples
                .into_iter()
                .map(|(code, dur)| make_failure("ws", code, dur))
                .collect();
            let threshold_ms = threshold_secs.saturating_mul(1000);
            let expected_slow = entries
                .iter()
                .filter(|e| e.duration_ms >= threshold_ms)
                .count();
            let dist = slow_request_reason_distribution(&entries, threshold_secs);
            prop_assert_eq!(dist.values().sum::<usize>(), expected_slow);
        }
    }
}
