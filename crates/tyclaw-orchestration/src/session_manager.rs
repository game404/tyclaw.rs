//! 会话管理器 —— JSONL 格式的对话历史持久化。
//!
//! 每个 workspace 对应一个 history.jsonl 文件，第一行是元数据，后续行是消息记录。
//! Session 通过 workspace_key 标识，路径由 WorkspaceManager 提供。

use chrono::{DateTime, Utc};
use serde_json::Value;
use std::collections::HashMap;
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::time::Instant;
use parking_lot::Mutex;
use tracing::{info, warn};
use uuid::Uuid;

use tyclaw_memory::{consolidate_with_provider, MemoryConsolidator};
use tyclaw_provider::LLMProvider;

use crate::config::SizeLimitConfig;
use crate::history::adjust_truncation_boundary;

/// 生成 session ID：`s_{YYYYMMDD}_{HHmmss}_{4位hex}`
fn generate_session_id() -> String {
    let now = Utc::now();
    let short_id = &Uuid::new_v4().to_string()[..4];
    format!("s_{}_{}", now.format("%Y%m%d_%H%M%S"), short_id)
}

/// 对话会话 —— 消息追加模式。
#[derive(Debug, Clone)]
pub struct Session {
    /// workspace key（标识归属的 workspace）
    pub workspace_key: String,
    /// 当前 session ID（每次唤醒生成新的）
    pub session_id: String,
    pub messages: Vec<HashMap<String, Value>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub metadata: HashMap<String, Value>,
    pub last_consolidated: usize,
}

impl Session {
    pub fn new(workspace_key: String) -> Self {
        let now = Utc::now();
        Self {
            workspace_key,
            session_id: generate_session_id(),
            messages: Vec::new(),
            created_at: now,
            updated_at: now,
            metadata: HashMap::new(),
            last_consolidated: 0,
        }
    }

    /// 添加一条消息到会话中。
    pub fn add_message(&mut self, role: &str, content: &str) {
        let mut msg = HashMap::new();
        msg.insert("role".into(), Value::String(role.into()));
        msg.insert("content".into(), Value::String(content.into()));
        msg.insert("timestamp".into(), Value::String(Utc::now().to_rfc3339()));
        self.messages.push(msg);
        self.updated_at = Utc::now();
    }

    /// 返回未合并的消息作为 LLM 输入。
    ///
    /// - `max_messages`: 最大返回消息数，0 表示返回所有未合并的消息
    /// - 严格保留切片后的原始顺序，不主动跳过开头非 user 消息。
    pub fn get_history(&self, max_messages: usize) -> Vec<HashMap<String, Value>> {
        // 防御越界：last_consolidated 可能因外部因素超过 messages.len()。
        let last_consolidated = self.last_consolidated.min(self.messages.len());
        let unconsolidated = &self.messages[last_consolidated..];
        let sliced = if max_messages > 0 && unconsolidated.len() > max_messages {
            &unconsolidated[unconsolidated.len() - max_messages..]
        } else {
            unconsolidated
        };
        sliced
            .iter()
            .map(|m| {
                let mut entry = HashMap::new();
                entry.insert(
                    "role".into(),
                    m.get("role").cloned().unwrap_or(Value::String("".into())),
                );
                entry.insert(
                    "content".into(),
                    m.get("content")
                        .cloned()
                        .unwrap_or(Value::String("".into())),
                );
                for k in &["tool_calls", "tool_call_id", "name"] {
                    if let Some(v) = m.get(*k) {
                        entry.insert(k.to_string(), v.clone());
                    }
                }
                entry
            })
            .collect()
    }

    /// 清除会话（`/new` 命令）。保留 session_id 不变。
    pub fn clear(&mut self) {
        self.messages.clear();
        self.last_consolidated = 0;
        self.updated_at = Utc::now();
    }
}

// ── 会话规模上限（Session Size Limits, R3）────────────────────────
//
// 超限处理顺序（design.md §3）：
//   1. 强制记忆合并：按时间顺序合并最早的消息（R3.2）。
//   2. 合并后仍超硬上限 → 滚动截断回落至 rolling_target（R3.3）。
//   3. Pairs_Fixes 超强制重置阈值 → 强制重置，仅保留最近一个完整 user turn（R3.5）。
// 截断/重置后统一调用 `history::adjust_truncation_boundary` 保证保留窗口不以
// 孤立 tool_result 开头，维持配对完整性（R3.6/R3.7）。

/// `enforce_size_limits` 实际执行的动作。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SizeLimitAction {
    /// 未触发任何动作（会话规模在阈值内）。
    None,
    /// 触发了强制记忆合并，`merged` 为本次合并的消息数。
    ForcedConsolidation { merged: usize },
    /// 触发了滚动截断，`kept` 为截断后保留的（活动）消息数。
    RollingTruncation { kept: usize },
    /// 触发了强制重置，`kept_last_turn` 为重置后保留的消息数（最近一个完整 user turn）。
    ForcedReset { kept_last_turn: usize },
}

// ── size-limit 动作审计记录（R3.8）────────────────────────────────
//
// 每次触发的 size-limit 动作对应一条审计记录，记录**触发原因**与 **Workspace 标识**。
// 触发原因与动作类型一一对应（design.md §3 处理顺序）：
//   - 强制合并 / 滚动截断：由「会话消息数超硬上限」触发 → MessageCountExceeded。
//   - 强制重置：由「Pairs_Fixes 超强制重置阈值」触发 → PairsFixesExceeded。
// 本节为纯函数化的审计记录构建器，便于属性测试（Property 13，task 5.9）。
// 实际写审计的接入点由编排层完成（task 19.2）。

/// size-limit 动作的触发原因（R3.8）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SizeLimitTrigger {
    /// 会话消息数超过硬上限（触发强制合并 / 滚动截断）。
    MessageCountExceeded,
    /// Pairs_Fixes 超过强制重置阈值（触发强制重置）。
    PairsFixesExceeded,
}

impl SizeLimitTrigger {
    /// 稳定的字符串标签，供日志/审计落地与跨进程序列化使用。
    pub fn as_str(&self) -> &'static str {
        match self {
            SizeLimitTrigger::MessageCountExceeded => "message-count-exceeded",
            SizeLimitTrigger::PairsFixesExceeded => "Pairs_Fixes-exceeded",
        }
    }
}

/// size-limit 动作类型标签（与 [`SizeLimitAction`] 变体一一对应，剥离数值载荷）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SizeLimitActionKind {
    ForcedConsolidation,
    RollingTruncation,
    ForcedReset,
}

impl SizeLimitActionKind {
    /// 稳定的字符串标签。
    pub fn as_str(&self) -> &'static str {
        match self {
            SizeLimitActionKind::ForcedConsolidation => "forced-consolidation",
            SizeLimitActionKind::RollingTruncation => "rolling-truncation",
            SizeLimitActionKind::ForcedReset => "forced-reset",
        }
    }
}

impl SizeLimitAction {
    /// 返回该动作的类型标签；`None` 动作（未触发）返回 `None`。
    pub fn kind(&self) -> Option<SizeLimitActionKind> {
        match self {
            SizeLimitAction::None => None,
            SizeLimitAction::ForcedConsolidation { .. } => {
                Some(SizeLimitActionKind::ForcedConsolidation)
            }
            SizeLimitAction::RollingTruncation { .. } => {
                Some(SizeLimitActionKind::RollingTruncation)
            }
            SizeLimitAction::ForcedReset { .. } => Some(SizeLimitActionKind::ForcedReset),
        }
    }
}

/// size-limit 动作审计记录（R3.8）。
///
/// 携带触发该动作的 **Workspace 标识**、**动作类型**与**触发原因**。
/// 不变式（Property 13）：`reason` 与 `action` 一一对应——
/// 强制合并 / 滚动截断 ↔ `MessageCountExceeded`；强制重置 ↔ `PairsFixesExceeded`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SizeLimitAuditRecord {
    /// 所属 Workspace 标识。
    pub workspace_key: String,
    /// 实际执行的动作类型。
    pub action: SizeLimitActionKind,
    /// 触发该动作的原因。
    pub reason: SizeLimitTrigger,
}

/// 由一个 [`SizeLimitAction`] 构建审计记录（R3.8）。
///
/// - `SizeLimitAction::None` → 返回 `None`（未触发任何动作，无需审计）。
/// - `ForcedConsolidation` / `RollingTruncation` → `reason = MessageCountExceeded`
///   （由会话消息数超硬上限触发）。
/// - `ForcedReset` → `reason = PairsFixesExceeded`（由 Pairs_Fixes 超强制重置阈值触发）。
///
/// 纯函数：相同输入恒得相同记录，便于属性测试（Property 13）。
pub fn build_size_limit_audit(
    workspace_key: &str,
    action: &SizeLimitAction,
) -> Option<SizeLimitAuditRecord> {
    let kind = action.kind()?;
    let reason = match kind {
        SizeLimitActionKind::ForcedConsolidation | SizeLimitActionKind::RollingTruncation => {
            SizeLimitTrigger::MessageCountExceeded
        }
        SizeLimitActionKind::ForcedReset => SizeLimitTrigger::PairsFixesExceeded,
    };
    Some(SizeLimitAuditRecord {
        workspace_key: workspace_key.to_string(),
        action: kind,
        reason,
    })
}

/// 读取 `messages[idx]` 的 role；越界或缺失时返回空串。
fn role_at(messages: &[HashMap<String, Value>], idx: usize) -> &str {
    messages
        .get(idx)
        .and_then(|m| m.get("role"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
}

/// 判断 `messages[idx]` 是否为一个 user turn 的起点（role == "user"）。
fn is_user_turn_start(messages: &[HashMap<String, Value>], idx: usize) -> bool {
    role_at(messages, idx) == "user"
}

/// 强制记忆合并边界（Property 8 / R3.2）。
///
/// 从 `last_consolidated` 起按时间顺序向后推进，选取一个 **user turn 起点**作为
/// 合并结束边界，使合并后剩余消息数（`messages.len() - boundary`）回落到
/// `target_remaining` 以内；在所有满足该条件的 user turn 起点中选取**最早**（最小）
/// 的一个，即只合并必要的最早消息。
///
/// 不变式与边界情况：
/// - 返回值恒 `>= last_consolidated`；当返回值 `> last_consolidated` 时，它一定
///   是一个 user turn 起点（合并从最早消息按时序推进）。
/// - 若没有任何 user turn 边界能使剩余数 `<= target_remaining`，返回最靠后的
///   user turn 起点（尽量多合并）。
/// - 若 `last_consolidated` 之后不存在 user turn 起点，返回 `last_consolidated`
///   （不推进，交由后续滚动截断处理）。
pub(crate) fn forced_consolidation_boundary(
    messages: &[HashMap<String, Value>],
    last_consolidated: usize,
    target_remaining: usize,
) -> usize {
    let len = messages.len();
    if last_consolidated >= len {
        return last_consolidated;
    }

    let mut last_user_boundary = last_consolidated;
    for idx in (last_consolidated + 1)..len {
        if is_user_turn_start(messages, idx) {
            last_user_boundary = idx;
            // 批次大小随 idx 单调递增 → 剩余数 (len - idx) 单调递减；
            // 第一个使剩余 <= target 的 user 起点即为最早满足者。
            if len - idx <= target_remaining {
                return idx;
            }
        }
    }

    last_user_boundary
}

/// 滚动截断边界（Property 9 / R3.3）。
///
/// 返回保留窗口在 `messages` 中的起始下标 `start`，使保留消息数
/// (`messages.len() - start`) `<= rolling_target`，且窗口**不以孤立 tool_result
/// 开头**。为同时满足两项约束，边界向**更晚**方向对齐到最近的 user turn 起点
/// （而非更早），必要时跳过开头连续的 tool 消息。
///
/// 这保证了 `messages.len() - start <= rolling_target`（回落到目标水位，R3.3）
/// 与「窗口首条非孤立 tool_result」（配对完整，R3.6）两项不变式同时成立；后续
/// 调用 `adjust_truncation_boundary` 仅作兜底，对本函数返回的干净边界为 no-op。
pub(crate) fn rolling_truncate_boundary(
    messages: &[HashMap<String, Value>],
    rolling_target: usize,
) -> usize {
    let len = messages.len();
    if rolling_target == 0 {
        return len;
    }
    if len <= rolling_target {
        return 0;
    }

    // 至少需要丢弃到 min_start，才能使保留数 <= rolling_target。
    let min_start = len - rolling_target;

    // 向更晚方向对齐到最近的 user turn 起点（保留数随之 <= rolling_target）。
    for idx in min_start..len {
        if is_user_turn_start(messages, idx) {
            return idx;
        }
    }

    // 窗口内无 user turn 起点：跳过开头连续的孤立 tool 消息，避免以 tool 开头。
    let mut idx = min_start;
    while idx < len && role_at(messages, idx) == "tool" {
        idx += 1;
    }
    idx
}

/// 强制重置边界（Property 10 / R3.5）。
///
/// 返回最近一个完整 user turn 的起点下标——即最后一条 `role == "user"` 消息的
/// 下标——使保留窗口仅含最近一个完整用户回合，且以 user 消息开头（无孤立
/// tool_result）。若不存在 user 消息，返回 0（保留全部，交由 adjust 兜底）。
pub(crate) fn forced_reset_boundary(messages: &[HashMap<String, Value>]) -> usize {
    for idx in (0..messages.len()).rev() {
        if is_user_turn_start(messages, idx) {
            return idx;
        }
    }
    0
}

/// RAII guard：持有期间 workspace 标记为 busy，drop 时自动 clear。
pub struct BusyGuard<'a> {
    sessions: &'a SessionManager,
    workspace_key: String,
}

impl<'a> Drop for BusyGuard<'a> {
    fn drop(&mut self) {
        self.sessions.clear_busy(&self.workspace_key);
    }
}

/// busy 超时上限：超过此时间仍为 busy 则视为异常，强制允许回收。
const BUSY_TIMEOUT_SECS: u64 = 1800; // 30 分钟

/// 会话活跃状态追踪。
struct ActiveSession {
    session: Session,
    last_access: Instant,
    /// 正在处理请求的起始时间，None 表示空闲。
    busy_since: Option<Instant>,
}

/// 会话管理器 —— 管理多个 workspace 的会话生命周期和持久化。
///
/// 通过 workspace_key 标识每个会话，历史文件路径由外部提供。
pub struct SessionManager {
    /// workspace_key → 活跃会话
    cache: Mutex<HashMap<String, ActiveSession>>,
    /// 用于从 workspace_key 计算 history.jsonl 路径
    root: PathBuf,
}

impl SessionManager {
    pub fn new(root: impl AsRef<Path>) -> Self {
        Self {
            cache: Mutex::new(HashMap::new()),
            root: root.as_ref().to_path_buf(),
        }
    }

    /// 获取 workspace 的 history.jsonl 路径。
    fn history_path(&self, workspace_key: &str) -> PathBuf {
        tyclaw_control::workspace_path(&self.root, workspace_key).join("history.jsonl")
    }

    /// 获取或创建会话（返回克隆），同时更新 last_access。
    pub fn get_or_create_clone(&self, workspace_key: &str) -> Session {
        let mut cache = self.cache.lock();
        if let Some(active) = cache.get_mut(workspace_key) {
            active.last_access = Instant::now();
            return active.session.clone();
        }
        let session = self
            .load(workspace_key)
            .unwrap_or_else(|| Session::new(workspace_key.into()));
        let cloned = session.clone();
        cache.insert(
            workspace_key.to_string(),
            ActiveSession {
                session,
                last_access: Instant::now(),
                busy_since: None,
            },
        );
        cloned
    }

    /// 刷新 workspace 的 last_access 时间戳（防止活跃任务被误回收）。
    pub fn touch(&self, workspace_key: &str) {
        let mut cache = self.cache.lock();
        if let Some(active) = cache.get_mut(workspace_key) {
            active.last_access = Instant::now();
        }
    }

    /// 标记 workspace 为忙碌状态（handle_with_context 开始时调用）。
    pub fn set_busy(&self, workspace_key: &str) {
        let mut cache = self.cache.lock();
        if let Some(active) = cache.get_mut(workspace_key) {
            active.busy_since = Some(Instant::now());
        }
    }

    /// 查询 workspace 的忙碌状态。
    ///
    /// 返回 `Some(elapsed)` 表示忙碌中（elapsed 为已忙碌时长），`None` 表示空闲。
    pub fn busy_elapsed(&self, workspace_key: &str) -> Option<std::time::Duration> {
        let cache = self.cache.lock();
        cache
            .get(workspace_key)
            .and_then(|a| a.busy_since)
            .filter(|since| since.elapsed().as_secs() < BUSY_TIMEOUT_SECS)
            .map(|since| since.elapsed())
    }

    /// 清除 workspace 的忙碌状态（handle_with_context 结束时调用）。
    pub fn clear_busy(&self, workspace_key: &str) {
        let mut cache = self.cache.lock();
        if let Some(active) = cache.get_mut(workspace_key) {
            active.busy_since = None;
            active.last_access = Instant::now();
        }
    }

    /// 创建一个 RAII guard，在 drop 时自动 clear_busy 并刷新 last_access。
    pub fn busy_guard(&self, workspace_key: &str) -> BusyGuard<'_> {
        self.set_busy(workspace_key);
        BusyGuard {
            sessions: self,
            workspace_key: workspace_key.to_string(),
        }
    }

    /// 获取 session_id（如果有活跃会话）。
    pub fn get_session_id(&self, workspace_key: &str) -> Option<String> {
        let cache = self.cache.lock();
        cache.get(workspace_key).map(|a| a.session.session_id.clone())
    }

    /// 从 JSONL 文件加载会话。
    fn load(&self, workspace_key: &str) -> Option<Session> {
        let path = self.history_path(workspace_key);
        if !path.exists() {
            return None;
        }

        let file = match std::fs::File::open(&path) {
            Ok(f) => f,
            Err(e) => {
                warn!("Failed to open session {}: {}", workspace_key, e);
                return None;
            }
        };

        let reader = std::io::BufReader::new(file);
        let mut messages = Vec::new();
        let mut metadata = HashMap::new();
        let mut created_at: Option<DateTime<Utc>> = None;
        let mut last_consolidated = 0usize;

        for line in reader.lines() {
            let line = match line {
                Ok(l) => l,
                Err(_) => continue,
            };
            let line = line.trim().to_string();
            if line.is_empty() {
                continue;
            }
            match serde_json::from_str::<HashMap<String, Value>>(&line) {
                Ok(data) => {
                    if data.get("_type").and_then(|v| v.as_str()) == Some("metadata") {
                        if let Some(m) = data.get("metadata") {
                            if let Ok(map) =
                                serde_json::from_value::<HashMap<String, Value>>(m.clone())
                            {
                                metadata = map;
                            }
                        }
                        if let Some(ts) = data.get("created_at").and_then(|v| v.as_str()) {
                            created_at = DateTime::parse_from_rfc3339(ts)
                                .ok()
                                .map(|dt| dt.with_timezone(&Utc));
                        }
                        if let Some(lc) = data.get("last_consolidated").and_then(|v| v.as_u64()) {
                            last_consolidated = lc as usize;
                        }
                    } else {
                        messages.push(data);
                    }
                }
                Err(e) => {
                    warn!("Failed to parse session line: {}", e);
                }
            }
        }

        // 防御性钳制：磁盘 metadata 里的 last_consolidated 可能与实际消息行数不一致
        // （文件被截断/手工编辑/写入中途崩溃），否则后续切片 messages[last_consolidated..] 会越界 panic。
        let last_consolidated = last_consolidated.min(messages.len());

        Some(Session {
            workspace_key: workspace_key.to_string(),
            session_id: generate_session_id(), // 每次加载生成新 session_id
            messages,
            created_at: created_at.unwrap_or_else(Utc::now),
            updated_at: Utc::now(),
            metadata,
            last_consolidated,
        })
    }

    /// 全量保存会话到 JSONL 文件（截断重写）。
    pub fn save(&self, session: &Session) -> std::io::Result<()> {
        let path = self.history_path(&session.workspace_key);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut file = std::fs::File::create(&path)?;

        let meta = serde_json::json!({
            "_type": "metadata",
            "workspace_key": session.workspace_key,
            "session_id": session.session_id,
            "created_at": session.created_at.to_rfc3339(),
            "updated_at": session.updated_at.to_rfc3339(),
            "metadata": session.metadata,
            "last_consolidated": session.last_consolidated,
        });
        writeln!(file, "{}", serde_json::to_string(&meta)?)?;

        for msg in &session.messages {
            writeln!(file, "{}", serde_json::to_string(msg)?)?;
        }

        let mut cache = self.cache.lock();
        if let Some(active) = cache.get_mut(&session.workspace_key) {
            active.session = session.clone();
            active.last_access = Instant::now();
        } else {
            cache.insert(
                session.workspace_key.clone(),
                ActiveSession {
                    session: session.clone(),
                    last_access: Instant::now(),
                    busy_since: None,
                },
            );
        }
        Ok(())
    }

    /// 追加消息到 JSONL 文件（O_APPEND 模式，并发安全）。
    pub fn append_messages(
        &self,
        workspace_key: &str,
        messages: &[HashMap<String, serde_json::Value>],
    ) -> std::io::Result<()> {
        use std::fs::OpenOptions;

        if messages.is_empty() {
            return Ok(());
        }

        let path = self.history_path(workspace_key);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let file_exists = path.exists();
        let mut file = OpenOptions::new().create(true).append(true).open(&path)?;

        if !file_exists {
            let session_id = self
                .get_session_id(workspace_key)
                .unwrap_or_else(generate_session_id);
            let meta = serde_json::json!({
                "_type": "metadata",
                "workspace_key": workspace_key,
                "session_id": session_id,
                "created_at": chrono::Utc::now().to_rfc3339(),
                "updated_at": chrono::Utc::now().to_rfc3339(),
                "metadata": {},
                "last_consolidated": 0,
            });
            writeln!(file, "{}", serde_json::to_string(&meta)?)?;
        }

        for msg in messages {
            writeln!(file, "{}", serde_json::to_string(msg)?)?;
        }
        // 先落盘（含 flush），再清缓存：保证清缓存之后的任何读取都能从磁盘读到完整、
        // 包含新追加消息的内容。若顺序相反，并发读取者可能在「已清缓存、未写完」的窗口
        // 读到旧磁盘内容并把陈旧副本重新写回缓存。
        use std::io::Write as _;
        file.flush()?;
        drop(file);
        {
            let mut cache = self.cache.lock();
            cache.remove(workspace_key);
        }

        Ok(())
    }

    /// 从缓存中移除会话。
    pub fn invalidate(&self, workspace_key: &str) {
        let mut cache = self.cache.lock();
        cache.remove(workspace_key);
    }

    // ── 超时回收 ──

    /// 返回所有超过 `timeout_secs` 未访问且非忙碌状态的 workspace key。
    /// busy 超过 BUSY_TIMEOUT_SECS 视为异常，强制允许回收。
    pub fn find_idle_workspaces(&self, timeout_secs: u64) -> Vec<String> {
        let cache = self.cache.lock();
        let now = Instant::now();
        cache
            .iter()
            .filter(|(_, active)| {
                let is_busy = active.busy_since.map_or(false, |since| {
                    now.duration_since(since).as_secs() < BUSY_TIMEOUT_SECS
                });
                !is_busy
                    && now.duration_since(active.last_access).as_secs() >= timeout_secs
            })
            .map(|(key, _)| key.clone())
            .collect()
    }

    /// 从活跃缓存中移除指定 workspace（回收前调用）。
    /// 返回被移除的 Session（用于 consolidation 等收尾操作）。
    ///
    /// 调用方应在 evict 后执行回收流程：
    /// 1. consolidate 对话历史 → memory
    /// 2. 清空 history.jsonl
    /// 3. 清空 work/tmp/、work/dispatches/、work/attachments/
    /// 4. 销毁 Docker 容器
    pub fn evict(&self, workspace_key: &str) -> Option<Session> {
        let mut cache = self.cache.lock();
        cache.remove(workspace_key).map(|a| {
            info!(
                workspace_key,
                session_id = %a.session.session_id,
                "Session evicted"
            );
            a.session
        })
    }

    /// 当前活跃 workspace 数量。
    pub fn active_count(&self) -> usize {
        self.cache.lock().len()
    }

    /// 强制会话规模上限（R3.2/R3.3/R3.4/R3.5/R3.6）。
    ///
    /// 处理顺序（design.md §3）：
    /// 1. **Pairs_Fixes 告警**（R3.4）：`pairs_fixes > pairs_fixes_warn` 时输出 WARN 日志。
    /// 2. **强制重置**（R3.5）：`pairs_fixes > pairs_fixes_force_reset` 时，仅保留最近一个
    ///    完整 user turn，其余合并入记忆（best-effort）；此动作优先级最高，命中即返回。
    /// 3. **强制记忆合并**（R3.2）：活动消息数超 `max_messages` 时，按时间顺序合并最早的
    ///    消息，使剩余回落到 `rolling_target` 以内。
    /// 4. **滚动截断**（R3.3）：合并后仍超 `max_messages` 时，截断回落至 `rolling_target`。
    ///
    /// 截断/重置后统一调用 [`adjust_truncation_boundary`] 保证保留窗口不以孤立
    /// tool_result 开头，维持 tool_call 配对完整性（R3.6/R3.7）。
    ///
    /// 注意：本函数仅执行规模治理与（best-effort）合并；审计记录由编排层接入点完成
    /// （task 5.8 / 19.2）。
    pub async fn enforce_size_limits(
        session: &mut Session,
        pairs_fixes: usize,
        cfg: &SizeLimitConfig,
        consolidator: &MemoryConsolidator,
        provider: &dyn LLMProvider,
        model: &str,
    ) -> SizeLimitAction {
        // 1. Pairs_Fixes 告警（R3.4）：超过告警阈值即输出 WARN，记录 workspace。
        if pairs_fixes > cfg.pairs_fixes_warn {
            warn!(
                workspace_key = %session.workspace_key,
                pairs_fixes,
                warn_threshold = cfg.pairs_fixes_warn,
                "Pairs_Fixes exceeded warn threshold"
            );
        }

        // 2. 强制重置（R3.5）：Pairs_Fixes 超强制重置阈值 → 仅保留最近一个完整 user turn。
        if pairs_fixes > cfg.pairs_fixes_force_reset {
            let boundary = forced_reset_boundary(&session.messages);
            // adjust 兜底：保证不以孤立 tool_result 开头（boundary 已是 user 起点，通常 no-op）。
            let boundary = adjust_truncation_boundary(&session.messages, boundary);

            // 将 boundary 之前的消息合并入记忆（best-effort），随后从活动会话移除。
            // 合并失败时，转储被丢弃的消息到恢复文件，避免静默数据丢失。
            if boundary > 0 {
                let chunk = session.messages[..boundary].to_vec();
                let merged_ok =
                    consolidate_with_provider(&consolidator.store, &chunk, provider, model).await;
                if !merged_ok {
                    let dump = consolidator
                        .store
                        .dump_unrecoverable(&session.workspace_key, &chunk);
                    warn!(
                        workspace_key = %session.workspace_key,
                        dropped = chunk.len(),
                        dump = ?dump,
                        "Forced reset: consolidation failed, dropped messages dumped for recovery"
                    );
                }
            }
            session.messages.drain(..boundary);
            session.last_consolidated = 0;
            session.updated_at = Utc::now();
            let kept = session.messages.len();
            warn!(
                workspace_key = %session.workspace_key,
                pairs_fixes,
                reset_threshold = cfg.pairs_fixes_force_reset,
                kept,
                "Forced session reset due to Pairs_Fixes"
            );
            return SizeLimitAction::ForcedReset {
                kept_last_turn: kept,
            };
        }

        // 活动（未合并）消息数。
        let active_count = session
            .messages
            .len()
            .saturating_sub(session.last_consolidated);
        if active_count <= cfg.max_messages {
            return SizeLimitAction::None;
        }

        let mut action = SizeLimitAction::None;

        // 3. 强制记忆合并（R3.2）：合并最早消息，使剩余回落到 rolling_target 以内。
        let boundary = forced_consolidation_boundary(
            &session.messages,
            session.last_consolidated,
            cfg.rolling_target,
        );
        if boundary > session.last_consolidated {
            let chunk = session.messages[session.last_consolidated..boundary].to_vec();
            let merged_ok =
                consolidate_with_provider(&consolidator.store, &chunk, provider, model).await;
            if merged_ok {
                let merged = boundary - session.last_consolidated;
                session.last_consolidated = boundary;
                session.updated_at = Utc::now();
                action = SizeLimitAction::ForcedConsolidation { merged };
            } else {
                warn!(
                    workspace_key = %session.workspace_key,
                    "Forced consolidation failed; falling back to rolling truncation"
                );
            }
        }

        // 4. 滚动截断（R3.3）：合并后仍超硬上限 → 截断回落到 rolling_target。
        let active_count = session
            .messages
            .len()
            .saturating_sub(session.last_consolidated);
        if active_count > cfg.max_messages {
            let active = &session.messages[session.last_consolidated..];
            let rel_start = rolling_truncate_boundary(active, cfg.rolling_target);
            let abs_start = session.last_consolidated + rel_start;
            // adjust 兜底保证配对完整（R3.6/R3.7）；干净边界为 no-op。
            let abs_start = adjust_truncation_boundary(&session.messages, abs_start);
            session.messages.drain(..abs_start);
            session.last_consolidated = 0;
            session.updated_at = Utc::now();
            let kept = session.messages.len();
            action = SizeLimitAction::RollingTruncation { kept };
        }

        action
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_session_id_format() {
        let id = generate_session_id();
        assert!(id.starts_with("s_"));
        assert!(id.len() > 15);
    }

    #[test]
    fn test_session_add_and_history() {
        let mut session = Session::new("test_ws".into());
        assert!(session.session_id.starts_with("s_"));
        session.add_message("user", "hello");
        session.add_message("assistant", "hi");

        let history = session.get_history(0);
        assert_eq!(history.len(), 2);
        assert_eq!(history[0]["role"], "user");
        assert_eq!(history[1]["role"], "assistant");
    }

    #[test]
    fn test_session_clear() {
        let mut session = Session::new("test_ws".into());
        let sid = session.session_id.clone();
        session.add_message("user", "hello");
        session.clear();
        assert!(session.messages.is_empty());
        assert_eq!(session.last_consolidated, 0);
        // session_id 不变
        assert_eq!(session.session_id, sid);
    }

    #[test]
    fn test_session_manager_persistence() {
        let tmp = tempfile::TempDir::new().unwrap();
        let mgr = SessionManager::new(tmp.path());

        let mut session = mgr.get_or_create_clone("alice");
        session.add_message("user", "hello");
        session.add_message("assistant", "hi");
        mgr.save(&session).unwrap();

        mgr.invalidate("alice");
        let reloaded = mgr.get_or_create_clone("alice");
        assert_eq!(reloaded.messages.len(), 2);
        assert_eq!(reloaded.workspace_key, "alice");
    }

    #[test]
    fn test_find_idle_workspaces() {
        let tmp = tempfile::TempDir::new().unwrap();
        let mgr = SessionManager::new(tmp.path());

        // 访问一次，触发缓存
        let _ = mgr.get_or_create_clone("active_ws");

        // 超时 0 秒 → 所有都算 idle
        let idle = mgr.find_idle_workspaces(0);
        assert!(idle.contains(&"active_ws".to_string()));

        // 超时 9999 秒 → 没有 idle
        let idle = mgr.find_idle_workspaces(9999);
        assert!(idle.is_empty());
    }

    #[test]
    fn test_evict() {
        let tmp = tempfile::TempDir::new().unwrap();
        let mgr = SessionManager::new(tmp.path());
        let _ = mgr.get_or_create_clone("ws1");
        assert_eq!(mgr.active_count(), 1);

        let evicted = mgr.evict("ws1");
        assert!(evicted.is_some());
        assert_eq!(evicted.unwrap().workspace_key, "ws1");
        assert_eq!(mgr.active_count(), 0);
    }
}

#[cfg(test)]
mod size_limit_tests {
    use super::*;
    use async_trait::async_trait;
    use proptest::prelude::*;
    use serde_json::json;
    use std::collections::HashMap as Map;
    use tyclaw_provider::types::{ChatRequest, LLMResponse, ToolCallRequest};
    use tyclaw_types::TyclawError;

    fn msg(role: &str) -> HashMap<String, Value> {
        let mut m = HashMap::new();
        m.insert("role".to_string(), json!(role));
        m
    }

    fn tool_msg(id: &str) -> HashMap<String, Value> {
        let mut m = HashMap::new();
        m.insert("role".to_string(), json!("tool"));
        m.insert("tool_call_id".to_string(), json!(id));
        m
    }

    /// 构造由若干完整 user turn 组成的会话：每个 turn = [user, assistant, tool]。
    fn turns(n: usize) -> Vec<HashMap<String, Value>> {
        let mut v = Vec::new();
        for i in 0..n {
            v.push(msg("user"));
            v.push(msg("assistant"));
            v.push(tool_msg(&format!("call_{i}")));
        }
        v
    }

    /// 保留窗口是否以孤立 tool_result（role==tool）开头。
    fn starts_with_orphan_tool(messages: &[HashMap<String, Value>]) -> bool {
        messages
            .first()
            .and_then(|m| m.get("role"))
            .and_then(|v| v.as_str())
            == Some("tool")
    }

    // ── 纯边界函数 ──────────────────────────────────────────

    #[test]
    fn forced_consolidation_advances_to_user_turn_start() {
        // 10 个 turn（30 条），目标剩余 9 条 → 合并到使剩余 <= 9 的最早 user 起点。
        let messages = turns(10);
        let boundary = forced_consolidation_boundary(&messages, 0, 9);
        assert!(boundary > 0);
        assert!(is_user_turn_start(&messages, boundary));
        assert!(messages.len() - boundary <= 9);
    }

    #[test]
    fn forced_consolidation_no_progress_without_user_turn() {
        // 无 user 起点（首条已过）：assistant, tool, tool。
        let messages = vec![msg("assistant"), tool_msg("a"), tool_msg("b")];
        assert_eq!(forced_consolidation_boundary(&messages, 0, 1), 0);
    }

    proptest! {
        #![proptest_config(ProptestConfig { cases: 100, ..Default::default() })]

        // Feature: execution-performance-optimization, Property 8: 强制合并边界自最早消息按时序推进
        #[test]
        fn prop_forced_consolidation_boundary_advances_earliest(
            // 每个 user turn 的尾随（非 user）消息数：1..12 个 turn，每个 0..4 条尾随。
            trailing in proptest::collection::vec(0usize..4, 1..12usize),
            target_remaining in 0usize..40usize,
            lc_pick in 0usize..1000usize,
        ) {
            // 构造会话：每个 turn = [user, 然后 0..k 个 assistant/tool]，按时序排列。
            let mut msgs: Vec<HashMap<String, Value>> = Vec::new();
            let mut turn_starts: Vec<usize> = Vec::new();
            for (i, &k) in trailing.iter().enumerate() {
                turn_starts.push(msgs.len());
                msgs.push(msg("user"));
                for j in 0..k {
                    if j % 2 == 0 {
                        msgs.push(msg("assistant"));
                    } else {
                        msgs.push(tool_msg(&format!("call_{i}_{j}")));
                    }
                }
            }
            let len = msgs.len();

            // last_consolidated 对齐到某个 user turn 起点（含 0，首条恒为 user）。
            let last_consolidated = turn_starts[lc_pick % turn_starts.len()];

            let boundary =
                forced_consolidation_boundary(&msgs, last_consolidated, target_remaining);

            // 不变式 1：边界恒 >= last_consolidated（只向更晚方向推进）。
            prop_assert!(boundary >= last_consolidated);

            // 不变式 2：推进时一定落在 user turn 起点（按时序前进的 user 边界）。
            if boundary > last_consolidated {
                prop_assert!(is_user_turn_start(&msgs, boundary));
            }

            // 不变式 3（最早/单调）：若存在 user 起点 s ∈ (last_consolidated, len]
            // 使 len - s <= target_remaining，则 boundary 为最小（最早）的这样的 s。
            let earliest_qualifying = turn_starts
                .iter()
                .copied()
                .filter(|&s| s > last_consolidated && len - s <= target_remaining)
                .min();
            if let Some(expected) = earliest_qualifying {
                prop_assert_eq!(boundary, expected);
            }
        }
    }

    proptest! {
        #![proptest_config(ProptestConfig { cases: 100, ..Default::default() })]

        // Feature: execution-performance-optimization, Property 9: 滚动截断回落至目标水位以内
        #[test]
        fn prop_rolling_truncate_falls_back_within_target(
            // 每个 user turn 的尾随（非 user）消息数：1..=20 个 turn，每个 0..4 条尾随。
            trailing in proptest::collection::vec(0usize..4, 1..=20usize),
            rolling_target in 1usize..=60usize,
        ) {
            // 构造会话：每个 turn = [user, 然后 0..k 个 assistant/tool]，按时序排列。
            let mut msgs: Vec<HashMap<String, Value>> = Vec::new();
            for (i, &k) in trailing.iter().enumerate() {
                msgs.push(msg("user"));
                for j in 0..k {
                    if j % 2 == 0 {
                        msgs.push(msg("assistant"));
                    } else {
                        msgs.push(tool_msg(&format!("call_{i}_{j}")));
                    }
                }
            }
            let len = msgs.len();

            let start = rolling_truncate_boundary(&msgs, rolling_target);
            let kept = len - start;

            // 不变式 1（R3.3）：保留消息数回落至目标水位以内。
            prop_assert!(kept <= rolling_target, "kept {kept} > target {rolling_target}");

            // 不变式 2（R3.6）：保留窗口不以孤立 tool_result 开头。
            prop_assert!(!starts_with_orphan_tool(&msgs[start..]));

            // 边界：rolling_target >= len 时无需截断，start == 0。
            if rolling_target >= len {
                prop_assert_eq!(start, 0);
            }
        }
    }


    #[test]
    fn rolling_truncate_keeps_within_target_and_clean_start() {
        let messages = turns(10); // 30 条
        for target in [1usize, 3, 7, 12, 25, 29] {
            let start = rolling_truncate_boundary(&messages, target);
            let kept = messages.len() - start;
            assert!(kept <= target, "target {target}: kept {kept}");
            assert!(!starts_with_orphan_tool(&messages[start..]));
        }
    }

    #[test]
    fn rolling_truncate_noop_when_within_target() {
        let messages = turns(3); // 9 条
        assert_eq!(rolling_truncate_boundary(&messages, 100), 0);
    }

    #[test]
    fn forced_reset_keeps_last_user_turn() {
        let messages = turns(5); // turn 起点在 0,3,6,9,12
        let boundary = forced_reset_boundary(&messages);
        assert_eq!(boundary, 12);
        assert!(is_user_turn_start(&messages, boundary));
    }

    proptest! {
        #![proptest_config(ProptestConfig { cases: 100, ..Default::default() })]

        // Feature: execution-performance-optimization, Property 10: Pairs_Fixes 阈值触发对应动作
        #[test]
        fn prop_pairs_fixes_threshold_triggers_actions(
            // >=1 个完整 user turn（每个 turn = [user, assistant, tool]），保证存在 user 消息。
            n in 1usize..=12usize,
            // 阈值生成：warn ∈ [1,50]，force_reset = warn + [1,50]（恒满足 force_reset > warn）。
            warn in 1usize..=50usize,
            force_gap in 1usize..=50usize,
            // 任意 pairs_fixes 值，覆盖低于 warn、warn~force_reset、高于 force_reset 三段。
            pairs_fixes in 0usize..=200usize,
        ) {
            let cfg = SizeLimitConfig {
                max_messages: 500,
                rolling_target: 400,
                pairs_fixes_warn: warn,
                pairs_fixes_force_reset: warn + force_gap,
            };
            let msgs = turns(n);

            // 参考决策布尔值（R3.4 / R3.5）：阈值为「超过」语义（>）。
            // warn_triggered 对应 R3.4（pairs_fixes > warn 记录 WARN）；
            // force_reset_triggered 对应 R3.5（pairs_fixes > force_reset 触发强制重置）。
            let warn_triggered = pairs_fixes > cfg.pairs_fixes_warn;
            let force_reset_triggered = pairs_fixes > cfg.pairs_fixes_force_reset;

            // 决策层级一致（R3.4/R3.5）：force_reset 阈值恒高于 warn 阈值 →
            // pairs_fixes > force_reset 蕴含 pairs_fixes > warn（reset ⇒ warn）。
            prop_assert!(!force_reset_triggered || warn_triggered);

            // 触发强制重置时（R3.5）：保留序列起点为最近一个完整 user 回合起点。
            if force_reset_triggered {
                let boundary = forced_reset_boundary(&msgs);

                // 首条保留消息为 user（不以孤立 tool_result 开头）。
                prop_assert!(is_user_turn_start(&msgs, boundary));
                prop_assert_eq!(role_at(&msgs, boundary), "user");
                prop_assert!(!starts_with_orphan_tool(&msgs[boundary..]));

                // boundary 恰为最后一条 user 消息下标（最近一个完整 user 回合）。
                let last_user = (0..msgs.len())
                    .rev()
                    .find(|&i| role_at(&msgs, i) == "user")
                    .expect("at least one user message exists");
                prop_assert_eq!(boundary, last_user);
                // turns(n) 的最后一个 user 位于 3*(n-1)。
                prop_assert_eq!(boundary, 3 * (n - 1));
            }
        }
    }

    // ── 异步 enforce_size_limits ──────────────────────────────

    /// 最小 Fake Provider：chat 返回一个 save_memory 工具调用，使
    /// `consolidate_with_provider` 成功（返回 true）。
    struct FakeProvider;

    #[async_trait]
    impl LLMProvider for FakeProvider {
        async fn chat(&self, _request: ChatRequest) -> Result<LLMResponse, TyclawError> {
            let mut args = Map::new();
            args.insert("history_entry".to_string(), json!("test history entry"));
            args.insert("memory_update".to_string(), json!("test memory"));
            Ok(LLMResponse {
                content: None,
                tool_calls: vec![ToolCallRequest {
                    id: "tc_1".into(),
                    name: "save_memory".into(),
                    arguments: args,
                }],
                finish_reason: "tool_calls".into(),
                usage: Map::new(),
                reasoning_content: None,
            })
        }

        fn default_model(&self) -> &str {
            "fake-model"
        }
    }

    fn small_cfg() -> SizeLimitConfig {
        // 小阈值便于触发：硬上限 6，目标水位 3。
        SizeLimitConfig {
            max_messages: 6,
            rolling_target: 3,
            pairs_fixes_warn: 30,
            pairs_fixes_force_reset: 60,
        }
    }

    /// 初始化全局 prompt_store（consolidate_with_provider 需要
    /// `memory_consolidation_prompt`）。写入临时 prompts.yaml 后 init；
    /// OnceLock 幂等，多次调用仅首次生效。
    fn ensure_prompt_store() {
        use std::io::Write as _;
        let tmp = tempfile::TempDir::new().unwrap();
        let cfg_dir = tmp.path().join("config");
        std::fs::create_dir_all(&cfg_dir).unwrap();
        let mut f = std::fs::File::create(cfg_dir.join("prompts.yaml")).unwrap();
        writeln!(
            f,
            "memory_consolidation_prompt: |\n  Summarize the conversation and call save_memory."
        )
        .unwrap();
        tyclaw_prompt::prompt_store::init(tmp.path());
    }

    #[tokio::test]
    async fn enforce_noop_when_under_limit() {
        let tmp = tempfile::TempDir::new().unwrap();
        let consolidator = MemoryConsolidator::new(tmp.path(), 200_000);
        let provider = FakeProvider;

        let mut session = Session::new("ws".into());
        session.messages = turns(1); // 3 条 < 上限 6
        let before = session.messages.len();

        let action = SessionManager::enforce_size_limits(
            &mut session,
            0,
            &small_cfg(),
            &consolidator,
            &provider,
            "fake-model",
        )
        .await;

        assert_eq!(action, SizeLimitAction::None);
        assert_eq!(session.messages.len(), before);
    }

    #[tokio::test]
    async fn enforce_consolidates_then_truncates_to_target() {
        let tmp = tempfile::TempDir::new().unwrap();
        let consolidator = MemoryConsolidator::new(tmp.path(), 200_000);
        let provider = FakeProvider;
        ensure_prompt_store();

        let mut session = Session::new("ws".into());
        session.messages = turns(10); // 30 条 >> 上限 6
        let cfg = small_cfg();

        let action = SessionManager::enforce_size_limits(
            &mut session,
            0,
            &cfg,
            &consolidator,
            &provider,
            "fake-model",
        )
        .await;

        // 合并将剩余压到 rolling_target(3) 以内，因此不应再触发滚动截断。
        let active = session.messages.len() - session.last_consolidated;
        assert!(active <= cfg.max_messages, "active {active} > max");
        // 配对完整：活动窗口不以孤立 tool_result 开头。
        assert!(!starts_with_orphan_tool(&session.messages[session.last_consolidated..]));
        match action {
            SizeLimitAction::ForcedConsolidation { merged } => assert!(merged > 0),
            SizeLimitAction::RollingTruncation { kept } => assert!(kept <= cfg.rolling_target),
            other => panic!("unexpected action {other:?}"),
        }
    }

    #[tokio::test]
    async fn enforce_forced_reset_on_high_pairs_fixes() {
        let tmp = tempfile::TempDir::new().unwrap();
        let consolidator = MemoryConsolidator::new(tmp.path(), 200_000);
        let provider = FakeProvider;
        ensure_prompt_store();

        let mut session = Session::new("ws".into());
        session.messages = turns(5); // 最近 user turn 起点在 12
        let cfg = small_cfg();

        let action = SessionManager::enforce_size_limits(
            &mut session,
            cfg.pairs_fixes_force_reset + 1, // 触发强制重置
            &cfg,
            &consolidator,
            &provider,
            "fake-model",
        )
        .await;

        // 仅保留最近一个完整 user turn（3 条），首条为 user。
        assert_eq!(action, SizeLimitAction::ForcedReset { kept_last_turn: 3 });
        assert_eq!(session.messages.len(), 3);
        assert_eq!(session.last_consolidated, 0);
        assert_eq!(role_at(&session.messages, 0), "user");
    }

    // Feature: execution-performance-optimization, Property 10 (async leg):
    // pairs_fixes > force_reset 时 enforce_size_limits 返回 ForcedReset，
    // 且保留窗口首条消息 role == "user"（R3.5 retained-sequence start）。
    #[tokio::test]
    async fn enforce_forced_reset_window_starts_with_user_turn() {
        let tmp = tempfile::TempDir::new().unwrap();
        let consolidator = MemoryConsolidator::new(tmp.path(), 200_000);
        let provider = FakeProvider;
        ensure_prompt_store();

        let cfg = small_cfg();
        let mut session = Session::new("ws".into());
        session.messages = turns(4); // 最近 user turn 起点在 9

        let action = SessionManager::enforce_size_limits(
            &mut session,
            cfg.pairs_fixes_force_reset + 1, // pairs_fixes > force_reset → ForcedReset
            &cfg,
            &consolidator,
            &provider,
            "fake-model",
        )
        .await;

        // 返回 ForcedReset，保留窗口仅含最近一个完整 user turn，首条 role == "user"。
        assert_eq!(action, SizeLimitAction::ForcedReset { kept_last_turn: 3 });
        assert_eq!(session.messages.len(), 3);
        assert_eq!(role_at(&session.messages, 0), "user");
        assert!(!starts_with_orphan_tool(&session.messages));
    }

    // ── enforce_size_limits 截断/重置后配对完整性（Property 12 / R3.6）──

    /// 构造一个带 tool_calls 的 assistant 消息（声明若干 tool_call id）。
    fn assistant_with_tool_calls(ids: &[String]) -> HashMap<String, Value> {
        let mut m = HashMap::new();
        m.insert("role".to_string(), json!("assistant"));
        if !ids.is_empty() {
            let tcs: Vec<Value> = ids.iter().map(|id| json!({ "id": id })).collect();
            m.insert("tool_calls".to_string(), json!(tcs));
        }
        m
    }

    /// 构造由若干完整 user turn 组成、tool_call 与 tool_result 严格配对的会话。
    /// 每个 turn = `[user, assistant(tool_calls=ids), tool(id)*k]`，ids 全局唯一。
    fn paired_turns(trailing: &[usize]) -> Vec<HashMap<String, Value>> {
        let mut v = Vec::new();
        for (i, &k) in trailing.iter().enumerate() {
            v.push(msg("user"));
            let ids: Vec<String> = (0..k).map(|j| format!("call_{i}_{j}")).collect();
            v.push(assistant_with_tool_calls(&ids));
            for id in &ids {
                v.push(tool_msg(id));
            }
        }
        v
    }

    /// 配对完整性谓词（R3.6）：保留窗口
    /// (1) 不以孤立 tool_result（role==tool）开头；
    /// (2) 每条 tool_result 的 tool_call_id 都能在其之前的 assistant.tool_calls 中找到。
    fn pairing_intact(window: &[HashMap<String, Value>]) -> bool {
        let mut declared: std::collections::HashSet<String> = std::collections::HashSet::new();
        for (i, m) in window.iter().enumerate() {
            match m.get("role").and_then(|v| v.as_str()).unwrap_or("") {
                "assistant" => {
                    if let Some(tcs) = m.get("tool_calls").and_then(|v| v.as_array()) {
                        for tc in tcs {
                            if let Some(id) = tc.get("id").and_then(|v| v.as_str()) {
                                declared.insert(id.to_string());
                            }
                        }
                    }
                }
                "tool" => {
                    // 窗口首条为 tool → 必为孤立 tool_result（其 tool_call 在窗口之前）。
                    if i == 0 {
                        return false;
                    }
                    let id = m.get("tool_call_id").and_then(|v| v.as_str()).unwrap_or("");
                    if !declared.contains(id) {
                        return false;
                    }
                }
                _ => {}
            }
        }
        true
    }

    proptest! {
        #![proptest_config(ProptestConfig { cases: 100, ..Default::default() })]

        // Feature: execution-performance-optimization, Property 12: 截断/重置后保持配对完整性
        // Validates: Requirements 3.6
        #[test]
        fn prop_truncation_reset_preserves_pairing(
            // 1..=12 个完整 user turn，每个 turn 含 0..4 个配对的 tool_call/tool_result。
            trailing in proptest::collection::vec(0usize..4, 1..=12usize),
            // 小硬上限 + 目标水位，便于触发强制合并 / 滚动截断。
            max_messages in 2usize..=8usize,
            rolling_raw in 0usize..64usize,
            // 触发原因覆盖：pairs_fixes 横跨告警 / 强制重置阈值上下。
            pairs_fixes in 0usize..=120usize,
        ) {
            let rolling_target = 1 + rolling_raw % max_messages; // 1..=max_messages
            let cfg = SizeLimitConfig {
                max_messages,
                rolling_target,
                pairs_fixes_warn: 30,
                pairs_fixes_force_reset: 60,
            };

            let messages = paired_turns(&trailing);
            // 前置：构造的会话本身配对完整（窗口以 user 开头）。
            prop_assert!(pairing_intact(&messages));

            let rt = tokio::runtime::Runtime::new().unwrap();
            let (action, window_clean, window_paired) = rt.block_on(async {
                ensure_prompt_store();
                let tmp = tempfile::TempDir::new().unwrap();
                let consolidator = MemoryConsolidator::new(tmp.path(), 200_000);
                let provider = FakeProvider;

                let mut session = Session::new("ws".into());
                session.messages = messages.clone();

                let action = SessionManager::enforce_size_limits(
                    &mut session,
                    pairs_fixes,
                    &cfg,
                    &consolidator,
                    &provider,
                    "fake-model",
                )
                .await;

                // 活动（未合并）保留窗口 —— 即送入 Agent_Loop 的消息序列。
                let active = &session.messages[session.last_consolidated..];
                let clean = !starts_with_orphan_tool(active);
                let paired = pairing_intact(active);
                (action, clean, paired)
            });

            // 不变式（R3.6）：截断/重置后保留窗口不以孤立 tool_result 开头，
            // 且每条 tool_result 都有对应的 tool_call（配对完整）。
            prop_assert!(
                window_clean,
                "action {:?}: retained window begins with an orphan tool_result",
                action
            );
            prop_assert!(
                window_paired,
                "action {:?}: retained window has a tool_result without matching tool_call",
                action
            );
        }
    }

    // ── size-limit 动作审计记录（R3.8 / task 5.8）────────────────

    #[test]
    fn audit_none_action_produces_no_record() {
        // None 动作（未触发）→ 无审计记录。
        assert_eq!(build_size_limit_audit("ws", &SizeLimitAction::None), None);
    }

    #[test]
    fn audit_forced_consolidation_reason_is_message_count() {
        let action = SizeLimitAction::ForcedConsolidation { merged: 42 };
        let record = build_size_limit_audit("ws-a", &action).expect("record");
        assert_eq!(
            record,
            SizeLimitAuditRecord {
                workspace_key: "ws-a".to_string(),
                action: SizeLimitActionKind::ForcedConsolidation,
                reason: SizeLimitTrigger::MessageCountExceeded,
            }
        );
    }

    #[test]
    fn audit_rolling_truncation_reason_is_message_count() {
        let action = SizeLimitAction::RollingTruncation { kept: 400 };
        let record = build_size_limit_audit("ws-b", &action).expect("record");
        assert_eq!(record.workspace_key, "ws-b");
        assert_eq!(record.action, SizeLimitActionKind::RollingTruncation);
        assert_eq!(record.reason, SizeLimitTrigger::MessageCountExceeded);
    }

    #[test]
    fn audit_forced_reset_reason_is_pairs_fixes() {
        let action = SizeLimitAction::ForcedReset { kept_last_turn: 3 };
        let record = build_size_limit_audit("ws-c", &action).expect("record");
        assert_eq!(record.workspace_key, "ws-c");
        assert_eq!(record.action, SizeLimitActionKind::ForcedReset);
        assert_eq!(record.reason, SizeLimitTrigger::PairsFixesExceeded);
    }

    #[test]
    fn audit_carries_workspace_identifier() {
        // workspace_key 被原样携带到审计记录中。
        for ws in ["alice", "group_123", "ws-with-dash"] {
            let record =
                build_size_limit_audit(ws, &SizeLimitAction::RollingTruncation { kept: 1 })
                    .expect("record");
            assert_eq!(record.workspace_key, ws);
        }
    }

    #[test]
    fn audit_is_deterministic() {
        let action = SizeLimitAction::ForcedReset { kept_last_turn: 7 };
        let a = build_size_limit_audit("ws", &action);
        let b = build_size_limit_audit("ws", &action);
        assert_eq!(a, b);
    }

    #[test]
    fn audit_reason_consistent_with_action_kind() {
        // Property 13 的不变式（示例形式）：reason 与 action 类型一致。
        let cases = [
            SizeLimitAction::ForcedConsolidation { merged: 1 },
            SizeLimitAction::RollingTruncation { kept: 1 },
            SizeLimitAction::ForcedReset { kept_last_turn: 1 },
        ];
        for action in cases {
            let record = build_size_limit_audit("ws", &action).expect("record");
            // action 标签与原动作变体一致。
            assert_eq!(Some(record.action), action.kind());
            // reason 与 action 类型一一对应。
            match record.action {
                SizeLimitActionKind::ForcedConsolidation
                | SizeLimitActionKind::RollingTruncation => {
                    assert_eq!(record.reason, SizeLimitTrigger::MessageCountExceeded);
                }
                SizeLimitActionKind::ForcedReset => {
                    assert_eq!(record.reason, SizeLimitTrigger::PairsFixesExceeded);
                }
            }
        }
    }

    /// 生成任意 `SizeLimitAction`（含 `None`，数值载荷任意）。
    fn any_size_limit_action() -> impl Strategy<Value = SizeLimitAction> {
        prop_oneof![
            Just(SizeLimitAction::None),
            any::<usize>().prop_map(|merged| SizeLimitAction::ForcedConsolidation { merged }),
            any::<usize>().prop_map(|kept| SizeLimitAction::RollingTruncation { kept }),
            any::<usize>()
                .prop_map(|kept_last_turn| SizeLimitAction::ForcedReset { kept_last_turn }),
        ]
    }

    proptest! {
        #![proptest_config(ProptestConfig { cases: 100, ..Default::default() })]

        // Feature: execution-performance-optimization, Property 13: size-limit 动作审计原因与动作一致
        // Validates: Requirements 3.8
        #[test]
        fn prop_size_limit_audit_reason_consistent_with_action(
            workspace_key in ".*",
            action in any_size_limit_action(),
        ) {
            let record = build_size_limit_audit(&workspace_key, &action);
            match action.kind() {
                // None 动作（未触发）→ 无审计记录。
                None => {
                    prop_assert_eq!(record, None);
                }
                Some(kind) => {
                    let record = record.expect("non-None action must yield an audit record");
                    // 动作标签与原动作变体一致。
                    prop_assert_eq!(record.action, kind);
                    // Workspace 标识原样携带。
                    prop_assert_eq!(&record.workspace_key, &workspace_key);
                    // reason 与 action 类型一一对应：
                    //   强制合并 / 滚动截断 ↔ MessageCountExceeded；强制重置 ↔ PairsFixesExceeded。
                    let expected_reason = match kind {
                        SizeLimitActionKind::ForcedConsolidation
                        | SizeLimitActionKind::RollingTruncation => {
                            SizeLimitTrigger::MessageCountExceeded
                        }
                        SizeLimitActionKind::ForcedReset => SizeLimitTrigger::PairsFixesExceeded,
                    };
                    prop_assert_eq!(record.reason, expected_reason);
                }
            }
        }
    }
}
