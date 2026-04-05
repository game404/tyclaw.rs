use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;

/// 从 LLM 响应中解析出的工具调用请求。
///
/// 当 LLM 决定调用工具时，会返回一个或多个 ToolCallRequest，
/// 每个请求包含：
/// - `id`: 唯一标识符，用于将执行结果关联回对应的调用
/// - `name`: 工具名称（如 "read_file"、"exec"）
/// - `arguments`: 工具参数键值对
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallRequest {
    pub id: String,
    pub name: String,
    pub arguments: HashMap<String, Value>,
}

/// LLM 提供者的响应结构。
///
/// 封装了 LLM API 返回的所有关键信息：
/// - `content`: 文本回复内容（工具调用时可能为 None）
/// - `tool_calls`: 解析后的工具调用请求列表
/// - `finish_reason`: 结束原因（"stop"=正常结束, "tool_calls"=需要调用工具, "error"=错误）
/// - `usage`: token 使用统计（prompt_tokens、completion_tokens、total_tokens）
/// - `reasoning_content`: 推理内容（部分模型支持，如 DeepSeek 的思考过程）
#[derive(Debug, Clone)]
pub struct LLMResponse {
    pub content: Option<String>,
    pub tool_calls: Vec<ToolCallRequest>,
    pub finish_reason: String,
    pub usage: HashMap<String, u64>,
    pub reasoning_content: Option<String>,
}

impl LLMResponse {
    /// 检查响应中是否包含工具调用请求。
    pub fn has_tool_calls(&self) -> bool {
        !self.tool_calls.is_empty()
    }

    /// 将原始 reasoning_content 解析为结构化的 `ParsedReasoning`。
    ///
    /// 返回 None 如果没有 reasoning_content。
    /// 解析后包含：结构化块（Thinking/Code/ToolCall）、前端展示文本、
    /// 是否有被错误放置的 tool_call 等信息。
    pub fn parsed_reasoning(&self) -> Option<crate::reasoning::ParsedReasoning> {
        self.reasoning_content
            .as_deref()
            .map(crate::reasoning::parse_reasoning)
    }

    /// 创建一个错误响应。
    ///
    /// 将错误信息封装为标准 LLMResponse 格式，
    /// finish_reason 设为 "error"，方便上层统一处理。
    pub fn error(msg: impl Into<String>) -> Self {
        Self {
            content: Some(msg.into()),
            tool_calls: Vec::new(),
            finish_reason: "error".into(),
            usage: HashMap::new(),
            reasoning_content: None,
        }
    }
}

/// Extended thinking 配置（Claude Opus 4.6）。
///
/// 两种模式（二选一）：
/// - **adaptive**（默认）：设置 `effort`（none/minimal/low/medium/high/xhigh），
///   模型自行决定是否 think 以及深度。
/// - **forced**：设置 `budget_tokens`（1024~128000），强制每轮都 think，
///   固定分配指定数量的 reasoning token。
#[derive(Debug, Clone)]
pub struct ThinkingConfig {
    pub effort: String,
    /// 设置后使用 forced 模式（reasoning.max_tokens），忽略 effort
    pub budget_tokens: Option<u32>,
}

/// 默认生成参数。
///
/// - `temperature`: 控制输出随机性，0.7 是创造性和确定性之间的平衡点
/// - `max_tokens`: 单次生成的最大 token 数，16384 足以应对大多数场景
/// - `thinking`: 可选的 extended thinking 配置
#[derive(Debug, Clone)]
pub struct GenerationSettings {
    pub temperature: f64,
    pub max_tokens: u32,
    pub thinking: Option<ThinkingConfig>,
}

impl Default for GenerationSettings {
    fn default() -> Self {
        Self {
            temperature: 0.3,
            max_tokens: 16384,
            thinking: None,
        }
    }
}

/// 发送给 LLM 提供者的聊天请求。
///
/// - `messages`: 完整的对话历史（包含 system、user、assistant、tool 等消息）
/// - `tools`: 可选的工具定义列表（OpenAI function calling 格式）
/// - `model`: 可选的模型覆盖（不指定则使用 provider 的默认模型）
/// - `max_tokens`: 最大生成 token 数
/// - `temperature`: 采样温度
#[derive(Debug, Clone)]
pub struct ChatRequest {
    pub messages: Vec<HashMap<String, Value>>,
    pub tools: Option<Vec<Value>>,
    pub model: Option<String>,
    pub cache_scope: Option<String>,
    pub max_tokens: u32,
    pub temperature: f64,
}
