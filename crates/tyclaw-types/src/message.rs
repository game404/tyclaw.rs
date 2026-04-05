use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;

/// 聊天消息类型 —— 采用 OpenAI 兼容格式。
///
/// 在 API 边界使用松散类型的 HashMap，以实现最大兼容性。
/// 键通常包括 "role"、"content"、"tool_calls"、"tool_call_id"、"name" 等。
/// 同时提供辅助构造函数来保证类型安全的创建方式。
pub type Message = HashMap<String, Value>;

/// 创建系统消息（system message）。
///
/// 系统消息用于设置 AI 助手的行为准则和上下文信息。
/// 通常作为对话的第一条消息。
pub fn system_message(content: &str) -> Message {
    let mut m = Message::new();
    m.insert("role".into(), Value::String("system".into()));
    m.insert("content".into(), Value::String(content.into()));
    m
}

/// 创建用户消息（user message）。
///
/// 用户消息代表终端用户的输入/请求。
pub fn user_message(content: &str) -> Message {
    let mut m = Message::new();
    m.insert("role".into(), Value::String("user".into()));
    m.insert("content".into(), Value::String(content.into()));
    m
}

/// 创建多模态用户消息（包含文本和图片）。
///
/// 生成 OpenAI vision 格式的 content array：
/// ```json
/// [
///   {"type": "text", "text": "描述这张图片"},
///   {"type": "image_url", "image_url": {"url": "data:image/png;base64,..."}}
/// ]
/// ```
///
/// - `text`: 文本内容
/// - `image_data_uris`: 图片 data URI 列表（`data:image/...;base64,...`）
pub fn user_message_multimodal(text: &str, image_data_uris: &[String]) -> Message {
    let mut parts = Vec::new();

    // 文本部分
    if !text.is_empty() {
        parts.push(serde_json::json!({
            "type": "text",
            "text": text,
        }));
    }

    // 图片部分 —— OpenAI vision 格式，由代理层转换为目标 LLM 格式
    for uri in image_data_uris {
        parts.push(serde_json::json!({
            "type": "image_url",
            "image_url": { "url": uri },
        }));
    }

    let mut m = Message::new();
    m.insert("role".into(), Value::String("user".into()));
    m.insert("content".into(), Value::Array(parts));
    m
}

/// 创建助手消息（assistant message）。
///
/// 助手消息代表 AI 的回复，可以包含文本内容和/或工具调用请求。
/// - `content`: 文本回复内容，可以为 None（例如纯工具调用时）
/// - `tool_calls`: 可选的工具调用列表，格式为 JSON 数组
pub fn assistant_message(content: Option<&str>, tool_calls: Option<Vec<Value>>) -> Message {
    let mut m = Message::new();
    m.insert("role".into(), Value::String("assistant".into()));
    match content {
        Some(c) => m.insert("content".into(), Value::String(c.into())),
        None => m.insert("content".into(), Value::Null), // 无文本内容时设为 null
    };
    if let Some(tc) = tool_calls {
        m.insert("tool_calls".into(), Value::Array(tc)); // 附加工具调用信息
    }
    m
}

/// 创建工具执行结果消息（tool result message）。
///
/// 当工具被调用并返回结果后，将结果封装为此消息类型反馈给 LLM。
/// - `tool_call_id`: 对应工具调用的唯一标识符，用于关联请求和响应
/// - `name`: 工具名称（如 "read_file"）
/// - `content`: 工具执行的输出结果
pub fn tool_result_message(tool_call_id: &str, name: &str, content: &str) -> Message {
    let mut m = Message::new();
    m.insert("role".into(), Value::String("tool".into()));
    m.insert("tool_call_id".into(), Value::String(tool_call_id.into()));
    m.insert("name".into(), Value::String(name.into()));
    m.insert("content".into(), Value::String(content.into()));
    m
}

/// 从消息中提取 "role" 字段。
///
/// 返回消息的角色类型字符串（如 "system"、"user"、"assistant"、"tool"）。
/// 如果字段不存在或类型不匹配，返回空字符串。
pub fn msg_role(msg: &Message) -> &str {
    msg.get("role").and_then(|v| v.as_str()).unwrap_or("")
}

/// 从消息中提取 "content" 字段。
///
/// 返回消息的文本内容。如果 content 为 null 或不存在，返回 None。
pub fn msg_content(msg: &Message) -> Option<&str> {
    msg.get("content").and_then(|v| v.as_str())
}

/// 用户角色枚举 —— 定义了系统中的四种权限等级。
///
/// 角色按权限从低到高排列：
/// - `Guest`: 访客，只能执行只读操作
/// - `Member`: 成员，可以执行读写操作
/// - `Developer`: 开发者，可以读写并查看审计日志
/// - `Admin`: 管理员，拥有所有权限，包括危险操作
///
/// 实现了 PartialOrd/Ord，所以可以直接比较权限高低。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")] // 序列化时使用小写形式
pub enum Role {
    Guest,     // 访客
    Member,    // 普通成员
    Developer, // 开发者
    Admin,     // 管理员
}

/// Role 的默认值为 Member（普通成员）
impl Default for Role {
    fn default() -> Self {
        Self::Member
    }
}

/// 风险等级枚举 —— 用于工具调用的权限控制。
///
/// - `Read`: 只读操作，如读取文件、列出目录，所有角色都可执行
/// - `Write`: 写入操作，如写文件、执行命令，需要 Member 及以上角色
/// - `Dangerous`: 危险操作，如删除文件、格式化磁盘，需要 Admin 确认
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RiskLevel {
    Read,      // 只读
    Write,     // 写入
    Dangerous, // 危险
}

/// RiskLevel 的默认值为 Read（只读）
impl Default for RiskLevel {
    fn default() -> Self {
        Self::Read
    }
}
