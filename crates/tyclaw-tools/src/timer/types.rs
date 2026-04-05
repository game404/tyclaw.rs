use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TimerSchedule {
    At { at_ms: i64 },
    Every { interval_ms: u64 },
    Cron { expr: String, tz: Option<String> },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TimerPayload {
    pub message: String,
    #[serde(default)]
    pub deliver: bool,
    pub channel: Option<String>,
    pub chat_id: Option<String>,
    pub workspace_id: Option<String>,
    /// 创建者的用户 ID（钉钉 staff_id），用于触发时发送回复
    pub user_id: String,
    /// 钉钉群会话 ID，群聊场景下用于发送回复
    pub conversation_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TimerRunRecord {
    pub run_at_ms: i64,
    pub status: String,
    #[serde(default)]
    pub duration_ms: u64,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TimerJobState {
    pub next_run_at_ms: Option<i64>,
    pub last_run_at_ms: Option<i64>,
    pub last_status: Option<String>,
    pub last_error: Option<String>,
    #[serde(default)]
    pub run_history: Vec<TimerRunRecord>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TimerJob {
    pub id: String,
    pub name: String,
    /// 所属用户 ID —— 定时任务按用户隔离的核心字段
    pub user_id: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
    pub schedule: TimerSchedule,
    pub payload: TimerPayload,
    #[serde(default)]
    pub state: TimerJobState,
    #[serde(default)]
    pub created_at_ms: i64,
    #[serde(default)]
    pub updated_at_ms: i64,
    #[serde(default)]
    pub delete_after_run: bool,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TimerStore {
    #[serde(default = "default_version")]
    pub version: u32,
    #[serde(default)]
    pub jobs: Vec<TimerJob>,
}

fn default_version() -> u32 {
    1
}

pub(crate) const MAX_RUN_HISTORY: usize = 20;
