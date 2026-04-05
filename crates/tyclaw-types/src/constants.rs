/// 默认 LLM 模型标识符。
/// 使用 "openai/" 前缀标识模型来源，实际调用时会自动去掉前缀。
pub const DEFAULT_MODEL: &str = "openai/claude-sonnet-4-20250514";

/// 默认上下文窗口大小（以 token 为单位）。
/// 200K tokens 适用于大多数现代 LLM 模型。
pub const DEFAULT_CONTEXT_WINDOW: usize = 200_000;

/// ReAct 循环的最大迭代次数。
/// 超过此次数后 Agent 会强制停止，防止无限循环。
pub const DEFAULT_MAX_ITERATIONS: usize = 40;

/// 单用户速率限制：每个时间窗口内允许的最大请求数。
pub const DEFAULT_RATE_LIMIT_PER_USER: usize = 5;

/// 全局速率限制：所有用户合计的每个时间窗口最大请求数。
pub const DEFAULT_RATE_LIMIT_GLOBAL: usize = 20;

/// 速率限制的时间窗口大小（单位：秒）。
pub const DEFAULT_RATE_LIMIT_WINDOW_SECS: u64 = 60;

/// 最大并发任务数。
pub const DEFAULT_MAX_TASKS: usize = 8;

/// 机器人的显示名称。
pub const BOT_NAME: &str = "TyClaw";
