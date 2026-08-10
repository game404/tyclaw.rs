//! 使用统计：旁路记录请求生命周期，并维护可按日期范围查询的 SQLite 日级聚合。

use chrono::{DateTime, Duration as ChronoDuration, NaiveDate, Utc};
use chrono_tz::Tz;
use parking_lot::Mutex;
use regex::Regex;
use rusqlite::{params, params_from_iter, types::Value as SqlValue, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc, OnceLock};
use std::time::Duration;
use tracing::{info, warn};
use uuid::Uuid;

const WRITER_QUEUE_CAPACITY: usize = 8192;
const WRITER_BATCH_SIZE: usize = 256;
const WRITER_BATCH_WAIT: Duration = Duration::from_millis(100);
const SCHEMA_VERSION: i64 = 1;

#[derive(Debug, Clone, Deserialize)]
pub struct AnalyticsConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_timezone")]
    pub timezone: String,
    #[serde(default = "default_detail_retention_days")]
    pub detail_retention_days: u32,
    #[serde(default = "default_aggregate_retention_days")]
    pub aggregate_retention_days: u32,
    #[serde(default = "default_session_timeout_minutes")]
    pub session_timeout_minutes: u32,
    #[serde(default = "default_max_storage_mb")]
    pub max_storage_mb: u64,
    #[serde(default = "default_true")]
    pub capture_content_preview: bool,
    #[serde(default = "default_preview_max_chars")]
    pub preview_max_chars: usize,
}

impl Default for AnalyticsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            timezone: default_timezone(),
            detail_retention_days: default_detail_retention_days(),
            aggregate_retention_days: default_aggregate_retention_days(),
            session_timeout_minutes: default_session_timeout_minutes(),
            max_storage_mb: default_max_storage_mb(),
            capture_content_preview: true,
            preview_max_chars: default_preview_max_chars(),
        }
    }
}

impl AnalyticsConfig {
    fn normalized(mut self) -> Self {
        self.detail_retention_days = self.detail_retention_days.clamp(1, 400);
        self.aggregate_retention_days = self
            .aggregate_retention_days
            .clamp(self.detail_retention_days, 3650);
        self.session_timeout_minutes = self.session_timeout_minutes.clamp(1, 1440);
        self.max_storage_mb = self.max_storage_mb.clamp(64, 1024 * 1024);
        self.preview_max_chars = self.preview_max_chars.min(500);
        self
    }
}

fn default_true() -> bool {
    true
}
fn default_timezone() -> String {
    "Asia/Shanghai".into()
}
fn default_detail_retention_days() -> u32 {
    90
}
fn default_aggregate_retention_days() -> u32 {
    400
}
fn default_session_timeout_minutes() -> u32 {
    30
}
fn default_max_storage_mb() -> u64 {
    5120
}
fn default_preview_max_chars() -> usize {
    120
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UsageSource {
    Interactive,
    Automated,
}

impl UsageSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Interactive => "interactive",
            Self::Automated => "automated",
        }
    }
}

impl FromStr for UsageSource {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "interactive" => Ok(Self::Interactive),
            "automated" => Ok(Self::Automated),
            _ => Err(format!("unsupported analytics source: {value}")),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InteractionKind {
    Question,
    Resume,
    Injected,
    Command,
}

impl InteractionKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Question => "question",
            Self::Resume => "resume",
            Self::Injected => "injected",
            Self::Command => "command",
        }
    }

    fn is_question(self) -> bool {
        matches!(self, Self::Question | Self::Resume)
    }
}

impl FromStr for InteractionKind {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "question" => Ok(Self::Question),
            "resume" => Ok(Self::Resume),
            "injected" => Ok(Self::Injected),
            "command" => Ok(Self::Command),
            _ => Err(format!("unsupported interaction kind: {value}")),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UsageStatus {
    Completed,
    NeedsInput,
    Injected,
    Command,
    Error,
    Interrupted,
}

impl UsageStatus {
    fn as_str(self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::NeedsInput => "needs_input",
            Self::Injected => "injected",
            Self::Command => "command",
            Self::Error => "error",
            Self::Interrupted => "interrupted",
        }
    }

    fn is_error(self) -> bool {
        matches!(self, Self::Error | Self::Interrupted)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UsageToolEvent {
    pub scope: String,
    pub name: String,
    pub status: String,
    pub route: String,
    pub risk_level: String,
    pub duration_ms: u64,
}

#[derive(Debug, Clone)]
pub struct UsageRequest {
    request_id: String,
}

#[derive(Debug, Clone)]
pub struct UsageFinish {
    pub status: UsageStatus,
    pub interaction_kind: InteractionKind,
    pub response: String,
    pub has_response: bool,
    pub duration_ms: u64,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub tools: Vec<UsageToolEvent>,
    pub error_kind: Option<String>,
}

impl Default for UsageFinish {
    fn default() -> Self {
        Self {
            status: UsageStatus::Completed,
            interaction_kind: InteractionKind::Question,
            response: String::new(),
            has_response: false,
            duration_ms: 0,
            prompt_tokens: 0,
            completion_tokens: 0,
            tools: Vec::new(),
            error_kind: None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct AnalyticsQuery {
    pub from: NaiveDate,
    pub to: NaiveDate,
    pub workspace: Option<String>,
    pub channel: Option<String>,
    pub source: Option<UsageSource>,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct AnalyticsHealth {
    pub enabled: bool,
    pub available: bool,
    pub dropped_events: u64,
    pub database_bytes: u64,
    pub storage_warning: bool,
    pub detail_evictions: u64,
    pub last_error: Option<String>,
    pub earliest_detail_date: Option<String>,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct AnalyticsSummary {
    pub requests: u64,
    pub interactive_requests: u64,
    pub automated_requests: u64,
    pub active_users: u64,
    pub sessions: u64,
    pub questions: u64,
    pub answers: u64,
    pub errors: u64,
    pub answer_rate: f64,
    pub error_rate: f64,
    pub average_duration_ms: u64,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub tool_calls: u64,
    pub tool_successes: u64,
    pub tool_failures: u64,
    pub tool_success_rate: f64,
    #[serde(skip)]
    duration_sum_ms: u64,
    #[serde(skip)]
    duration_count: u64,
}

impl AnalyticsSummary {
    fn finalize(&mut self) {
        self.answer_rate = percent(self.answers, self.questions);
        self.error_rate = percent(self.errors, self.requests);
        self.average_duration_ms = if self.duration_count == 0 {
            0
        } else {
            self.duration_sum_ms / self.duration_count
        };
        self.tool_success_rate = percent(self.tool_successes, self.tool_calls);
    }
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct AnalyticsSeriesPoint {
    pub period_start: String,
    pub requests: u64,
    pub interactive_requests: u64,
    pub automated_requests: u64,
    pub active_users: u64,
    pub sessions: u64,
    pub questions: u64,
    pub answers: u64,
    pub errors: u64,
    pub average_duration_ms: u64,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub tool_calls: u64,
    #[serde(skip)]
    duration_sum_ms: u64,
    #[serde(skip)]
    duration_count: u64,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct AnalyticsToolRow {
    pub scope: String,
    pub name: String,
    pub calls: u64,
    pub successes: u64,
    pub failures: u64,
    pub denied: u64,
    pub average_duration_ms: u64,
    #[serde(skip)]
    duration_sum_ms: u64,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct AnalyticsUserRow {
    pub user_name: String,
    pub masked_user_id: String,
    pub requests: u64,
    pub sessions: u64,
    pub questions: u64,
    pub answers: u64,
    pub errors: u64,
    pub tool_calls: u64,
    pub last_active_at: String,
    #[serde(skip)]
    user_id: String,
    #[serde(skip)]
    last_seen_ms: i64,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct AnalyticsChannelRow {
    pub channel: String,
    pub requests: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct AnalyticsRecentRow {
    pub started_at: String,
    pub user_name: String,
    pub masked_user_id: String,
    pub channel: String,
    pub source: String,
    pub status: String,
    pub request_preview: String,
    pub response_preview: String,
    pub duration_ms: u64,
    pub tools: Vec<UsageToolEvent>,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct AnalyticsFilters {
    pub workspaces: Vec<String>,
    pub channels: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct AnalyticsReport {
    pub timezone: String,
    pub grain: String,
    pub from: String,
    pub to: String,
    pub detail_retention_days: u32,
    pub aggregate_retention_days: u32,
    pub health: AnalyticsHealth,
    pub summary: AnalyticsSummary,
    pub series: Vec<AnalyticsSeriesPoint>,
    pub tools: Vec<AnalyticsToolRow>,
    pub users: Vec<AnalyticsUserRow>,
    pub channels: Vec<AnalyticsChannelRow>,
    pub recent: Vec<AnalyticsRecentRow>,
    pub filters: AnalyticsFilters,
}

#[derive(Clone)]
pub struct UsageAnalytics {
    inner: Arc<AnalyticsInner>,
    sender: Option<mpsc::SyncSender<WriterMessage>>,
}

struct AnalyticsInner {
    config: AnalyticsConfig,
    timezone: Tz,
    db_path: PathBuf,
    sessions: Mutex<HashMap<String, SessionState>>,
    health: Arc<HealthState>,
}

#[derive(Debug, Clone)]
struct SessionState {
    session_id: String,
    last_seen_ms: i64,
}

#[derive(Default)]
struct HealthState {
    enabled: AtomicBool,
    available: AtomicBool,
    dropped_events: AtomicU64,
    database_bytes: AtomicU64,
    storage_warning: AtomicBool,
    detail_evictions: AtomicU64,
    last_error: Mutex<Option<String>>,
    earliest_detail_date: Mutex<Option<String>>,
}

#[derive(Debug)]
enum WriterMessage {
    Start(StartRecord),
    Finish(FinishRecord),
}

#[derive(Debug)]
struct StartRecord {
    request_id: String,
    started_at_ms: i64,
    local_date: String,
    source: UsageSource,
    interaction_kind: InteractionKind,
    user_id: String,
    user_name: String,
    workspace_key: String,
    channel: String,
    session_key: String,
    session_id: String,
    session_started: bool,
    request_preview: String,
}

#[derive(Debug)]
struct FinishRecord {
    request_id: String,
    finished_at_ms: i64,
    finish: UsageFinish,
    response_preview: String,
}

impl UsageAnalytics {
    pub fn new(workspace: impl AsRef<Path>, config: AnalyticsConfig) -> Self {
        let config = config.normalized();
        let db_path = workspace.as_ref().join("analytics").join("usage.sqlite3");
        let health = Arc::new(HealthState::default());
        health.enabled.store(config.enabled, Ordering::Relaxed);

        let timezone = match config.timezone.parse::<Tz>() {
            Ok(tz) => tz,
            Err(_) => {
                *health.last_error.lock() = Some("invalid_analytics_timezone".into());
                return Self::inactive(config, chrono_tz::UTC, db_path, health);
            }
        };
        if !config.enabled {
            return Self::inactive(config, timezone, db_path, health);
        }

        let parent = db_path.parent().unwrap_or_else(|| Path::new("."));
        if let Err(error) = std::fs::create_dir_all(parent) {
            warn!(error = %error, path = %parent.display(), "无法创建使用统计目录");
            *health.last_error.lock() = Some("analytics_directory_unavailable".into());
            return Self::inactive(config, timezone, db_path, health);
        }

        let mut connection = match Connection::open(&db_path).and_then(|conn| {
            configure_writer_connection(&conn)?;
            init_schema(&conn)?;
            Ok(conn)
        }) {
            Ok(connection) => connection,
            Err(error) => {
                warn!(error = %error, path = %db_path.display(), "使用统计数据库初始化失败");
                *health.last_error.lock() = Some("analytics_database_unavailable".into());
                return Self::inactive(config, timezone, db_path, health);
            }
        };

        if let Err(error) = finalize_interrupted_requests(&mut connection) {
            warn!(error = %error, "恢复中断的使用统计请求失败");
            *health.last_error.lock() = Some("analytics_recovery_failed".into());
        }
        if let Err(error) = run_maintenance(&mut connection, &db_path, timezone, &config, &health) {
            warn!(error = %error, "使用统计启动清理失败");
            *health.last_error.lock() = Some("analytics_maintenance_failed".into());
        }

        let sessions = load_session_states(&connection).unwrap_or_else(|error| {
            warn!(error = %error, "加载分析会话状态失败");
            HashMap::new()
        });
        let (sender, receiver) = mpsc::sync_channel(WRITER_QUEUE_CAPACITY);
        health.available.store(true, Ordering::Release);
        let writer_health = Arc::clone(&health);
        let writer_config = config.clone();
        let writer_path = db_path.clone();
        std::thread::Builder::new()
            .name("tyclaw-analytics-writer".into())
            .spawn(move || {
                writer_loop(
                    &mut connection,
                    receiver,
                    writer_path,
                    timezone,
                    writer_config,
                    writer_health,
                )
            })
            .unwrap_or_else(|error| {
                health.available.store(false, Ordering::Release);
                *health.last_error.lock() = Some("analytics_writer_unavailable".into());
                warn!(error = %error, "使用统计写线程启动失败");
                std::thread::spawn(|| {})
            });

        info!(path = %db_path.display(), "使用统计已启用");
        Self {
            inner: Arc::new(AnalyticsInner {
                config,
                timezone,
                db_path,
                sessions: Mutex::new(sessions),
                health,
            }),
            sender: Some(sender),
        }
    }

    fn inactive(
        config: AnalyticsConfig,
        timezone: Tz,
        db_path: PathBuf,
        health: Arc<HealthState>,
    ) -> Self {
        Self {
            inner: Arc::new(AnalyticsInner {
                config,
                timezone,
                db_path,
                sessions: Mutex::new(HashMap::new()),
                health,
            }),
            sender: None,
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn begin_request(
        &self,
        source: UsageSource,
        interaction_kind: InteractionKind,
        user_id: &str,
        user_name: &str,
        workspace_key: &str,
        channel: &str,
        chat_id: &str,
        request: &str,
    ) -> UsageRequest {
        let request_id = Uuid::new_v4().to_string();
        if self.sender.is_none() || !self.inner.health.available.load(Ordering::Acquire) {
            return UsageRequest { request_id };
        }

        let now = Utc::now();
        let now_ms = now.timestamp_millis();
        let local_date = now
            .with_timezone(&self.inner.timezone)
            .date_naive()
            .to_string();
        let session_key = hash_session_key(user_id, channel, chat_id);
        let (session_id, session_started) = if source == UsageSource::Interactive {
            let timeout_ms = self.inner.config.session_timeout_minutes as i64 * 60_000;
            let mut sessions = self.inner.sessions.lock();
            match sessions.get_mut(&session_key) {
                Some(state) if now_ms.saturating_sub(state.last_seen_ms) <= timeout_ms => {
                    state.last_seen_ms = now_ms;
                    (state.session_id.clone(), false)
                }
                _ => {
                    let state = SessionState {
                        session_id: Uuid::new_v4().to_string(),
                        last_seen_ms: now_ms,
                    };
                    let result = (state.session_id.clone(), true);
                    sessions.insert(session_key.clone(), state);
                    result
                }
            }
        } else {
            (String::new(), false)
        };

        let record = StartRecord {
            request_id: request_id.clone(),
            started_at_ms: now_ms,
            local_date,
            source,
            interaction_kind,
            user_id: user_id.to_string(),
            user_name: user_name.to_string(),
            workspace_key: workspace_key.to_string(),
            channel: channel.to_string(),
            session_key,
            session_id,
            session_started,
            request_preview: self.preview(request),
        };
        self.try_send(WriterMessage::Start(record));
        UsageRequest { request_id }
    }

    pub fn finish_request(&self, request: &UsageRequest, mut finish: UsageFinish) {
        finish.error_kind = finish.error_kind.map(|value| truncate_chars(&value, 64));
        let response_preview = self.preview(&finish.response);
        self.try_send(WriterMessage::Finish(FinishRecord {
            request_id: request.request_id.clone(),
            finished_at_ms: Utc::now().timestamp_millis(),
            finish,
            response_preview,
        }));
    }

    fn preview(&self, content: &str) -> String {
        if !self.inner.config.capture_content_preview || self.inner.config.preview_max_chars == 0 {
            return String::new();
        }
        sanitize_preview(content, self.inner.config.preview_max_chars)
    }

    fn try_send(&self, message: WriterMessage) {
        let Some(sender) = &self.sender else { return };
        if let Err(error) = sender.try_send(message) {
            self.inner
                .health
                .dropped_events
                .fetch_add(1, Ordering::Relaxed);
            if matches!(error, mpsc::TrySendError::Disconnected(_)) {
                self.inner.health.available.store(false, Ordering::Release);
                *self.inner.health.last_error.lock() = Some("analytics_writer_disconnected".into());
            }
        }
    }

    pub fn health(&self) -> AnalyticsHealth {
        AnalyticsHealth {
            enabled: self.inner.health.enabled.load(Ordering::Relaxed),
            available: self.inner.health.available.load(Ordering::Acquire),
            dropped_events: self.inner.health.dropped_events.load(Ordering::Relaxed),
            database_bytes: self.inner.health.database_bytes.load(Ordering::Relaxed),
            storage_warning: self.inner.health.storage_warning.load(Ordering::Relaxed),
            detail_evictions: self.inner.health.detail_evictions.load(Ordering::Relaxed),
            last_error: self.inner.health.last_error.lock().clone(),
            earliest_detail_date: self.inner.health.earliest_detail_date.lock().clone(),
        }
    }

    pub fn today(&self) -> NaiveDate {
        Utc::now().with_timezone(&self.inner.timezone).date_naive()
    }

    pub fn aggregate_retention_days(&self) -> u32 {
        self.inner.config.aggregate_retention_days
    }

    pub fn query(&self, query: &AnalyticsQuery) -> Result<AnalyticsReport, String> {
        if !self.inner.config.enabled {
            return Err("analytics_disabled".into());
        }
        if !self.inner.health.available.load(Ordering::Acquire) {
            return Err(self
                .inner
                .health
                .last_error
                .lock()
                .clone()
                .unwrap_or_else(|| "analytics_unavailable".into()));
        }
        if query.from > query.to {
            return Err("analytics_invalid_date_range".into());
        }
        let requested_days = (query.to - query.from).num_days() + 1;
        if requested_days > self.inner.config.aggregate_retention_days as i64 {
            return Err("analytics_range_exceeds_retention".into());
        }

        let connection = Connection::open(&self.inner.db_path)
            .map_err(|_| "analytics_query_unavailable".to_string())?;
        connection
            .busy_timeout(Duration::from_secs(2))
            .map_err(|_| "analytics_query_unavailable".to_string())?;
        build_report(&connection, &self.inner, query).map_err(|error| {
            warn!(error = %error, "使用统计查询失败");
            "analytics_query_failed".into()
        })
    }
}

fn writer_loop(
    connection: &mut Connection,
    receiver: mpsc::Receiver<WriterMessage>,
    db_path: PathBuf,
    timezone: Tz,
    config: AnalyticsConfig,
    health: Arc<HealthState>,
) {
    let mut last_maintenance_date = Utc::now().with_timezone(&timezone).date_naive();
    loop {
        let first = match receiver.recv_timeout(WRITER_BATCH_WAIT) {
            Ok(message) => message,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                refresh_database_health(connection, &db_path, &config, &health);
                continue;
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        };
        let mut batch = Vec::with_capacity(WRITER_BATCH_SIZE);
        batch.push(first);
        while batch.len() < WRITER_BATCH_SIZE {
            match receiver.try_recv() {
                Ok(message) => batch.push(message),
                Err(_) => break,
            }
        }

        let write_result = connection.transaction().and_then(|transaction| {
            for message in batch {
                match message {
                    WriterMessage::Start(record) => apply_start(&transaction, &record)?,
                    WriterMessage::Finish(record) => apply_finish(&transaction, &record)?,
                }
            }
            transaction.commit()
        });
        if let Err(error) = write_result {
            warn!(error = %error, "使用统计批量写入失败");
            *health.last_error.lock() = Some("analytics_write_failed".into());
        }

        let today = Utc::now().with_timezone(&timezone).date_naive();
        if today != last_maintenance_date {
            if let Err(error) = run_maintenance(connection, &db_path, timezone, &config, &health) {
                warn!(error = %error, "使用统计每日清理失败");
                *health.last_error.lock() = Some("analytics_maintenance_failed".into());
            }
            last_maintenance_date = today;
        } else {
            refresh_database_health(connection, &db_path, &config, &health);
            if health.storage_warning.load(Ordering::Relaxed)
                && health.database_bytes.load(Ordering::Relaxed)
                    >= config.max_storage_mb * 1024 * 1024
            {
                if let Err(error) = enforce_storage_quota(connection, &db_path, &config, &health) {
                    warn!(error = %error, "使用统计存储配额处理失败");
                    *health.last_error.lock() = Some("analytics_quota_cleanup_failed".into());
                }
            }
        }
    }
    health.available.store(false, Ordering::Release);
}

fn configure_writer_connection(connection: &Connection) -> rusqlite::Result<()> {
    connection.busy_timeout(Duration::from_secs(5))?;
    connection.execute_batch(
        "PRAGMA journal_mode=WAL;
         PRAGMA synchronous=NORMAL;
         PRAGMA foreign_keys=ON;
         PRAGMA auto_vacuum=INCREMENTAL;",
    )
}

fn init_schema(connection: &Connection) -> rusqlite::Result<()> {
    connection.execute_batch(
        "CREATE TABLE IF NOT EXISTS usage_requests (
            request_id TEXT PRIMARY KEY,
            started_at_ms INTEGER NOT NULL,
            finished_at_ms INTEGER,
            local_date TEXT NOT NULL,
            source TEXT NOT NULL,
            interaction_kind TEXT NOT NULL,
            status TEXT NOT NULL,
            user_id TEXT NOT NULL,
            user_name TEXT NOT NULL,
            workspace_key TEXT NOT NULL,
            channel TEXT NOT NULL,
            session_id TEXT NOT NULL,
            session_started INTEGER NOT NULL DEFAULT 0,
            request_preview TEXT NOT NULL DEFAULT '',
            response_preview TEXT NOT NULL DEFAULT '',
            duration_ms INTEGER NOT NULL DEFAULT 0,
            prompt_tokens INTEGER NOT NULL DEFAULT 0,
            completion_tokens INTEGER NOT NULL DEFAULT 0,
            tool_calls INTEGER NOT NULL DEFAULT 0,
            tool_successes INTEGER NOT NULL DEFAULT 0,
            tool_failures INTEGER NOT NULL DEFAULT 0,
            tools_json TEXT NOT NULL DEFAULT '[]',
            error_kind TEXT
         );
         CREATE INDEX IF NOT EXISTS idx_usage_requests_date ON usage_requests(local_date, started_at_ms DESC);
         CREATE INDEX IF NOT EXISTS idx_usage_requests_dimensions ON usage_requests(local_date, workspace_key, channel, source);
         CREATE TABLE IF NOT EXISTS analytics_sessions (
            session_key TEXT PRIMARY KEY,
            session_id TEXT NOT NULL,
            last_seen_ms INTEGER NOT NULL
         );
         CREATE TABLE IF NOT EXISTS daily_usage (
            local_date TEXT NOT NULL,
            workspace_key TEXT NOT NULL,
            channel TEXT NOT NULL,
            source TEXT NOT NULL,
            requests INTEGER NOT NULL DEFAULT 0,
            questions INTEGER NOT NULL DEFAULT 0,
            answers INTEGER NOT NULL DEFAULT 0,
            errors INTEGER NOT NULL DEFAULT 0,
            sessions INTEGER NOT NULL DEFAULT 0,
            duration_sum_ms INTEGER NOT NULL DEFAULT 0,
            duration_count INTEGER NOT NULL DEFAULT 0,
            prompt_tokens INTEGER NOT NULL DEFAULT 0,
            completion_tokens INTEGER NOT NULL DEFAULT 0,
            tool_calls INTEGER NOT NULL DEFAULT 0,
            tool_successes INTEGER NOT NULL DEFAULT 0,
            tool_failures INTEGER NOT NULL DEFAULT 0,
            PRIMARY KEY(local_date, workspace_key, channel, source)
         );
         CREATE TABLE IF NOT EXISTS daily_users (
            local_date TEXT NOT NULL,
            workspace_key TEXT NOT NULL,
            channel TEXT NOT NULL,
            source TEXT NOT NULL,
            user_id TEXT NOT NULL,
            user_name TEXT NOT NULL,
            requests INTEGER NOT NULL DEFAULT 0,
            questions INTEGER NOT NULL DEFAULT 0,
            answers INTEGER NOT NULL DEFAULT 0,
            errors INTEGER NOT NULL DEFAULT 0,
            sessions INTEGER NOT NULL DEFAULT 0,
            tool_calls INTEGER NOT NULL DEFAULT 0,
            last_seen_ms INTEGER NOT NULL,
            PRIMARY KEY(local_date, workspace_key, channel, source, user_id)
         );
         CREATE TABLE IF NOT EXISTS daily_tools (
            local_date TEXT NOT NULL,
            workspace_key TEXT NOT NULL,
            channel TEXT NOT NULL,
            source TEXT NOT NULL,
            scope TEXT NOT NULL,
            tool_name TEXT NOT NULL,
            calls INTEGER NOT NULL DEFAULT 0,
            successes INTEGER NOT NULL DEFAULT 0,
            failures INTEGER NOT NULL DEFAULT 0,
            denied INTEGER NOT NULL DEFAULT 0,
            duration_sum_ms INTEGER NOT NULL DEFAULT 0,
            PRIMARY KEY(local_date, workspace_key, channel, source, scope, tool_name)
         );",
    )?;
    connection.pragma_update(None, "user_version", SCHEMA_VERSION)?;
    Ok(())
}

fn apply_start(
    transaction: &rusqlite::Transaction<'_>,
    record: &StartRecord,
) -> rusqlite::Result<()> {
    transaction.execute(
        "INSERT OR IGNORE INTO usage_requests (
            request_id, started_at_ms, local_date, source, interaction_kind, status,
            user_id, user_name, workspace_key, channel, session_id, session_started, request_preview
         ) VALUES (?1, ?2, ?3, ?4, ?5, 'running', ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
        params![
            record.request_id,
            record.started_at_ms,
            record.local_date,
            record.source.as_str(),
            record.interaction_kind.as_str(),
            record.user_id,
            record.user_name,
            record.workspace_key,
            record.channel,
            record.session_id,
            record.session_started as i64,
            record.request_preview,
        ],
    )?;
    if record.source == UsageSource::Interactive {
        transaction.execute(
            "INSERT INTO analytics_sessions(session_key, session_id, last_seen_ms)
             VALUES (?1, ?2, ?3)
             ON CONFLICT(session_key) DO UPDATE SET
                session_id=excluded.session_id, last_seen_ms=excluded.last_seen_ms",
            params![record.session_key, record.session_id, record.started_at_ms],
        )?;
    }
    Ok(())
}

#[derive(Debug)]
struct StoredRequest {
    local_date: String,
    source: UsageSource,
    user_id: String,
    user_name: String,
    workspace_key: String,
    channel: String,
    session_started: bool,
}

fn apply_finish(
    transaction: &rusqlite::Transaction<'_>,
    record: &FinishRecord,
) -> rusqlite::Result<()> {
    let stored = transaction
        .query_row(
            "SELECT local_date, source, user_id, user_name, workspace_key, channel, session_started
             FROM usage_requests WHERE request_id=?1 AND status='running'",
            params![record.request_id],
            |row| {
                let source: String = row.get(1)?;
                Ok(StoredRequest {
                    local_date: row.get(0)?,
                    source: source.parse().unwrap_or(UsageSource::Interactive),
                    user_id: row.get(2)?,
                    user_name: row.get(3)?,
                    workspace_key: row.get(4)?,
                    channel: row.get(5)?,
                    session_started: row.get::<_, i64>(6)? != 0,
                })
            },
        )
        .optional()?;
    let Some(stored) = stored else { return Ok(()) };

    let tool_calls = record.finish.tools.len() as u64;
    let tool_successes = record
        .finish
        .tools
        .iter()
        .filter(|event| event.status == "ok")
        .count() as u64;
    let tool_failures = tool_calls.saturating_sub(tool_successes);
    let tools_json =
        serde_json::to_string(&record.finish.tools.iter().take(50).collect::<Vec<_>>())
            .unwrap_or_else(|_| "[]".into());
    let updated = transaction.execute(
        "UPDATE usage_requests SET finished_at_ms=?2, interaction_kind=?3, status=?4,
            response_preview=?5, duration_ms=?6, prompt_tokens=?7, completion_tokens=?8,
            tool_calls=?9, tool_successes=?10, tool_failures=?11, tools_json=?12, error_kind=?13
         WHERE request_id=?1 AND status='running'",
        params![
            record.request_id,
            record.finished_at_ms,
            record.finish.interaction_kind.as_str(),
            record.finish.status.as_str(),
            record.response_preview,
            record.finish.duration_ms,
            record.finish.prompt_tokens,
            record.finish.completion_tokens,
            tool_calls,
            tool_successes,
            tool_failures,
            tools_json,
            record.finish.error_kind,
        ],
    )?;
    if updated == 0 {
        return Ok(());
    }

    let questions = (stored.source == UsageSource::Interactive
        && record.finish.interaction_kind.is_question()) as u64;
    let answers = (questions == 1
        && record.finish.status == UsageStatus::Completed
        && record.finish.has_response) as u64;
    let errors = record.finish.status.is_error() as u64;
    transaction.execute(
        "INSERT INTO daily_usage (
            local_date, workspace_key, channel, source, requests, questions, answers, errors,
            sessions, duration_sum_ms, duration_count, prompt_tokens, completion_tokens,
            tool_calls, tool_successes, tool_failures
         ) VALUES (?1, ?2, ?3, ?4, 1, ?5, ?6, ?7, ?8, ?9, 1, ?10, ?11, ?12, ?13, ?14)
         ON CONFLICT(local_date, workspace_key, channel, source) DO UPDATE SET
            requests=requests+1, questions=questions+excluded.questions,
            answers=answers+excluded.answers, errors=errors+excluded.errors,
            sessions=sessions+excluded.sessions, duration_sum_ms=duration_sum_ms+excluded.duration_sum_ms,
            duration_count=duration_count+1, prompt_tokens=prompt_tokens+excluded.prompt_tokens,
            completion_tokens=completion_tokens+excluded.completion_tokens,
            tool_calls=tool_calls+excluded.tool_calls,
            tool_successes=tool_successes+excluded.tool_successes,
            tool_failures=tool_failures+excluded.tool_failures",
        params![
            stored.local_date,
            stored.workspace_key,
            stored.channel,
            stored.source.as_str(),
            questions,
            answers,
            errors,
            stored.session_started as u64,
            record.finish.duration_ms,
            record.finish.prompt_tokens,
            record.finish.completion_tokens,
            tool_calls,
            tool_successes,
            tool_failures,
        ],
    )?;

    if stored.source == UsageSource::Interactive {
        transaction.execute(
            "INSERT INTO daily_users (
                local_date, workspace_key, channel, source, user_id, user_name, requests,
                questions, answers, errors, sessions, tool_calls, last_seen_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, 1, ?7, ?8, ?9, ?10, ?11, ?12)
             ON CONFLICT(local_date, workspace_key, channel, source, user_id) DO UPDATE SET
                user_name=CASE WHEN excluded.user_name='' THEN user_name ELSE excluded.user_name END,
                requests=requests+1, questions=questions+excluded.questions,
                answers=answers+excluded.answers, errors=errors+excluded.errors,
                sessions=sessions+excluded.sessions, tool_calls=tool_calls+excluded.tool_calls,
                last_seen_ms=MAX(last_seen_ms, excluded.last_seen_ms)",
            params![
                stored.local_date,
                stored.workspace_key,
                stored.channel,
                stored.source.as_str(),
                stored.user_id,
                stored.user_name,
                questions,
                answers,
                errors,
                stored.session_started as u64,
                tool_calls,
                record.finished_at_ms,
            ],
        )?;
    }

    for event in &record.finish.tools {
        let success = (event.status == "ok") as u64;
        let denied = (event.status == "denied") as u64;
        transaction.execute(
            "INSERT INTO daily_tools (
                local_date, workspace_key, channel, source, scope, tool_name,
                calls, successes, failures, denied, duration_sum_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, 1, ?7, ?8, ?9, ?10)
             ON CONFLICT(local_date, workspace_key, channel, source, scope, tool_name) DO UPDATE SET
                calls=calls+1, successes=successes+excluded.successes,
                failures=failures+excluded.failures, denied=denied+excluded.denied,
                duration_sum_ms=duration_sum_ms+excluded.duration_sum_ms",
            params![
                stored.local_date,
                stored.workspace_key,
                stored.channel,
                stored.source.as_str(),
                event.scope,
                event.name,
                success,
                1 - success,
                denied,
                event.duration_ms,
            ],
        )?;
    }
    Ok(())
}

fn finalize_interrupted_requests(connection: &mut Connection) -> rusqlite::Result<()> {
    let requests = {
        let mut statement = connection.prepare(
            "SELECT request_id, interaction_kind FROM usage_requests WHERE status='running'",
        )?;
        let values = statement
            .query_map([], |row| {
                let kind = row.get::<_, String>(1)?;
                Ok((
                    row.get::<_, String>(0)?,
                    kind.parse().unwrap_or(InteractionKind::Question),
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        values
    };
    if requests.is_empty() {
        return Ok(());
    }
    let transaction = connection.transaction()?;
    for (request_id, interaction_kind) in requests {
        apply_finish(
            &transaction,
            &FinishRecord {
                request_id,
                finished_at_ms: Utc::now().timestamp_millis(),
                response_preview: String::new(),
                finish: UsageFinish {
                    status: UsageStatus::Interrupted,
                    interaction_kind,
                    error_kind: Some("process_interrupted".into()),
                    ..UsageFinish::default()
                },
            },
        )?;
    }
    transaction.commit()
}

fn load_session_states(connection: &Connection) -> rusqlite::Result<HashMap<String, SessionState>> {
    let mut statement = connection
        .prepare("SELECT session_key, session_id, last_seen_ms FROM analytics_sessions")?;
    let rows = statement.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            SessionState {
                session_id: row.get(1)?,
                last_seen_ms: row.get(2)?,
            },
        ))
    })?;
    rows.collect()
}

fn run_maintenance(
    connection: &mut Connection,
    db_path: &Path,
    timezone: Tz,
    config: &AnalyticsConfig,
    health: &HealthState,
) -> rusqlite::Result<()> {
    let today = Utc::now().with_timezone(&timezone).date_naive();
    let detail_cutoff = today - ChronoDuration::days(config.detail_retention_days as i64 - 1);
    let aggregate_cutoff = today - ChronoDuration::days(config.aggregate_retention_days as i64 - 1);
    let transaction = connection.transaction()?;
    transaction.execute(
        "DELETE FROM usage_requests WHERE local_date < ?1",
        params![detail_cutoff.to_string()],
    )?;
    for table in ["daily_usage", "daily_users", "daily_tools"] {
        transaction.execute(
            &format!("DELETE FROM {table} WHERE local_date < ?1"),
            params![aggregate_cutoff.to_string()],
        )?;
    }
    let session_cutoff = Utc::now().timestamp_millis()
        - (config.session_timeout_minutes as i64 * 60_000).max(86_400_000);
    transaction.execute(
        "DELETE FROM analytics_sessions WHERE last_seen_ms < ?1",
        params![session_cutoff],
    )?;
    transaction.commit()?;
    enforce_storage_quota(connection, db_path, config, health)?;
    update_earliest_detail(connection, health)?;
    Ok(())
}

fn refresh_database_health(
    connection: &Connection,
    db_path: &Path,
    config: &AnalyticsConfig,
    health: &HealthState,
) {
    let bytes = sqlite_storage_bytes(db_path);
    health.database_bytes.store(bytes, Ordering::Relaxed);
    let limit = config.max_storage_mb * 1024 * 1024;
    health
        .storage_warning
        .store(bytes >= limit.saturating_mul(80) / 100, Ordering::Relaxed);
    let _ = update_earliest_detail(connection, health);
}

fn sqlite_storage_bytes(db_path: &Path) -> u64 {
    ["", "-wal", "-shm"]
        .iter()
        .map(|suffix| {
            let mut path = db_path.as_os_str().to_os_string();
            path.push(suffix);
            std::fs::metadata(PathBuf::from(path))
                .map(|metadata| metadata.len())
                .unwrap_or(0)
        })
        .fold(0, u64::saturating_add)
}

fn enforce_storage_quota(
    connection: &mut Connection,
    db_path: &Path,
    config: &AnalyticsConfig,
    health: &HealthState,
) -> rusqlite::Result<()> {
    let limit = config.max_storage_mb * 1024 * 1024;
    let target = limit.saturating_mul(90) / 100;
    refresh_database_health(connection, db_path, config, health);
    if health.database_bytes.load(Ordering::Relaxed) < limit {
        return Ok(());
    }

    for _ in 0..128 {
        let oldest = connection
            .query_row("SELECT MIN(local_date) FROM usage_requests", [], |row| {
                row.get::<_, Option<String>>(0)
            })?
            .unwrap_or_default();
        if oldest.is_empty() {
            break;
        }
        let removed = connection.execute(
            "DELETE FROM usage_requests WHERE local_date=?1",
            params![oldest],
        )?;
        if removed == 0 {
            break;
        }
        health
            .detail_evictions
            .fetch_add(removed as u64, Ordering::Relaxed);
        connection
            .execute_batch("PRAGMA wal_checkpoint(TRUNCATE); PRAGMA incremental_vacuum(4096);")?;
        refresh_database_health(connection, db_path, config, health);
        if health.database_bytes.load(Ordering::Relaxed) <= target {
            break;
        }
    }
    update_earliest_detail(connection, health)?;
    Ok(())
}

fn update_earliest_detail(connection: &Connection, health: &HealthState) -> rusqlite::Result<()> {
    let earliest =
        connection.query_row("SELECT MIN(local_date) FROM usage_requests", [], |row| {
            row.get::<_, Option<String>>(0)
        })?;
    *health.earliest_detail_date.lock() = earliest;
    Ok(())
}

#[derive(Debug)]
struct DailyUsageRow {
    date: NaiveDate,
    workspace: String,
    channel: String,
    source: UsageSource,
    requests: u64,
    questions: u64,
    answers: u64,
    errors: u64,
    sessions: u64,
    duration_sum_ms: u64,
    duration_count: u64,
    prompt_tokens: u64,
    completion_tokens: u64,
    tool_calls: u64,
    tool_successes: u64,
    tool_failures: u64,
}

#[derive(Debug)]
struct DailyUserRow {
    date: NaiveDate,
    workspace: String,
    channel: String,
    source: UsageSource,
    user_id: String,
    user_name: String,
    requests: u64,
    questions: u64,
    answers: u64,
    errors: u64,
    sessions: u64,
    tool_calls: u64,
    last_seen_ms: i64,
}

#[derive(Debug)]
struct DailyToolRow {
    workspace: String,
    channel: String,
    source: UsageSource,
    scope: String,
    name: String,
    calls: u64,
    successes: u64,
    failures: u64,
    denied: u64,
    duration_sum_ms: u64,
}

fn build_report(
    connection: &Connection,
    inner: &AnalyticsInner,
    query: &AnalyticsQuery,
) -> rusqlite::Result<AnalyticsReport> {
    let usage_rows = load_daily_usage(connection, query)?;
    let user_rows = load_daily_users(connection, query)?;
    let tool_rows = load_daily_tools(connection, query)?;
    let recent = load_recent(connection, query, inner.timezone)?;

    let mut summary = AnalyticsSummary::default();
    let mut series: BTreeMap<NaiveDate, AnalyticsSeriesPoint> = BTreeMap::new();
    let mut channels: HashMap<String, u64> = HashMap::new();
    for row in &usage_rows {
        add_usage_to_summary(&mut summary, row);
        *channels.entry(row.channel.clone()).or_default() += row.requests;
        let period = row.date;
        let point = series
            .entry(period)
            .or_insert_with(|| AnalyticsSeriesPoint {
                period_start: period.to_string(),
                ..Default::default()
            });
        point.requests += row.requests;
        match row.source {
            UsageSource::Interactive => point.interactive_requests += row.requests,
            UsageSource::Automated => point.automated_requests += row.requests,
        }
        point.sessions += row.sessions;
        point.questions += row.questions;
        point.answers += row.answers;
        point.errors += row.errors;
        point.duration_sum_ms += row.duration_sum_ms;
        point.duration_count += row.duration_count;
        point.prompt_tokens += row.prompt_tokens;
        point.completion_tokens += row.completion_tokens;
        point.tool_calls += row.tool_calls;
    }

    let mut unique_users = HashSet::new();
    let mut period_users: HashMap<NaiveDate, HashSet<String>> = HashMap::new();
    let mut users: HashMap<String, AnalyticsUserRow> = HashMap::new();
    for row in user_rows {
        if row.source != UsageSource::Interactive {
            continue;
        }
        unique_users.insert(row.user_id.clone());
        period_users
            .entry(row.date)
            .or_default()
            .insert(row.user_id.clone());
        let entry = users
            .entry(row.user_id.clone())
            .or_insert_with(|| AnalyticsUserRow {
                user_id: row.user_id.clone(),
                masked_user_id: mask_user_id(&row.user_id),
                ..Default::default()
            });
        if row.last_seen_ms >= entry.last_seen_ms {
            entry.last_seen_ms = row.last_seen_ms;
            entry.last_active_at = timestamp_string(row.last_seen_ms, inner.timezone);
            if !row.user_name.is_empty() {
                entry.user_name = row.user_name;
            }
        }
        entry.requests += row.requests;
        entry.sessions += row.sessions;
        entry.questions += row.questions;
        entry.answers += row.answers;
        entry.errors += row.errors;
        entry.tool_calls += row.tool_calls;
    }
    summary.active_users = unique_users.len() as u64;
    summary.finalize();
    for (period, point) in &mut series {
        point.active_users = period_users.get(period).map_or(0, |set| set.len() as u64);
        point.average_duration_ms = if point.duration_count == 0 {
            0
        } else {
            point.duration_sum_ms / point.duration_count
        };
    }

    let mut tool_map: HashMap<(String, String), AnalyticsToolRow> = HashMap::new();
    for row in tool_rows {
        let entry = tool_map
            .entry((row.scope.clone(), row.name.clone()))
            .or_insert_with(|| AnalyticsToolRow {
                scope: row.scope,
                name: row.name,
                ..Default::default()
            });
        entry.calls += row.calls;
        entry.successes += row.successes;
        entry.failures += row.failures;
        entry.denied += row.denied;
        entry.duration_sum_ms += row.duration_sum_ms;
    }
    let mut tools: Vec<_> = tool_map.into_values().collect();
    for row in &mut tools {
        row.average_duration_ms = if row.calls == 0 {
            0
        } else {
            row.duration_sum_ms / row.calls
        };
    }
    tools.sort_by(|a, b| b.calls.cmp(&a.calls).then_with(|| a.name.cmp(&b.name)));
    tools.truncate(50);

    let mut users: Vec<_> = users.into_values().collect();
    users.sort_by(|a, b| {
        b.requests
            .cmp(&a.requests)
            .then_with(|| a.user_id.cmp(&b.user_id))
    });
    users.truncate(50);
    let mut channels: Vec<_> = channels
        .into_iter()
        .map(|(channel, requests)| AnalyticsChannelRow { channel, requests })
        .collect();
    channels.sort_by(|a, b| b.requests.cmp(&a.requests));

    Ok(AnalyticsReport {
        timezone: inner.config.timezone.clone(),
        grain: "day".into(),
        from: query.from.to_string(),
        to: query.to.to_string(),
        detail_retention_days: inner.config.detail_retention_days,
        aggregate_retention_days: inner.config.aggregate_retention_days,
        health: UsageAnalytics {
            inner: Arc::new(AnalyticsInner {
                config: inner.config.clone(),
                timezone: inner.timezone,
                db_path: inner.db_path.clone(),
                sessions: Mutex::new(HashMap::new()),
                health: Arc::clone(&inner.health),
            }),
            sender: None,
        }
        .health(),
        summary,
        series: series.into_values().collect(),
        tools,
        users,
        channels,
        recent,
        filters: load_filters(connection)?,
    })
}

fn add_usage_to_summary(summary: &mut AnalyticsSummary, row: &DailyUsageRow) {
    summary.requests += row.requests;
    match row.source {
        UsageSource::Interactive => summary.interactive_requests += row.requests,
        UsageSource::Automated => summary.automated_requests += row.requests,
    }
    summary.sessions += row.sessions;
    summary.questions += row.questions;
    summary.answers += row.answers;
    summary.errors += row.errors;
    summary.duration_sum_ms += row.duration_sum_ms;
    summary.duration_count += row.duration_count;
    summary.prompt_tokens += row.prompt_tokens;
    summary.completion_tokens += row.completion_tokens;
    summary.tool_calls += row.tool_calls;
    summary.tool_successes += row.tool_successes;
    summary.tool_failures += row.tool_failures;
}

fn load_daily_usage(
    connection: &Connection,
    query: &AnalyticsQuery,
) -> rusqlite::Result<Vec<DailyUsageRow>> {
    let mut statement = connection.prepare(
        "SELECT local_date, workspace_key, channel, source, requests, questions, answers, errors,
                sessions, duration_sum_ms, duration_count, prompt_tokens, completion_tokens,
                tool_calls, tool_successes, tool_failures
         FROM daily_usage WHERE local_date>=?1 AND local_date<=?2",
    )?;
    let rows = statement.query_map(
        params![query.from.to_string(), query.to.to_string()],
        |row| {
            let source: String = row.get(3)?;
            Ok(DailyUsageRow {
                date: parse_sql_date(row.get::<_, String>(0)?),
                workspace: row.get(1)?,
                channel: row.get(2)?,
                source: source.parse().unwrap_or(UsageSource::Interactive),
                requests: row.get(4)?,
                questions: row.get(5)?,
                answers: row.get(6)?,
                errors: row.get(7)?,
                sessions: row.get(8)?,
                duration_sum_ms: row.get(9)?,
                duration_count: row.get(10)?,
                prompt_tokens: row.get(11)?,
                completion_tokens: row.get(12)?,
                tool_calls: row.get(13)?,
                tool_successes: row.get(14)?,
                tool_failures: row.get(15)?,
            })
        },
    )?;
    let rows = rows.collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows
        .into_iter()
        .filter(|row| matches_filters(&row.workspace, &row.channel, row.source, query))
        .collect())
}

fn load_daily_users(
    connection: &Connection,
    query: &AnalyticsQuery,
) -> rusqlite::Result<Vec<DailyUserRow>> {
    let mut statement = connection.prepare(
        "SELECT local_date, workspace_key, channel, source, user_id, user_name, requests,
                questions, answers, errors, sessions, tool_calls, last_seen_ms
         FROM daily_users WHERE local_date>=?1 AND local_date<=?2",
    )?;
    let rows = statement.query_map(
        params![query.from.to_string(), query.to.to_string()],
        |row| {
            let source: String = row.get(3)?;
            Ok(DailyUserRow {
                date: parse_sql_date(row.get::<_, String>(0)?),
                workspace: row.get(1)?,
                channel: row.get(2)?,
                source: source.parse().unwrap_or(UsageSource::Interactive),
                user_id: row.get(4)?,
                user_name: row.get(5)?,
                requests: row.get(6)?,
                questions: row.get(7)?,
                answers: row.get(8)?,
                errors: row.get(9)?,
                sessions: row.get(10)?,
                tool_calls: row.get(11)?,
                last_seen_ms: row.get(12)?,
            })
        },
    )?;
    let rows = rows.collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows
        .into_iter()
        .filter(|row| matches_filters(&row.workspace, &row.channel, row.source, query))
        .collect())
}

fn load_daily_tools(
    connection: &Connection,
    query: &AnalyticsQuery,
) -> rusqlite::Result<Vec<DailyToolRow>> {
    let mut statement = connection.prepare(
        "SELECT workspace_key, channel, source, scope, tool_name, calls, successes, failures,
                denied, duration_sum_ms
         FROM daily_tools WHERE local_date>=?1 AND local_date<=?2",
    )?;
    let rows = statement.query_map(
        params![query.from.to_string(), query.to.to_string()],
        |row| {
            let source: String = row.get(2)?;
            Ok(DailyToolRow {
                workspace: row.get(0)?,
                channel: row.get(1)?,
                source: source.parse().unwrap_or(UsageSource::Interactive),
                scope: row.get(3)?,
                name: row.get(4)?,
                calls: row.get(5)?,
                successes: row.get(6)?,
                failures: row.get(7)?,
                denied: row.get(8)?,
                duration_sum_ms: row.get(9)?,
            })
        },
    )?;
    let rows = rows.collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows
        .into_iter()
        .filter(|row| matches_filters(&row.workspace, &row.channel, row.source, query))
        .collect())
}

fn matches_filters(
    workspace: &str,
    channel: &str,
    source: UsageSource,
    query: &AnalyticsQuery,
) -> bool {
    (query.workspace.is_none() || query.workspace.as_deref() == Some(workspace))
        && (query.channel.is_none() || query.channel.as_deref() == Some(channel))
        && (query.source.is_none() || query.source == Some(source))
}

fn load_recent(
    connection: &Connection,
    query: &AnalyticsQuery,
    timezone: Tz,
) -> rusqlite::Result<Vec<AnalyticsRecentRow>> {
    let mut sql = String::from(
        "SELECT started_at_ms, user_name, user_id, channel, source, status, request_preview,
                response_preview, duration_ms, tools_json
         FROM usage_requests WHERE local_date>=?1 AND local_date<=?2 AND status!='running'",
    );
    let mut values = vec![
        SqlValue::Text(query.from.to_string()),
        SqlValue::Text(query.to.to_string()),
    ];
    if let Some(workspace) = &query.workspace {
        sql.push_str(" AND workspace_key=?");
        sql.push_str(&(values.len() + 1).to_string());
        values.push(SqlValue::Text(workspace.clone()));
    }
    if let Some(channel) = &query.channel {
        sql.push_str(" AND channel=?");
        sql.push_str(&(values.len() + 1).to_string());
        values.push(SqlValue::Text(channel.clone()));
    }
    if let Some(source) = query.source {
        sql.push_str(" AND source=?");
        sql.push_str(&(values.len() + 1).to_string());
        values.push(SqlValue::Text(source.as_str().into()));
    }
    sql.push_str(" ORDER BY started_at_ms DESC LIMIT 50");
    let mut statement = connection.prepare(&sql)?;
    let rows = statement.query_map(params_from_iter(values), |row| {
        let tools_json: String = row.get(9)?;
        Ok(AnalyticsRecentRow {
            started_at: timestamp_string(row.get(0)?, timezone),
            user_name: row.get(1)?,
            masked_user_id: mask_user_id(&row.get::<_, String>(2)?),
            channel: row.get(3)?,
            source: row.get(4)?,
            status: row.get(5)?,
            request_preview: row.get(6)?,
            response_preview: row.get(7)?,
            duration_ms: row.get(8)?,
            tools: serde_json::from_str(&tools_json).unwrap_or_default(),
        })
    })?;
    rows.collect()
}

fn load_filters(connection: &Connection) -> rusqlite::Result<AnalyticsFilters> {
    fn distinct(connection: &Connection, column: &str) -> rusqlite::Result<Vec<String>> {
        let mut statement = connection.prepare(&format!(
            "SELECT DISTINCT {column} FROM daily_usage ORDER BY {column}"
        ))?;
        let values = statement
            .query_map([], |row| row.get::<_, String>(0))?
            .collect();
        values
    }
    Ok(AnalyticsFilters {
        workspaces: distinct(connection, "workspace_key")?,
        channels: distinct(connection, "channel")?,
    })
}

fn hash_session_key(user_id: &str, channel: &str, chat_id: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(user_id.as_bytes());
    hasher.update([0]);
    hasher.update(channel.as_bytes());
    hasher.update([0]);
    hasher.update(chat_id.as_bytes());
    format!("{:x}", hasher.finalize())
}

fn sanitize_preview(content: &str, max_chars: usize) -> String {
    static SECRET_RE: OnceLock<Regex> = OnceLock::new();
    static TOKEN_RE: OnceLock<Regex> = OnceLock::new();
    static EMAIL_RE: OnceLock<Regex> = OnceLock::new();
    static PHONE_RE: OnceLock<Regex> = OnceLock::new();
    let secret_re = SECRET_RE.get_or_init(|| {
        Regex::new(r"(?i)(api[_-]?key|token|password|secret)\s*[:=]\s*[^\s,;]+")
            .expect("valid secret regex")
    });
    let token_re = TOKEN_RE.get_or_init(|| {
        Regex::new(r"(?:sk|ghp|glpat)-?[A-Za-z0-9_-]{16,}").expect("valid token regex")
    });
    let email_re = EMAIL_RE.get_or_init(|| {
        Regex::new(r"[A-Za-z0-9._%+-]+@[A-Za-z0-9.-]+\.[A-Za-z]{2,}").expect("valid email regex")
    });
    let phone_re =
        PHONE_RE.get_or_init(|| Regex::new(r"\b1[3-9]\d{9}\b").expect("valid phone regex"));

    let normalized = content.split_whitespace().collect::<Vec<_>>().join(" ");
    let value = secret_re.replace_all(&normalized, "$1=[REDACTED]");
    let value = token_re.replace_all(&value, "[REDACTED_TOKEN]");
    let value = email_re.replace_all(&value, "[REDACTED_EMAIL]");
    let value = phone_re.replace_all(&value, "[REDACTED_PHONE]");
    truncate_chars(&value, max_chars)
}

fn truncate_chars(value: &str, max_chars: usize) -> String {
    let mut chars = value.chars();
    let prefix: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_some() {
        format!("{prefix}...")
    } else {
        prefix
    }
}

fn mask_user_id(user_id: &str) -> String {
    let chars: Vec<char> = user_id.chars().collect();
    if chars.len() <= 4 {
        return "****".into();
    }
    let suffix: String = chars[chars.len() - 4..].iter().collect();
    format!("****{suffix}")
}

fn parse_sql_date(value: String) -> NaiveDate {
    NaiveDate::parse_from_str(&value, "%Y-%m-%d").unwrap_or(NaiveDate::MIN)
}

fn timestamp_string(timestamp_ms: i64, timezone: Tz) -> String {
    DateTime::<Utc>::from_timestamp_millis(timestamp_ms)
        .unwrap_or(DateTime::<Utc>::UNIX_EPOCH)
        .with_timezone(&timezone)
        .to_rfc3339()
}

fn percent(numerator: u64, denominator: u64) -> f64 {
    if denominator == 0 {
        0.0
    } else {
        (numerator as f64 / denominator as f64 * 10_000.0).round() / 100.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn test_config() -> AnalyticsConfig {
        AnalyticsConfig {
            max_storage_mb: 64,
            ..AnalyticsConfig::default()
        }
    }

    fn wait_for_writer() {
        std::thread::sleep(Duration::from_millis(180));
    }

    #[test]
    fn preview_redacts_sensitive_values_and_is_utf8_safe() {
        let value = sanitize_preview(
            "联系 foo@example.com 或 13800138000，token=secret-value 中文内容继续",
            42,
        );
        assert!(!value.contains("foo@example.com"));
        assert!(!value.contains("13800138000"));
        assert!(!value.contains("secret-value"));
        assert!(value.is_char_boundary(value.len()));
    }

    #[test]
    fn request_round_trip_aggregates_without_raw_content() {
        let temp = TempDir::new().unwrap();
        let analytics = UsageAnalytics::new(temp.path(), test_config());
        let request = analytics.begin_request(
            UsageSource::Interactive,
            InteractionKind::Question,
            "staff-123456",
            "测试用户",
            "ws",
            "cli",
            "direct",
            "查询 foo@example.com 的余额",
        );
        analytics.finish_request(
            &request,
            UsageFinish {
                response: "处理完成".into(),
                has_response: true,
                duration_ms: 25,
                prompt_tokens: 10,
                completion_tokens: 4,
                tools: vec![UsageToolEvent {
                    scope: "main".into(),
                    name: "read_file".into(),
                    status: "ok".into(),
                    route: "sandbox".into(),
                    risk_level: "read".into(),
                    duration_ms: 5,
                }],
                ..UsageFinish::default()
            },
        );
        wait_for_writer();
        let today = analytics.today();
        let report = analytics
            .query(&AnalyticsQuery {
                from: today,
                to: today,
                workspace: None,
                channel: None,
                source: None,
            })
            .unwrap();
        assert_eq!(report.summary.requests, 1);
        assert_eq!(report.summary.active_users, 1);
        assert_eq!(report.summary.answers, 1);
        assert_eq!(report.summary.tool_calls, 1);
        assert_eq!(report.recent.len(), 1);
        assert!(!report.recent[0].request_preview.contains("foo@example.com"));
    }

    #[test]
    fn session_timeout_survives_restart() {
        let temp = TempDir::new().unwrap();
        {
            let analytics = UsageAnalytics::new(temp.path(), test_config());
            let request = analytics.begin_request(
                UsageSource::Interactive,
                InteractionKind::Question,
                "user",
                "name",
                "ws",
                "cli",
                "direct",
                "one",
            );
            analytics.finish_request(&request, UsageFinish::default());
            wait_for_writer();
        }
        let analytics = UsageAnalytics::new(temp.path(), test_config());
        let request = analytics.begin_request(
            UsageSource::Interactive,
            InteractionKind::Question,
            "user",
            "name",
            "ws",
            "cli",
            "direct",
            "two",
        );
        analytics.finish_request(&request, UsageFinish::default());
        wait_for_writer();
        let today = analytics.today();
        let report = analytics
            .query(&AnalyticsQuery {
                from: today,
                to: today,
                workspace: None,
                channel: None,
                source: None,
            })
            .unwrap();
        assert_eq!(report.summary.requests, 2);
        assert_eq!(report.summary.sessions, 1);
    }

    #[test]
    fn automated_requests_do_not_create_users_or_sessions() {
        let temp = TempDir::new().unwrap();
        let analytics = UsageAnalytics::new(temp.path(), test_config());
        let request = analytics.begin_request(
            UsageSource::Automated,
            InteractionKind::Question,
            "timer-user",
            "timer",
            "ws",
            "cli",
            "direct",
            "scheduled",
        );
        analytics.finish_request(&request, UsageFinish::default());
        wait_for_writer();
        let today = analytics.today();
        let report = analytics
            .query(&AnalyticsQuery {
                from: today,
                to: today,
                workspace: None,
                channel: None,
                source: None,
            })
            .unwrap();
        assert_eq!(report.summary.automated_requests, 1);
        assert_eq!(report.summary.active_users, 0);
        assert_eq!(report.summary.sessions, 0);
        assert_eq!(report.summary.questions, 0);
        assert_eq!(report.summary.answers, 0);
    }

    #[test]
    fn duplicate_finish_is_idempotent() {
        let temp = TempDir::new().unwrap();
        let analytics = UsageAnalytics::new(temp.path(), test_config());
        let request = analytics.begin_request(
            UsageSource::Interactive,
            InteractionKind::Question,
            "user",
            "name",
            "ws",
            "cli",
            "direct",
            "question",
        );
        let finish = UsageFinish {
            response: "answer".into(),
            has_response: true,
            ..UsageFinish::default()
        };
        analytics.finish_request(&request, finish.clone());
        analytics.finish_request(&request, finish);
        wait_for_writer();

        let today = analytics.today();
        let report = analytics
            .query(&AnalyticsQuery {
                from: today,
                to: today,
                workspace: None,
                channel: None,
                source: None,
            })
            .unwrap();
        assert_eq!(report.summary.requests, 1);
        assert_eq!(report.summary.answers, 1);
    }

    #[test]
    fn unavailable_directory_keeps_callers_non_failing() {
        let temp = TempDir::new().unwrap();
        std::fs::write(temp.path().join("analytics"), "not a directory").unwrap();
        let analytics = UsageAnalytics::new(temp.path(), test_config());
        assert!(analytics.health().enabled);
        assert!(!analytics.health().available);

        let request = analytics.begin_request(
            UsageSource::Interactive,
            InteractionKind::Question,
            "user",
            "name",
            "ws",
            "cli",
            "direct",
            "question",
        );
        analytics.finish_request(&request, UsageFinish::default());
        assert!(analytics
            .query(&AnalyticsQuery {
                from: analytics.today(),
                to: analytics.today(),
                workspace: None,
                channel: None,
                source: None,
            })
            .is_err());
    }

    #[test]
    fn session_timeout_starts_a_new_persisted_session() {
        let temp = TempDir::new().unwrap();
        let analytics = UsageAnalytics::new(temp.path(), test_config());
        let finish = UsageFinish::default();
        let first = analytics.begin_request(
            UsageSource::Interactive,
            InteractionKind::Question,
            "user",
            "name",
            "ws",
            "cli",
            "direct",
            "one",
        );
        analytics.finish_request(&first, finish.clone());

        let key = hash_session_key("user", "cli", "direct");
        analytics
            .inner
            .sessions
            .lock()
            .get_mut(&key)
            .unwrap()
            .last_seen_ms = Utc::now().timestamp_millis() - 31 * 60_000;
        let second = analytics.begin_request(
            UsageSource::Interactive,
            InteractionKind::Resume,
            "user",
            "name",
            "ws",
            "cli",
            "direct",
            "two",
        );
        analytics.finish_request(&second, finish);
        wait_for_writer();

        let today = analytics.today();
        let report = analytics
            .query(&AnalyticsQuery {
                from: today,
                to: today,
                workspace: None,
                channel: None,
                source: None,
            })
            .unwrap();
        assert_eq!(report.summary.sessions, 2);
    }

    #[test]
    fn retention_removes_old_detail_but_keeps_valid_aggregate() {
        let temp = TempDir::new().unwrap();
        let db_path = temp.path().join("usage.sqlite3");
        let mut connection = Connection::open(&db_path).unwrap();
        configure_writer_connection(&connection).unwrap();
        init_schema(&connection).unwrap();
        let config = test_config();
        let today = Utc::now()
            .with_timezone(&chrono_tz::Asia::Shanghai)
            .date_naive();
        let old_detail = today - ChronoDuration::days(90);
        let valid_aggregate = today - ChronoDuration::days(200);
        let expired_aggregate = today - ChronoDuration::days(400);
        connection
            .execute(
                "INSERT INTO usage_requests (
                request_id, started_at_ms, local_date, source, interaction_kind, status,
                user_id, user_name, workspace_key, channel, session_id
             ) VALUES ('old', 0, ?1, 'interactive', 'question', 'completed',
                'user', 'name', 'ws', 'cli', 'session')",
                params![old_detail.to_string()],
            )
            .unwrap();
        for date in [valid_aggregate, expired_aggregate] {
            connection
                .execute(
                    "INSERT INTO daily_usage(local_date, workspace_key, channel, source)
                 VALUES (?1, 'ws', 'cli', 'interactive')",
                    params![date.to_string()],
                )
                .unwrap();
        }
        let health = HealthState::default();
        run_maintenance(
            &mut connection,
            &db_path,
            chrono_tz::Asia::Shanghai,
            &config,
            &health,
        )
        .unwrap();

        let detail_count: u64 = connection
            .query_row("SELECT COUNT(*) FROM usage_requests", [], |row| row.get(0))
            .unwrap();
        let aggregate_dates: Vec<String> = connection
            .prepare("SELECT local_date FROM daily_usage ORDER BY local_date")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        assert_eq!(detail_count, 0);
        assert_eq!(aggregate_dates, vec![valid_aggregate.to_string()]);
    }

    #[test]
    fn quota_evicts_only_detail_rows() {
        let temp = TempDir::new().unwrap();
        let db_path = temp.path().join("usage.sqlite3");
        let mut connection = Connection::open(&db_path).unwrap();
        configure_writer_connection(&connection).unwrap();
        init_schema(&connection).unwrap();
        let today = Utc::now().date_naive().to_string();
        connection
            .execute(
                "INSERT INTO usage_requests (
                request_id, started_at_ms, local_date, source, interaction_kind, status,
                user_id, user_name, workspace_key, channel, session_id
             ) VALUES ('detail', 0, ?1, 'interactive', 'question', 'completed',
                'user', 'name', 'ws', 'cli', 'session')",
                params![today],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO daily_usage(local_date, workspace_key, channel, source, requests)
             VALUES (?1, 'ws', 'cli', 'interactive', 1)",
                params![today],
            )
            .unwrap();
        let health = HealthState::default();
        let config = AnalyticsConfig {
            max_storage_mb: 0,
            ..test_config()
        };
        enforce_storage_quota(&mut connection, &db_path, &config, &health).unwrap();

        let detail_count: u64 = connection
            .query_row("SELECT COUNT(*) FROM usage_requests", [], |row| row.get(0))
            .unwrap();
        let aggregate_count: u64 = connection
            .query_row("SELECT COUNT(*) FROM daily_usage", [], |row| row.get(0))
            .unwrap();
        assert_eq!(detail_count, 0);
        assert_eq!(aggregate_count, 1);
        assert_eq!(health.detail_evictions.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn invalid_aggregate_rows_fail_the_query_instead_of_looking_empty() {
        let temp = TempDir::new().unwrap();
        let connection = Connection::open(temp.path().join("usage.sqlite3")).unwrap();
        configure_writer_connection(&connection).unwrap();
        init_schema(&connection).unwrap();
        let today = Utc::now().date_naive();
        connection
            .execute(
                "INSERT INTO daily_usage(
                    local_date, workspace_key, channel, source, requests
                 ) VALUES (?1, 'ws', 'cli', 'interactive', 'invalid')",
                params![today.to_string()],
            )
            .unwrap();

        let result = load_daily_usage(
            &connection,
            &AnalyticsQuery {
                from: today,
                to: today,
                workspace: None,
                channel: None,
                source: None,
            },
        );
        assert!(result.is_err());
    }

    #[test]
    fn range_queries_keep_daily_series_points() {
        let temp = TempDir::new().unwrap();
        let analytics = UsageAnalytics::new(temp.path(), test_config());
        let today = analytics.today();
        let yesterday = today - ChronoDuration::days(1);
        let connection = Connection::open(&analytics.inner.db_path).unwrap();
        for (date, requests) in [(yesterday, 2), (today, 3)] {
            connection
                .execute(
                    "INSERT INTO daily_usage(
                        local_date, workspace_key, channel, source, requests
                     ) VALUES (?1, 'ws', 'cli', 'interactive', ?2)",
                    params![date.to_string(), requests],
                )
                .unwrap();
        }

        let report = analytics
            .query(&AnalyticsQuery {
                from: yesterday,
                to: today,
                workspace: None,
                channel: None,
                source: None,
            })
            .unwrap();
        assert_eq!(report.grain, "day");
        assert_eq!(report.summary.requests, 5);
        assert_eq!(report.series.len(), 2);
        assert_eq!(report.series[0].period_start, yesterday.to_string());
        assert_eq!(report.series[1].period_start, today.to_string());
    }

    #[test]
    fn full_queue_is_reported_without_blocking_producers() {
        let temp = TempDir::new().unwrap();
        let analytics = UsageAnalytics::new(temp.path(), test_config());
        let locker = Connection::open(&analytics.inner.db_path).unwrap();
        locker.execute_batch("BEGIN IMMEDIATE").unwrap();
        for index in 0..(WRITER_QUEUE_CAPACITY + WRITER_BATCH_SIZE + 512) {
            let _ = analytics.begin_request(
                UsageSource::Automated,
                InteractionKind::Question,
                "timer",
                "timer",
                "ws",
                "cli",
                "direct",
                &format!("job-{index}"),
            );
        }
        assert!(analytics.health().dropped_events > 0);
        locker.execute_batch("ROLLBACK").unwrap();
    }
}
