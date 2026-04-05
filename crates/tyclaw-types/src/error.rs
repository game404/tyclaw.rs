use thiserror::Error;

/// TyClaw 统一错误类型。
///
/// 使用 `thiserror` 宏自动实现 `std::error::Error` trait，
/// 将系统中各种错误统一到一个枚举中，方便上层统一处理。
#[derive(Debug, Error)]
pub enum TyclawError {
    /// IO 错误 —— 文件读写、网络连接等底层 IO 操作失败时触发。
    /// 通过 `#[from]` 自动从 `std::io::Error` 转换。
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    /// JSON 序列化/反序列化错误 —— 解析 LLM 响应或配置文件时触发。
    /// 通过 `#[from]` 自动从 `serde_json::Error` 转换。
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    /// LLM 提供者错误 —— API 调用失败、HTTP 错误、响应格式异常等。
    #[error("Provider error: {0}")]
    Provider(String),

    /// 工具执行错误 —— 某个具体工具执行失败时触发。
    /// 包含工具名称和错误信息，方便定位问题。
    #[error("Tool error [{tool}]: {message}")]
    Tool { tool: String, message: String },

    /// 执行门禁拒绝 —— 权限不足导致工具调用被拦截。
    /// 例如：Guest 角色尝试执行写入操作。
    #[error("Gate denied: {0}")]
    GateDenied(String),

    /// 达到最大迭代次数 —— ReAct 循环超过预设上限。
    /// 携带实际达到的迭代次数。
    #[error("Max iterations reached: {0}")]
    MaxIterations(usize),

    /// 速率限制 —— 请求频率超过限制时触发。
    #[error("Rate limit: {0}")]
    RateLimitExceeded(String),

    /// 其他未分类错误 —— 兜底错误类型。
    #[error("{0}")]
    Other(String),
}
