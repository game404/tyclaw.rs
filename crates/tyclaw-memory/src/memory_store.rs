//! 记忆存储 —— MEMORY.md（长期记忆）+ HISTORY.md（可搜索日志）。
//!
//! 双层记忆系统：
//! - MEMORY.md：长期事实记忆，由 LLM 合并更新
//! - HISTORY.md：追加式日志，适合 grep 搜索

use std::path::{Path, PathBuf};
use tracing::{info, warn};

/// 双层记忆存储。
pub struct MemoryStore {
    memory_dir: PathBuf,
    memory_file: PathBuf,
    history_file: PathBuf,
}

impl MemoryStore {
    /// 创建 MemoryStore。`memory_dir` 是记忆存储目录（如 `workspaces/{key}/memory`）。
    pub fn new(memory_dir: &Path) -> Self {
        std::fs::create_dir_all(memory_dir).ok();
        Self {
            memory_file: memory_dir.join("MEMORY.md"),
            history_file: memory_dir.join("HISTORY.md"),
            memory_dir: memory_dir.to_path_buf(),
        }
    }

    /// 读取长期记忆。
    pub fn read_long_term(&self) -> String {
        if self.memory_file.exists() {
            std::fs::read_to_string(&self.memory_file).unwrap_or_default()
        } else {
            String::new()
        }
    }

    /// 写入长期记忆。
    pub fn write_long_term(&self, content: &str) -> std::io::Result<()> {
        std::fs::create_dir_all(&self.memory_dir)?;
        std::fs::write(&self.memory_file, content)
    }

    /// 将无法合并入记忆、又即将从活动会话中移除的消息转储到恢复文件，避免静默丢失。
    ///
    /// 用于强制重置或滚动截断时记忆合并失败的兜底：把被丢弃的消息以 JSONL 追加到
    /// `{memory_dir}/reset_dumps/{workspace_key}.jsonl`，便于事后人工恢复/排查。
    /// 返回写入的转储文件路径（失败返回 None 并已打日志）。
    pub fn dump_unrecoverable<T: serde::Serialize>(
        &self,
        workspace_key: &str,
        messages: &[T],
    ) -> Option<PathBuf> {
        use std::io::Write;
        if messages.is_empty() {
            return None;
        }
        let dir = self.memory_dir.join("reset_dumps");
        if let Err(e) = std::fs::create_dir_all(&dir) {
            warn!(dir = %dir.display(), error = %e, "Failed to create reset_dumps dir");
            return None;
        }
        // 文件名按 workspace 隔离；同一 workspace 多次失败追加到同一文件。
        let safe_key: String = workspace_key
            .chars()
            .map(|c| if c.is_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
            .collect();
        let path = dir.join(format!("{safe_key}.jsonl"));
        let ts = chrono::Local::now().to_rfc3339();
        let mut buf = String::new();
        for msg in messages {
            match serde_json::to_string(msg) {
                Ok(line) => buf.push_str(&format!("{{\"dumped_at\":\"{ts}\",\"message\":{line}}}\n")),
                Err(e) => warn!(error = %e, "Failed to serialize message for reset dump"),
            }
        }
        match std::fs::OpenOptions::new().create(true).append(true).open(&path) {
            Ok(mut file) => {
                if let Err(e) = file.write_all(buf.as_bytes()) {
                    warn!(path = %path.display(), error = %e, "Failed to write reset dump");
                    return None;
                }
                Some(path)
            }
            Err(e) => {
                warn!(path = %path.display(), error = %e, "Failed to open reset dump file");
                None
            }
        }
    }

    /// 追加历史日志。
    pub fn append_history(&self, entry: &str) -> std::io::Result<()> {
        use std::io::Write;
        std::fs::create_dir_all(&self.memory_dir)?;
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.history_file)?;
        writeln!(file, "{}\n", entry.trim_end())
    }

    /// 获取记忆上下文（用于注入系统提示）。
    ///
    /// 在 Long-term Memory 顶部加一段解读规则（护栏），防止 LLM 把"上次承诺但没做完"
    /// 误读成"当前正在做"从而继续输出承诺——典型失败模式：memory 里有
    /// "assistant acknowledged ... no results yet"，LLM 读到后这一轮又发"我来给你查"
    /// 然后 stop，没有任何 tool call，陷入自我强化循环。
    ///
    /// 护栏文本从 `prompts.yaml` 的 `memory_guard` 字段加载（需先 `init()` prompt_store）。
    pub fn get_memory_context(&self) -> String {
        let long_term = self.read_long_term();
        if long_term.is_empty() {
            String::new()
        } else {
            let guard = tyclaw_prompt::nudge_loader::memory_guard();
            format!("{guard}\n## Long-term Memory\n{long_term}")
        }
    }

    /// 将消息列表格式化为文本，用于 LLM 合并。
    fn format_messages(
        messages: &[std::collections::HashMap<String, serde_json::Value>],
    ) -> String {
        use serde_json::Value;
        let mut lines = Vec::new();
        for msg in messages {
            let content = match msg.get("content") {
                Some(Value::String(s)) if !s.is_empty() => s.as_str(),
                _ => continue,
            };
            let role = msg
                .get("role")
                .and_then(|v| v.as_str())
                .unwrap_or("?")
                .to_uppercase();
            let ts = msg.get("timestamp").and_then(|v| v.as_str()).unwrap_or("?");
            let ts_short = &ts[..ts.len().min(16)];
            lines.push(format!("[{ts_short}] {role}: {content}"));
        }
        lines.join("\n")
    }
}

/// 逐行剔除"工具参数机制/参数名纠错"类噪声，返回 `(清洗后文本, 剔除行数)`。
///
/// 目的：切断记忆投毒反馈环——模型偶发把 `path` 写成 `file_path` 后自我纠错，
/// 该纠错被合并进 `MEMORY.md`（条目里含 `file_path` 字样），此后每轮注入系统提示，
/// 否定式表述（"用 path，不是 file_path"）反而激活 `file_path` token，诱导模型继续犯错。
///
/// 保守策略：只剔除"明确的瞬时工具参数错误"或"同时命中参数机制词 + 工具/参数 token"的行，
/// 避免误伤合法业务事实（按行过滤，其余内容原样保留）。
pub fn sanitize_memory_text(text: &str) -> (String, usize) {
    let mut removed = 0usize;
    let kept: Vec<&str> = text
        .lines()
        .filter(|line| {
            if is_tool_param_noise(line) {
                removed += 1;
                false
            } else {
                true
            }
        })
        .collect();
    let mut out = kept.join("\n");
    // 保留原文结尾换行语义。
    if text.ends_with('\n') && !out.is_empty() {
        out.push('\n');
    }
    (out, removed)
}

/// 判断单行是否为"工具参数机制/参数名纠错"噪声。
fn is_tool_param_noise(line: &str) -> bool {
    let l = line.to_lowercase();
    // 1) 明确的瞬时工具参数错误，一律剔除。
    if l.contains("missing required parameter")
        || l.contains("missing 'path' parameter")
        || l.contains("missing \"path\" parameter")
    {
        return true;
    }
    // 2) 需同时命中"参数机制词" + "工具名/参数名 token" 才判噪声（保守，避免误伤业务事实）。
    let param_marker = ["参数", "参数名", "字段名", "parameter", "argument"]
        .iter()
        .any(|k| l.contains(k));
    let tool_token = [
        "file_path",
        "read_file",
        "write_file",
        "edit_file",
        "list_dir",
        "apply_patch",
        "grep_search",
        "delete_file",
        "send_file",
        "move_file",
        "copy_file",
    ]
    .iter()
    .any(|k| l.contains(k));
    param_marker && tool_token
}

/// save_memory 工具定义（用于 LLM 合并调用）。
pub fn save_memory_tool_def() -> serde_json::Value {
    serde_json::json!([{
        "type": "function",
        "function": {
            "name": "save_memory",
            "description": "Save the memory consolidation result to persistent storage.",
            "parameters": {
                "type": "object",
                "properties": {
                    "history_entry": {
                        "type": "string",
                        "description": "A paragraph summarizing key events/decisions/topics. Start with [YYYY-MM-DD HH:MM]. Include detail useful for grep search."
                    },
                    "memory_update": {
                        "type": "string",
                        "description": "Full updated long-term memory as markdown. Include all existing facts plus new ones. Return unchanged if nothing new."
                    }
                },
                "required": ["history_entry", "memory_update"]
            }
        }
    }])
}

/// 通过 LLM 进行记忆合并。
///
/// 将消息列表交给 LLM，由 LLM 调用 save_memory 工具写入 MEMORY.md 和 HISTORY.md。
pub async fn consolidate_with_provider(
    store: &MemoryStore,
    messages: &[std::collections::HashMap<String, serde_json::Value>],
    provider: &dyn tyclaw_provider::LLMProvider,
    model: &str,
) -> bool {
    if messages.is_empty() {
        return true;
    }

    let current_memory = store.read_long_term();
    let formatted = MemoryStore::format_messages(messages);
    let prompt = format!(
        "Process this conversation and call the save_memory tool.\n\n\
         ## Current Long-term Memory\n{}\n\n\
         ## Conversation to Process\n{}",
        if current_memory.is_empty() {
            "(empty)".to_string()
        } else {
            current_memory.clone()
        },
        formatted,
    );

    let tools_def = save_memory_tool_def();
    let tools_vec = tools_def.as_array().cloned().unwrap_or_default();

    let mut sys_msg = std::collections::HashMap::new();
    sys_msg.insert("role".into(), serde_json::Value::String("system".into()));
    sys_msg.insert(
        "content".into(),
        serde_json::Value::String(
            tyclaw_prompt::prompt_store::get("memory_consolidation_prompt"),
        ),
    );

    let mut user_msg = std::collections::HashMap::new();
    user_msg.insert("role".into(), serde_json::Value::String("user".into()));
    user_msg.insert("content".into(), serde_json::Value::String(prompt));

    match provider
        .chat_with_retry(
            vec![sys_msg, user_msg],
            Some(tools_vec),
            Some(model.to_string()),
            None,
        )
        .await
    {
        response if response.has_tool_calls() => {
            let tc = &response.tool_calls[0];
            if tc.name != "save_memory" {
                warn!("Memory consolidation: unexpected tool call '{}'", tc.name);
                return false;
            }

            if let Some(entry) = tc.arguments.get("history_entry").and_then(|v| v.as_str()) {
                if let Err(e) = store.append_history(entry) {
                    warn!("Failed to append history: {}", e);
                }
            }

            if let Some(update) = tc.arguments.get("memory_update").and_then(|v| v.as_str()) {
                // 落盘前剔除"工具参数纠错"类噪声，切断记忆投毒反馈环。
                // 仅过滤 memory_update（会被每轮注入系统提示）；history_entry 保留原始排障留痕。
                let (sanitized, removed) = sanitize_memory_text(update);
                if removed > 0 {
                    warn!(
                        removed_lines = removed,
                        "Memory consolidation: sanitized {removed} tool-param-noise line(s) from memory_update"
                    );
                }
                if sanitized != current_memory {
                    if let Err(e) = store.write_long_term(&sanitized) {
                        warn!("Failed to write long-term memory: {}", e);
                    }
                }
            }

            info!("Memory consolidation done for {} messages", messages.len());
            true
        }
        _ => {
            warn!("Memory consolidation: LLM did not call save_memory");
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_memory_store_read_write() {
        let tmp = tempfile::TempDir::new().unwrap();
        let store = MemoryStore::new(tmp.path());

        assert!(store.read_long_term().is_empty());
        store.write_long_term("test memory").unwrap();
        assert_eq!(store.read_long_term(), "test memory");
    }

    #[test]
    fn test_memory_context() {
        let tmp = tempfile::TempDir::new().unwrap();
        let store = MemoryStore::new(tmp.path());

        assert!(store.get_memory_context().is_empty());
        store.write_long_term("facts here").unwrap();
        assert!(store.get_memory_context().contains("Long-term Memory"));
    }

    #[test]
    fn test_append_history() {
        let tmp = tempfile::TempDir::new().unwrap();
        let store = MemoryStore::new(tmp.path());

        store.append_history("entry 1").unwrap();
        store.append_history("entry 2").unwrap();

        let content = std::fs::read_to_string(&store.history_file).unwrap();
        assert!(content.contains("entry 1"));
        assert!(content.contains("entry 2"));
    }

    // ── sanitize_memory_text / is_tool_param_noise ──

    #[test]
    fn sanitize_removes_param_correction_keeps_facts() {
        let input = "\
- read_file 的参数是 path，不是 file_path
- 用户偏好中文回复
- payment 报表口径：按自然月汇总
Missing required parameter: path
- skill weekly-report-faq 的使用规则说明";
        let (out, removed) = sanitize_memory_text(input);
        assert_eq!(removed, 2, "应剔除 2 行工具参数噪声");
        assert!(!out.contains("file_path"));
        assert!(!out.to_lowercase().contains("missing required parameter"));
        // 业务事实全部保留
        assert!(out.contains("用户偏好中文回复"));
        assert!(out.contains("payment 报表口径"));
        assert!(out.contains("skill weekly-report-faq"));
    }

    #[test]
    fn sanitize_keeps_business_line_mentioning_only_a_marker() {
        // 只出现"参数"但没有工具/参数 token，不应误删。
        let input = "- 报表参数：统计周期为自然月";
        let (out, removed) = sanitize_memory_text(input);
        assert_eq!(removed, 0);
        assert_eq!(out, input);
    }

    #[test]
    fn sanitize_keeps_tool_token_without_param_marker() {
        // 出现工具名但不涉及参数机制，不应误删（保守）。
        let input = "- 使用 read_file 读取 SKILL.md 后按说明执行";
        let (out, removed) = sanitize_memory_text(input);
        assert_eq!(removed, 0);
        assert_eq!(out, input);
    }

    #[test]
    fn sanitize_empty_input() {
        let (out, removed) = sanitize_memory_text("");
        assert_eq!(out, "");
        assert_eq!(removed, 0);
    }

    #[test]
    fn sanitize_preserves_trailing_newline() {
        let input = "- 用户偏好中文回复\n";
        let (out, removed) = sanitize_memory_text(input);
        assert_eq!(removed, 0);
        assert_eq!(out, input);
    }

    // ── consolidate_with_provider 落盘时对 memory_update 应用过滤 ──

    /// 脚本化 provider：固定返回一次带投毒行的 save_memory 调用。
    struct ScriptedProvider {
        history_entry: String,
        memory_update: String,
    }

    #[async_trait::async_trait]
    impl tyclaw_provider::LLMProvider for ScriptedProvider {
        async fn chat(
            &self,
            _request: tyclaw_provider::ChatRequest,
        ) -> Result<tyclaw_provider::LLMResponse, tyclaw_types::TyclawError> {
            let mut args = std::collections::HashMap::new();
            args.insert(
                "history_entry".to_string(),
                serde_json::Value::String(self.history_entry.clone()),
            );
            args.insert(
                "memory_update".to_string(),
                serde_json::Value::String(self.memory_update.clone()),
            );
            Ok(tyclaw_provider::LLMResponse {
                content: None,
                tool_calls: vec![tyclaw_provider::ToolCallRequest {
                    id: "tc_1".into(),
                    name: "save_memory".into(),
                    arguments: args,
                }],
                finish_reason: "tool_calls".into(),
                usage: std::collections::HashMap::new(),
                reasoning_content: None,
            })
        }

        fn default_model(&self) -> &str {
            "fake-model"
        }
    }

    /// consolidate_with_provider 需要全局 prompt_store 提供
    /// `memory_consolidation_prompt`；OnceLock 幂等，多次调用仅首次生效。
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

    fn one_message() -> Vec<std::collections::HashMap<String, serde_json::Value>> {
        let mut m = std::collections::HashMap::new();
        m.insert("role".into(), serde_json::Value::String("user".into()));
        m.insert(
            "content".into(),
            serde_json::Value::String("请帮我看下周报".into()),
        );
        vec![m]
    }

    #[tokio::test]
    async fn consolidate_filters_memory_update_but_keeps_history() {
        ensure_prompt_store();
        let tmp = tempfile::TempDir::new().unwrap();
        let store = MemoryStore::new(tmp.path());

        let provider = ScriptedProvider {
            // history_entry 含参数纠错措辞——应原样保留（排障留痕）。
            history_entry:
                "[2026-07-28 14:00] 修正了 read_file 的 file_path 参数错误，用户偏好中文回复。"
                    .into(),
            // memory_update 含投毒行 + 业务事实——投毒行应被剔除。
            memory_update: "\
- read_file 的参数应为 path，不是 file_path
- 用户偏好中文回复
- payment 报表口径：按自然月汇总"
                .into(),
        };

        let ok = consolidate_with_provider(&store, &one_message(), &provider, "fake-model").await;
        assert!(ok);

        let memory = store.read_long_term();
        assert!(!memory.contains("file_path"), "投毒行未被剔除: {memory}");
        assert!(memory.contains("用户偏好中文回复"));
        assert!(memory.contains("payment 报表口径"));

        let history = std::fs::read_to_string(&store.history_file).unwrap();
        assert!(
            history.contains("file_path"),
            "history_entry 不应被过滤，应保留原始留痕: {history}"
        );
    }
}
