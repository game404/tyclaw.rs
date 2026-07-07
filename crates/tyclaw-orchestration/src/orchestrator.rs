//! 编排器：将 上下文 → 循环 → 门禁 → 记忆 → 审计 → 会话 → 技能 串联起来。
//!
//! 端到端的消息处理流程（14 步）：
//!  1. 速率限制检查（滑动窗口，per-user + global）
//!  2. 获取用户角色（admin / user / viewer）
//!  3. 根据 workspace_id:channel:chat_id 获取或创建会话
//!  4. 处理斜杠命令（如 /new 清除会话并归档记忆）
//!  5. 合并前检查：若 token 超过上下文窗口 50%，自动合并旧消息
//!  6. 收集技能（内建 + 个人）和能力列表
//!  7. 检索相似历史案例（基于关键词匹配）
//!  8. 构建完整消息列表（系统提示 + 历史 + 当前用户消息）
//!  9. 运行 ReAct 循环引擎（AgentLoop）
//! 10. 保存新轮次消息到会话（截断大的工具结果、剥离运行时元数据）
//! 11. 合并后检查：再次检查是否需要合并
//! 12. 记录速率使用
//! 13. 写入审计日志
//! 14. 自动提取案例记录（若本次使用了工具）

use serde_json::Value;
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

use tyclaw_agent::AgentRuntime;
use tyclaw_prompt::ContextBuilder;
use tyclaw_provider::LLMProvider;

use crate::app_context::AppContext;
use crate::builder::OrchestratorBuilder;
#[cfg(test)]
use crate::helpers;
#[cfg(test)]
use crate::history;
use crate::persistence::PersistenceLayer;

/// 中枢编排器 —— 连接所有层的核心组件。
///
/// 持有所有子系统的引用，负责协调消息处理的完整生命周期。
pub struct Orchestrator {
    /// 不可变的应用级上下文（workspace/model/features 等），Arc 共享给 subtasks 等子系统
    pub(crate) app: Arc<AppContext>,
    pub(crate) provider: Arc<dyn LLMProvider>,
    pub(crate) runtime: Box<dyn AgentRuntime>,
    pub(crate) context: ContextBuilder,
    /// 有状态的持久化服务（会话/审计/案例/技能/合并/限流/工作区管理）
    pub(crate) persistence: PersistenceLayer,
    pub(crate) pending_files: Arc<tyclaw_tools::PendingFileStore>,
    pub(crate) pending_recommends: Arc<tyclaw_tools::PendingRecommendStore>,
    pub(crate) pending_ask_user:
        parking_lot::Mutex<HashMap<String, (String, Vec<HashMap<String, Value>>)>>,
    pub(crate) timer_service: Option<Arc<tyclaw_tools::timer::TimerService>>,
    pub(crate) active_tasks: Arc<parking_lot::Mutex<HashMap<String, ActiveTask>>>,
    pub(crate) sandbox_pool: Option<Arc<dyn tyclaw_sandbox::SandboxPool>>,
    /// Per-workspace 消息注入队列：workspace busy 时，新消息注入到运行中的 agent loop。
    pub(crate) injection_queues:
        parking_lot::Mutex<HashMap<String, tyclaw_agent::runtime::InjectionQueue>>,
    /// Per-conversation 取消令牌：外部（钉钉停止按钮 / 关键字 / SIGINT 等）据此中断
    /// 正在运行的 agent loop。handler 开始处理时注册、完成时移除。
    /// 键为 `conversation_key`（含 workspace_key 前缀），保证群聊停止不会误杀同一
    /// 用户的私聊任务。
    pub(crate) cancellations: parking_lot::Mutex<
        HashMap<String, tokio_util::sync::CancellationToken>,
    >,
    /// Per-workspace 当前运行中的会话归属：`workspace_key -> conversation_key`。
    /// 用于判断新到消息是否与正在运行的 loop 属于同一会话——同会话才注入，
    /// 不同会话走串行锁排队。
    pub(crate) active_conversations: parking_lot::Mutex<HashMap<String, String>>,
    /// Per-workspace 串行锁：同一用户工作区(私聊/群聊共享 history)同一时刻只允许
    /// 一个会话运行，不同会话在此排队，避免并发写同一份历史造成串扰。
    pub(crate) run_locks:
        parking_lot::Mutex<HashMap<String, std::sync::Arc<tokio::sync::Mutex<()>>>>,
}

/// 运行态会话键分隔符——控制字符 `\u{1}`，用户输入不会包含，避免与 channel/chat_id
/// 内容冲突。
pub(crate) const CONVERSATION_KEY_SEP: char = '\u{1}';

/// 构造运行态"会话键"。
///
/// 形如 `{workspace_key}\u{1}{channel}\u{1}{chat_id}`：
/// - 前缀 `workspace_key` 让 reaper 能按 workspace 前缀批量清理。
/// - `channel` + `chat_id` 保证私聊 / 群聊 / 不同群彼此唯一。
pub(crate) fn conversation_key(workspace_key: &str, channel: &str, chat_id: &str) -> String {
    format!("{workspace_key}{CONVERSATION_KEY_SEP}{channel}{CONVERSATION_KEY_SEP}{chat_id}")
}

/// 某 workspace 下所有 conversation_key 的公共前缀（供 reaper 前缀清理）。
pub(crate) fn conversation_key_prefix(workspace_key: &str) -> String {
    format!("{workspace_key}{CONVERSATION_KEY_SEP}")
}

/// RAII guard：持有期间在 `active_conversations` 中标记某 workspace 的当前运行会话，
/// drop 时自动清除。
pub(crate) struct ActiveConversationGuard<'a> {
    orch: &'a Orchestrator,
    workspace_key: String,
}

impl Drop for ActiveConversationGuard<'_> {
    fn drop(&mut self) {
        self.orch
            .active_conversations
            .lock()
            .remove(&self.workspace_key);
    }
}

/// 活跃任务条目
#[derive(Debug, Clone)]
pub struct ActiveTask {
    pub user_id: String,
    pub summary: String,
    pub started_at: Instant,
}

impl Orchestrator {
    /// 将活跃任务列表写入 .active_tasks.json 文件
    pub(crate) fn write_active_tasks_file(&self, tasks: &HashMap<String, ActiveTask>) {
        let entries: Vec<serde_json::Value> = tasks
            .values()
            .map(|t| {
                serde_json::json!({
                    "user_id": t.user_id,
                    "summary": t.summary,
                    "running_seconds": t.started_at.elapsed().as_secs(),
                })
            })
            .collect();
        let content = serde_json::to_string_pretty(&serde_json::json!({
            "active_tasks": entries,
            "updated_at": chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string(),
        }))
        .unwrap_or_default();
        let _ = std::fs::write(self.app.workspace.join(".active_tasks.json"), content);
    }

    /// 注册一个取消令牌，与 `conversation_key` 关联。handler 进入时调用。
    ///
    /// 若同会话已有令牌（典型情况：上一条消息还没跑完新消息又来了），
    /// 旧令牌会被覆盖丢弃——其对应的任务会失去被外部 cancel 的能力，
    /// 但串行锁保证同一会话不会同时运行两个任务，所以覆盖是安全的。
    pub(crate) fn register_cancellation(
        &self,
        conversation_key: &str,
    ) -> tokio_util::sync::CancellationToken {
        let token = tokio_util::sync::CancellationToken::new();
        self.cancellations
            .lock()
            .insert(conversation_key.to_string(), token.clone());
        token
    }

    /// 任务结束时清理令牌。
    pub(crate) fn clear_cancellation(&self, conversation_key: &str) {
        self.cancellations.lock().remove(conversation_key);
    }

    /// 外部入口：中断指定会话正在运行的 agent 任务。
    ///
    /// 返回 `true` 表示找到了正在运行的任务并已触发 cancel；`false` 表示当前
    /// 没有运行中的任务（已结束、未开始、或还在排队等待运行锁）。
    pub fn cancel(&self, conversation_key: &str) -> bool {
        let map = self.cancellations.lock();
        if let Some(token) = map.get(conversation_key) {
            token.cancel();
            true
        } else {
            false
        }
    }

    /// 获取（或创建）某 workspace 的串行运行锁。
    pub(crate) fn get_run_lock(
        &self,
        workspace_key: &str,
    ) -> std::sync::Arc<tokio::sync::Mutex<()>> {
        let mut locks = self.run_locks.lock();
        locks
            .entry(workspace_key.to_string())
            .or_insert_with(|| std::sync::Arc::new(tokio::sync::Mutex::new(())))
            .clone()
    }

    /// 判断某 workspace 当前运行中的会话是否就是 `conversation_key`。
    pub(crate) fn active_conversation_matches(
        &self,
        workspace_key: &str,
        conversation_key: &str,
    ) -> bool {
        self.active_conversations
            .lock()
            .get(workspace_key)
            .map(|c| c == conversation_key)
            .unwrap_or(false)
    }

    /// 标记某 workspace 的当前运行会话，返回在 drop 时自动清除标记的 guard。
    pub(crate) fn mark_active_conversation(
        &self,
        workspace_key: &str,
        conversation_key: &str,
    ) -> ActiveConversationGuard<'_> {
        self.active_conversations
            .lock()
            .insert(workspace_key.to_string(), conversation_key.to_string());
        ActiveConversationGuard {
            orch: self,
            workspace_key: workspace_key.to_string(),
        }
    }

    /// 外部入口：按请求身份（用户 / 通道 / 会话）解析 workspace_key 后取消。
    ///
    /// 钉钉关键字 / 卡片停止按钮用这个——它们持有的是 staff_id、channel、chat_id，
    /// 不知道内部的 workspace_key 是怎么拼出来的。这里走 WorkspaceManager 的
    /// resolve_key 策略，保证和 handler 进入时用的 key 完全一致。
    pub fn cancel_for_identity(
        &self,
        user_id: &str,
        channel: &str,
        chat_id: &str,
        conversation_id: Option<&str>,
    ) -> bool {
        let identity = tyclaw_control::RequestIdentity {
            user_id,
            channel,
            chat_id,
            conversation_id,
        };
        let workspace_key = self.persistence.workspace_mgr.resolve_key(&identity);
        let conv_key = conversation_key(&workspace_key, channel, chat_id);
        self.cancel(&conv_key)
    }

    /// 获取或创建指定 workspace 的注入队列。
    pub(crate) fn get_injection_queue(
        &self,
        workspace_key: &str,
    ) -> tyclaw_agent::runtime::InjectionQueue {
        let mut queues = self.injection_queues.lock();
        queues
            .entry(workspace_key.to_string())
            .or_insert_with(|| {
                std::sync::Arc::new(std::sync::Mutex::new(Vec::new()))
            })
            .clone()
    }

    /// 创建 Builder（SDK 场景推荐）。
    pub fn builder(
        provider: Arc<dyn LLMProvider>,
        workspace: impl AsRef<Path>,
    ) -> OrchestratorBuilder {
        OrchestratorBuilder::new(provider, workspace)
    }

    /// 创建新的编排器实例。
    ///
    /// 该方法保持兼容原有用法：默认启用审计、记忆、RBAC、限流，并注册默认工具集。
    pub fn new(
        provider: Arc<dyn LLMProvider>,
        workspace: impl AsRef<Path>,
        model: Option<String>,
        max_iterations: Option<usize>,
        context_window_tokens: Option<usize>,
        write_snapshot: bool,
        workspaces_config: Option<HashMap<String, tyclaw_control::WorkspaceConfig>>,
    ) -> Self {
        Self::builder(provider, workspace)
            .with_model_opt(model)
            .with_max_iterations_opt(max_iterations)
            .with_context_window_tokens_opt(context_window_tokens)
            .with_write_snapshot(write_snapshot)
            .with_workspaces_config_opt(workspaces_config)
            .build()
    }

    /// 创建新的编排器实例，支持子任务调度配置。
    pub fn new_with_subtasks(
        provider: Arc<dyn LLMProvider>,
        workspace: impl AsRef<Path>,
        model: Option<String>,
        max_iterations: Option<usize>,
        context_window_tokens: Option<usize>,
        write_snapshot: bool,
        workspaces_config: Option<HashMap<String, tyclaw_control::WorkspaceConfig>>,
        subtasks_config: crate::subtasks::SubtasksConfig,
    ) -> Self {
        Self::builder(provider, workspace)
            .with_model_opt(model)
            .with_max_iterations_opt(max_iterations)
            .with_context_window_tokens_opt(context_window_tokens)
            .with_write_snapshot(write_snapshot)
            .with_workspaces_config_opt(workspaces_config)
            .with_subtasks(subtasks_config)
            .build()
    }

    /// 获取不可变的应用级上下文。
    pub fn app(&self) -> &Arc<AppContext> {
        &self.app
    }

    pub fn timer_service(&self) -> Option<&Arc<tyclaw_tools::timer::TimerService>> {
        self.timer_service.as_ref()
    }

    /// 获取活跃任务列表（监控用）。
    pub fn active_tasks(&self) -> &Arc<parking_lot::Mutex<HashMap<String, ActiveTask>>> {
        &self.active_tasks
    }

    /// 获取持久化层引用（审计、技能等，监控用）。
    pub fn persistence(&self) -> &PersistenceLayer {
        &self.persistence
    }

    /// 覆盖 works 目录路径（对应 --works-dir 命令行参数）。
    pub fn set_works_dir(&mut self, path: std::path::PathBuf) {
        self.persistence.workspace_mgr.set_works_dir(&path);
        self.persistence.skills.set_works_dir(path);
    }

    /// 设置沙箱池（启动时由 main.rs 注入）。
    pub fn set_sandbox_pool(&mut self, pool: Arc<dyn tyclaw_sandbox::SandboxPool>) {
        self.sandbox_pool = Some(pool);
        // sandbox 模式下 LLM 的工具在容器内执行，路径应显示为 "." 而非 host 绝对路径
        self.context.set_display_workspace(".");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_conversation_key_distinguishes_private_and_group() {
        // 同一用户(staff)私聊 vs 群聊：workspace_key 相同(user_id 策略)，
        // 但 conversation_key 必须不同，否则会跨会话注入/串台。
        let ws = "staff123";
        let private = conversation_key(ws, "dingtalk_private", "staff123");
        let group = conversation_key(ws, "dingtalk_group", "cidAAA:staff123");
        assert_ne!(private, group, "私聊与群聊的会话键必须不同");

        // 不同群之间也必须互相区分。
        let group_b = conversation_key(ws, "dingtalk_group", "cidBBB:staff123");
        assert_ne!(group, group_b, "不同群的会话键必须不同");
    }

    #[test]
    fn test_conversation_key_prefixed_by_workspace_key() {
        // reaper 依赖该前缀不变量按 workspace 批量清理 conversation_key 索引的 map。
        let ws = "staff123";
        let conv = conversation_key(ws, "dingtalk_group", "cidAAA:staff123");
        let prefix = conversation_key_prefix(ws);
        assert!(conv.starts_with(&prefix), "会话键必须以 workspace 前缀开头");

        // 另一个用户的会话键不应命中本 workspace 的前缀。
        let other = conversation_key("staff999", "dingtalk_private", "staff999");
        assert!(
            !other.starts_with(&prefix),
            "其他用户的会话键不应被本 workspace 前缀匹配"
        );
    }

    fn msg(role: &str, content: &str) -> HashMap<String, serde_json::Value> {
        let mut m = HashMap::new();
        m.insert("role".into(), json!(role));
        m.insert("content".into(), json!(content));
        m
    }

    #[test]
    fn test_dedupe_history_only_removes_consecutive_duplicates() {
        let history = vec![
            msg("user", "hello"),
            msg("user", "hello"), // 连续重复，应该被去掉
            msg("assistant", "ok"),
            msg("user", "hello"), // 非连续重复，应该保留
        ];
        let deduped = history::dedupe_history(&history);
        assert_eq!(deduped.len(), 3);
        assert_eq!(deduped[0]["role"], "user");
        assert_eq!(deduped[1]["role"], "assistant");
        assert_eq!(deduped[2]["role"], "user");
    }

    #[test]
    fn test_dedupe_history_keeps_different_tool_calls() {
        let mut a1 = msg("assistant", "");
        a1.insert(
            "tool_calls".into(),
            json!([{"id":"tool_1","type":"function","function":{"name":"read_file","arguments":"{}"}}]),
        );
        let mut a2 = msg("assistant", "");
        a2.insert(
            "tool_calls".into(),
            json!([{"id":"tool_2","type":"function","function":{"name":"read_file","arguments":"{}"}}]),
        );

        let history = vec![a1, a2];
        let deduped = history::dedupe_history(&history);
        // tool_calls id 不同，不应被误去重
        assert_eq!(deduped.len(), 2);
    }

    #[test]
    fn test_trim_history_by_token_budget_keeps_latest() {
        let history = vec![
            msg("user", "first"),
            msg("assistant", "second"),
            msg("user", "third"),
        ];
        let trimmed = history::trim_history_by_token_budget(&history, 2);
        assert!(!trimmed.is_empty());
        // 至少包含最后一条
        assert_eq!(trimmed.last().unwrap()["content"], "third");
    }

    #[test]
    fn test_optimize_similar_cases_dedup_and_truncate() {
        let raw = "Case A\nCase A\nCase B\n";
        let optimized = helpers::optimize_similar_cases(raw, 10);
        assert!(optimized.contains("Case A"));
        assert!(optimized.contains("truncated"));
        // 行级去重后不应出现两次完全相同的 Case A
        assert_eq!(optimized.matches("Case A").count(), 1);
    }

    #[test]
    fn test_optimize_similar_cases_utf8_safe_truncate() {
        let raw = "案例一：中文内容\n案例二：继续排查";
        let optimized = helpers::optimize_similar_cases(raw, 7);
        assert!(optimized.contains("truncated"));
        assert!(optimized.is_char_boundary(optimized.len()));
    }

    #[test]
    fn test_enforce_tool_call_pairing_drops_orphan_tool_result() {
        let mut assistant = msg("assistant", "");
        assistant.insert(
            "tool_calls".into(),
            json!([{"id":"tool_ok","type":"function","function":{"name":"exec","arguments":"{}"}}]),
        );

        let mut valid_tool = msg("tool", "ok");
        valid_tool.insert("tool_call_id".into(), json!("tool_ok"));
        valid_tool.insert("name".into(), json!("exec"));

        let mut orphan_tool = msg("tool", "orphan");
        orphan_tool.insert("tool_call_id".into(), json!("tool_missing"));
        orphan_tool.insert("name".into(), json!("exec"));

        let history = vec![assistant, valid_tool, orphan_tool];
        let cleaned = history::enforce_tool_call_pairing(&history);
        assert_eq!(cleaned.len(), 2);
        assert_eq!(cleaned[0]["role"], "assistant");
        assert_eq!(cleaned[1]["role"], "tool");
        assert_eq!(cleaned[1]["tool_call_id"], "tool_ok");
    }

    #[test]
    fn test_compute_context_budget_plan_modes() {
        let p_debug = helpers::compute_context_budget_plan("帮我排查服务报错和 timeout");
        assert!(p_debug.max_cases_chars >= 3000);

        let p_follow = helpers::compute_context_budget_plan("继续刚才那个问题");
        assert!(p_follow.history_ratio >= 60);

        let p_code = helpers::compute_context_budget_plan("请实现一个重构方案");
        assert!(p_code.max_skills >= 10);
    }

    #[test]
    fn test_cross_round_tool_pairing_regression() {
        // Round 1: assistant(tool_a) -> tool_a result
        let mut a1 = msg("assistant", "");
        a1.insert(
            "tool_calls".into(),
            json!([{"id":"tool_a","type":"function","function":{"name":"list_dir","arguments":"{}"}}]),
        );
        let mut t1 = msg("tool", "result_a");
        t1.insert("tool_call_id".into(), json!("tool_a"));
        t1.insert("name".into(), json!("list_dir"));

        // Round 2: assistant(tool_b) -> tool_b result
        let mut a2 = msg("assistant", "");
        a2.insert(
            "tool_calls".into(),
            json!([{"id":"tool_b","type":"function","function":{"name":"read_file","arguments":"{}"}}]),
        );
        let mut t2 = msg("tool", "result_b");
        t2.insert("tool_call_id".into(), json!("tool_b"));
        t2.insert("name".into(), json!("read_file"));

        // 插入一条孤儿 tool（不在上一条 assistant 的 tool_calls 中），应被清理
        let mut orphan = msg("tool", "orphan");
        orphan.insert("tool_call_id".into(), json!("tool_x"));
        orphan.insert("name".into(), json!("exec"));

        let history = vec![
            msg("user", "round1"),
            a1,
            t1,
            msg("assistant", "after round1"),
            msg("user", "round2"),
            a2,
            t2,
            orphan,
            msg("assistant", "done"),
        ];

        // 模拟真实链路：先预算裁剪（给足预算不触发裁掉），再做配对修复
        let trimmed = history::trim_history_by_token_budget(&history, 10_000);
        let cleaned = history::enforce_tool_call_pairing(&trimmed);

        // 有效 tool 结果保留
        assert!(cleaned
            .iter()
            .any(|m| m.get("tool_call_id").and_then(|v| v.as_str()) == Some("tool_a")));
        assert!(cleaned
            .iter()
            .any(|m| m.get("tool_call_id").and_then(|v| v.as_str()) == Some("tool_b")));

        // 孤儿 tool 结果必须被移除
        assert!(!cleaned
            .iter()
            .any(|m| m.get("tool_call_id").and_then(|v| v.as_str()) == Some("tool_x")));
    }

    #[test]
    fn test_cross_round_pairing_under_tight_budget() {
        // 构造两轮 tool 调用，并让第一轮内容很长，逼迫预算裁剪时优先丢弃旧轮次。
        let mut a1 = msg("assistant", "");
        a1.insert(
            "tool_calls".into(),
            json!([{"id":"tool_old","type":"function","function":{"name":"list_dir","arguments":"{}"}}]),
        );
        let mut t1 = msg("tool", &"old_result ".repeat(200));
        t1.insert("tool_call_id".into(), json!("tool_old"));
        t1.insert("name".into(), json!("list_dir"));

        let mut a2 = msg("assistant", "");
        a2.insert(
            "tool_calls".into(),
            json!([{"id":"tool_new","type":"function","function":{"name":"read_file","arguments":"{}"}}]),
        );
        let mut t2 = msg("tool", "new_result");
        t2.insert("tool_call_id".into(), json!("tool_new"));
        t2.insert("name".into(), json!("read_file"));

        // 额外孤儿 tool，理论上必须清理
        let mut orphan = msg("tool", "orphan");
        orphan.insert("tool_call_id".into(), json!("tool_orphan"));
        orphan.insert("name".into(), json!("exec"));

        let history = vec![
            msg("user", "round_old"),
            a1,
            t1,
            msg("assistant", "after old"),
            msg("user", "round_new"),
            a2,
            t2,
            orphan,
            msg("assistant", "done"),
        ];

        // 小预算触发裁剪（这里只要求行为正确，不依赖精确 token 值）
        let trimmed = history::trim_history_by_token_budget(&history, 120);
        let cleaned = history::enforce_tool_call_pairing(&trimmed);

        // 不允许出现孤儿 tool 结果
        assert!(!cleaned.iter().any(|m| {
            m.get("role").and_then(|v| v.as_str()) == Some("tool")
                && m.get("tool_call_id").and_then(|v| v.as_str()) == Some("tool_orphan")
        }));

        // 如果存在 tool 消息，必须都能在"紧邻之前的 assistant.tool_calls"中找到配对 id
        let mut expected_ids = std::collections::HashSet::new();
        for m in &cleaned {
            let role = m.get("role").and_then(|v| v.as_str()).unwrap_or("");
            if role == "assistant" {
                expected_ids.clear();
                if let Some(tool_calls) = m.get("tool_calls").and_then(|v| v.as_array()) {
                    for tc in tool_calls {
                        if let Some(id) = tc.get("id").and_then(|v| v.as_str()) {
                            expected_ids.insert(id.to_string());
                        }
                    }
                }
            } else if role == "tool" {
                let id = m
                    .get("tool_call_id")
                    .and_then(|v| v.as_str())
                    .expect("tool must have tool_call_id");
                assert!(
                    expected_ids.contains(id),
                    "found unpaired tool_result id={id} after trimming"
                );
            } else {
                expected_ids.clear();
            }
        }

        // 一般情况下，最近轮次应保留；若预算极端导致无 tool，也应至少不报错。
        let has_new_pair = cleaned
            .iter()
            .any(|m| m.get("tool_call_id").and_then(|v| v.as_str()) == Some("tool_new"));
        if cleaned
            .iter()
            .any(|m| m.get("role").and_then(|v| v.as_str()) == Some("tool"))
        {
            assert!(
                has_new_pair,
                "when tool messages remain, latest pair should survive"
            );
        }
    }
}

