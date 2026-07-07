//! 交互工具 —— 允许 Agent 在执行过程中向用户提问。
//!
//! `ask_user` 工具让 Agent 可以在循环中暂停，向用户提问并等待回复，
//! 而不是盲目猜测用户意图。这实现了多轮交互式任务处理。

use async_trait::async_trait;
use parking_lot::Mutex;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;

use crate::base::{brief_truncate, RiskLevel, Tool};
use crate::filesystem::CURRENT_REQUEST_ID;

/// 向用户提问工具。
///
/// 此工具不会真正执行——Agent Loop 检测到此工具调用后会暂停循环，
/// 将问题返回给用户，等待用户回复后再恢复执行。
pub struct AskUserTool;

impl AskUserTool {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Tool for AskUserTool {
    fn name(&self) -> &str {
        "ask_user"
    }

    fn description(&self) -> &str {
        "Pause execution and ask the user a question. Use this when you need clarification, \
         confirmation, or additional information before proceeding. Do not use this for information \
         you can obtain from available tools, and do not ask for confirmation on routine safe steps. \
         The agent loop will pause and wait for the user's response before continuing."
    }

    fn brief(&self, args: &HashMap<String, Value>) -> Option<String> {
        let question = args.get("question").and_then(|v| v.as_str())?;
        Some(format!("ask: {}", brief_truncate(question, 60)))
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "question": {
                    "type": "string",
                    "description": "The question or message to present to the user. Be specific about what information you need."
                }
            },
            "required": ["question"]
        })
    }

    fn risk_level(&self) -> RiskLevel {
        RiskLevel::Read
    }

    async fn execute(&self, params: HashMap<String, Value>) -> String {
        // 此方法不会被实际调用——Agent Loop 会拦截 ask_user 工具调用。
        // 如果意外被调用，返回提示信息。
        let question = params
            .get("question")
            .and_then(|v| v.as_str())
            .unwrap_or("(no question provided)");
        format!("[ask_user] Question sent to user: {question}")
    }
}

/// 按请求 ID 隔离的「推荐问题」存储。
///
/// 与 [`crate::filesystem::PendingFileStore`] 同构：`suggest_recommends` 工具在
/// agent loop 执行期间通过 [`CURRENT_REQUEST_ID`] 定位当前请求并写入推荐问题，
/// 执行结束后由编排层 `drain` 取走，交给上层（如 DingTalk bot）渲染成卡片推荐组件。
///
/// 复用 `PendingFileStore::new_request` 分配的同一 `request_id`，故本存储无需
/// 单独分配 ID：`set` 用 `insert` 覆盖写入、`drain` 用 `remove` 取走。
pub struct PendingRecommendStore {
    inner: Mutex<HashMap<u64, Vec<String>>>,
}

impl PendingRecommendStore {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
        }
    }

    /// 设置指定请求的推荐问题（覆盖式，后一次调用生效）。
    pub fn set(&self, request_id: u64, questions: Vec<String>) {
        let mut map = self.inner.lock();
        map.insert(request_id, questions);
    }

    /// 取走并返回指定请求的推荐问题。
    pub fn drain(&self, request_id: u64) -> Vec<String> {
        let mut map = self.inner.lock();
        map.remove(&request_id).unwrap_or_default()
    }
}

impl Default for PendingRecommendStore {
    fn default() -> Self {
        Self::new()
    }
}

/// 推荐问题工具 —— Agent 在回答结束时给出「猜你想问」推荐问题。
///
/// 仅在与当前话题强相关、能帮助用户深入时调用；不调用则不展示推荐（默认无推荐）。
/// 推荐问题经编排层回传到 `AgentResponse.recommends`，由上层渲染为钉钉 AI 卡片的
/// 推荐组件，用户点击即以本人身份把问题发回会话触发新一轮问答。
pub struct SuggestRecommendsTool {
    store: Arc<PendingRecommendStore>,
}

impl SuggestRecommendsTool {
    pub fn new(store: Arc<PendingRecommendStore>) -> Self {
        Self { store }
    }
}

#[async_trait]
impl Tool for SuggestRecommendsTool {
    fn name(&self) -> &str {
        "suggest_recommends"
    }

    fn description(&self) -> &str {
        "在回答结束时，给出 2-3 个用户可能想继续追问的推荐问题（“猜你想问”）。\
         仅在与当前话题强相关、能帮助用户深入了解时调用；没有合适的推荐时不要调用此工具。\
         每个问题应是完整、可直接发送的问句。"
    }

    fn brief(&self, args: &HashMap<String, Value>) -> Option<String> {
        let n = args
            .get("questions")
            .and_then(|v| v.as_array())
            .map(|a| a.len())
            .unwrap_or(0);
        Some(format!("suggest {n} question(s)"))
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "questions": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "推荐问题列表，建议 2-3 个，每个是完整、可直接发送的问句"
                }
            },
            "required": ["questions"]
        })
    }

    fn risk_level(&self) -> RiskLevel {
        RiskLevel::Read
    }

    async fn execute(&self, params: HashMap<String, Value>) -> String {
        let questions: Vec<String> = params
            .get("questions")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str())
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect()
            })
            .unwrap_or_default();

        if questions.is_empty() {
            return "No valid recommend questions provided.".to_string();
        }

        let count = questions.len();
        let request_id = CURRENT_REQUEST_ID.try_with(|id| *id).unwrap_or(0);
        self.store.set(request_id, questions);
        format!("Recorded {count} recommended question(s).")
    }
}
