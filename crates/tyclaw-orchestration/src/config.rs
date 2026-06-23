//! 公共配置结构 —— 供 tyclaw-app 和 tyclaw-client 共用。
//!
//! 配置优先级：命令行参数 > 环境变量 > config.yaml > 默认值

use serde::Deserialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use tyclaw_control::WorkspaceConfig;

use crate::subtasks::SubtasksConfig;

// ── 公共配置结构 ──────────────────────────────────────────

/// 统一配置（config.yaml，app 和 client 共用同一个文件）。
///
/// 各端只解析自己需要的段，不认识的段自动忽略（serde default）。
#[derive(Debug, Default, Deserialize)]
pub struct BaseConfig {
    /// 全局 Provider 定义：各模型独立配置 endpoint / api_key / model。
    /// 主控 LLM 和子任务引擎通过名字引用。
    #[serde(default)]
    pub providers: HashMap<String, crate::subtasks::config::ProviderConfig>,
    #[serde(default)]
    pub llm: LlmConfig,
    #[serde(default)]
    pub workspace: WorkspaceRuntimeConfig,
    #[serde(default)]
    pub workspaces: HashMap<String, WorkspaceConfig>,
    #[serde(default)]
    pub logging: LoggingConfig,
    #[serde(default)]
    pub subtasks: SubtasksConfig,
    #[serde(default)]
    pub web_search: tyclaw_tools::WebSearchConfig,
    #[serde(default)]
    pub control: tyclaw_control::ControlConfig,
    /// 统一性能治理配置（污染过滤 / 会话规模 / 截断 / 并发 / 超时 等）。
    #[serde(default)]
    pub performance: PerformanceConfig,
}

/// Workspace 运行时配置。
#[derive(Debug, Clone, Deserialize)]
pub struct WorkspaceRuntimeConfig {
    /// workspace key 解析策略
    #[serde(default)]
    pub key_strategy: tyclaw_control::WorkspaceKeyStrategy,
    /// 空闲超时（秒）：超过此时间未访问的 workspace 将被回收。0 表示不回收。
    #[serde(default = "default_idle_timeout_secs")]
    pub idle_timeout_secs: u64,
}

impl Default for WorkspaceRuntimeConfig {
    fn default() -> Self {
        Self {
            key_strategy: tyclaw_control::WorkspaceKeyStrategy::default(),
            idle_timeout_secs: default_idle_timeout_secs(),
        }
    }
}

fn default_idle_timeout_secs() -> u64 {
    1800 // 30 分钟
}

/// LLM 配置。
///
/// 推荐用法：在顶层 `providers` 定义模型，这里用 `provider` 引用名字。
/// 也兼容旧格式：直接写 `api_key` / `api_base` / `model`。
#[derive(Debug, Default, Deserialize)]
pub struct LlmConfig {
    /// 引用全局 providers 中的名字（推荐）。
    /// 设置后忽略下方的 api_key / api_base / model / thinking_* 字段。
    pub provider: Option<String>,
    pub api_key: Option<String>,
    pub api_base: Option<String>,
    pub model: Option<String>,
    #[serde(default = "default_max_iterations")]
    pub max_iterations: usize,
    #[serde(default)]
    pub context_window_tokens: Option<usize>,
    #[serde(default)]
    pub snapshot: bool,
    #[serde(default)]
    pub thinking_enabled: bool,
    #[serde(default = "default_thinking_effort")]
    pub thinking_effort: String,
    #[serde(default)]
    pub thinking_budget_tokens: Option<u32>,
    /// LLM 并发调用上限（多个 agent loop 共享，默认 4，0=使用默认值）
    #[serde(default)]
    pub max_concurrent_llm: usize,
}

/// 日志配置。
#[derive(Debug, Clone, Deserialize)]
pub struct LoggingConfig {
    #[serde(default = "default_log_level")]
    pub level: String,
    #[serde(default)]
    pub file: Option<PathBuf>,
}

impl Default for LoggingConfig {
    fn default() -> Self {
        Self {
            level: default_log_level(),
            file: None,
        }
    }
}

// ── 默认值函数 ──────────────────────────────────────────

pub fn default_max_iterations() -> usize {
    40
}
pub fn default_thinking_effort() -> String {
    "high".into()
}
pub fn default_log_level() -> String {
    "info".into()
}

// ── 辅助函数 ──────────────────────────────────────────

/// 加载 YAML 配置文件，文件不存在或解析失败时返回默认值。
pub fn load_yaml<T: Default + serde::de::DeserializeOwned>(config_path: &Path) -> T {
    if !config_path.exists() {
        return T::default();
    }
    match std::fs::read_to_string(config_path) {
        Ok(text) => match serde_yaml::from_str(&text) {
            Ok(cfg) => cfg,
            Err(e) => {
                eprintln!("Warning: Failed to parse {}: {e}", config_path.display());
                T::default()
            }
        },
        Err(e) => {
            eprintln!("Warning: Failed to read {}: {e}", config_path.display());
            T::default()
        }
    }
}

/// 对 API 密钥等敏感字段做掩码（保留首尾 4 字符）。
pub fn mask_secret(secret: &str) -> String {
    if secret.is_empty() {
        return "<empty>".into();
    }
    if secret.len() <= 8 {
        return "***".into();
    }
    let prefix = &secret[..4];
    let suffix = &secret[secret.len() - 4..];
    format!("{prefix}***{suffix}")
}

// ── 统一性能治理配置（PerformanceConfig）────────────────────────
//
// 所有性能相关阈值收敛到此处，便于线上调参。各子结构均实现 `Default`，
// 缺省值取自 requirements.md；加载后通过 `clamp` 对有下限/上限约束的字段做钳制。

/// 截断上限下限：exec / grep_search 输出截断上限不得低于此值。
pub const TRUNCATE_FLOOR_CHARS: usize = 8000;
/// 单个 Node 最大执行时间上限（秒）：node 超时不得超过 5 分钟。
pub const NODE_TIMEOUT_CEIL_SECS: u64 = 300;

/// 污染识别配置（R1 / R2）。
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct PollutionConfig {
    /// 污染关键词集合，默认 ["I cannot make progress", "error", "blocked"]。
    pub keywords: Vec<String>,
    /// 污染候选短消息字符上限，默认 512。
    pub short_message_max_chars: usize,
}

impl Default for PollutionConfig {
    fn default() -> Self {
        Self {
            keywords: vec![
                "I cannot make progress".to_string(),
                "error".to_string(),
                "blocked".to_string(),
            ],
            short_message_max_chars: 512,
        }
    }
}

/// 会话规模上限配置（R3）。
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct SizeLimitConfig {
    /// 会话消息数硬上限，默认 500。
    pub max_messages: usize,
    /// 滚动截断目标水位，默认 400（= 80%）。
    pub rolling_target: usize,
    /// Pairs_Fixes 告警阈值，默认 30。
    pub pairs_fixes_warn: usize,
    /// Pairs_Fixes 强制重置阈值，默认 60。
    pub pairs_fixes_force_reset: usize,
}

impl Default for SizeLimitConfig {
    fn default() -> Self {
        Self {
            max_messages: 500,
            rolling_target: 400,
            pairs_fixes_warn: 30,
            pairs_fixes_force_reset: 60,
        }
    }
}

/// 记忆合并体量上限配置（R4）。
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct ConsolidationConfig {
    /// 单次记忆合并最大消息数上限，默认 500。
    pub max_messages_per_batch: usize,
    /// 单次合并调用最多处理的分片批次数，默认 5。
    pub max_rounds: usize,
}

impl Default for ConsolidationConfig {
    fn default() -> Self {
        Self {
            max_messages_per_batch: 500,
            max_rounds: 5,
        }
    }
}

/// 工具输出截断配置（R5）。
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct TruncationConfig {
    /// exec 工具输出截断上限，默认 20000，下限 8000。
    pub exec_truncate_chars: usize,
    /// grep_search 工具输出截断上限，默认 20000，下限 8000。
    pub grep_truncate_chars: usize,
    /// 尾部保留段最低占比，默认 0.25。
    pub tail_ratio: f64,
}

impl Default for TruncationConfig {
    fn default() -> Self {
        Self {
            exec_truncate_chars: 20000,
            grep_truncate_chars: 20000,
            tail_ratio: 0.25,
        }
    }
}

/// 并发控制配置（R6）。
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct ConcurrencyConfig {
    /// 全局并发 LLM 调用上限（in-flight 上限），默认 4，下限 1。
    pub global_max_inflight: usize,
    /// 单用户并发请求上限，默认 3，下限 1。
    pub per_user_max_inflight: usize,
    /// 排队超时（秒），默认 300（5 分钟）。
    pub queue_timeout_secs: u64,
}

impl Default for ConcurrencyConfig {
    fn default() -> Self {
        Self {
            global_max_inflight: 4,
            per_user_max_inflight: 3,
            queue_timeout_secs: 300,
        }
    }
}

/// 子任务链超时配置（R7）。
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct SubtaskTimeoutConfig {
    /// 单个 Node 最大执行时间（秒），默认 300，上限 300（5 分钟）。
    pub node_max_duration_secs: u64,
    /// dispatch_subtasks 整体最大执行时间（秒），默认 600。
    pub dispatch_max_duration_secs: u64,
}

impl Default for SubtaskTimeoutConfig {
    fn default() -> Self {
        Self {
            node_max_duration_secs: 300,
            dispatch_max_duration_secs: 600,
        }
    }
}

/// 空结果任务快速返回配置（R9）。
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct EmptyResultConfig {
    /// 快速返回时限（秒），默认 30。
    pub fast_return_secs: u64,
}

impl Default for EmptyResultConfig {
    fn default() -> Self {
        Self {
            fast_return_secs: 30,
        }
    }
}

/// SSE 动态超时配置（R10）。
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct SseConfig {
    /// chunk 超时基线（秒），默认 90。
    pub chunk_timeout_secs: u64,
    /// 高并发 chunk 超时阈值（秒），默认 150。
    pub high_concurrency_timeout_secs: u64,
    /// 触发动态超时提升的 in-flight 阈值，默认 8。
    pub high_concurrency_inflight: usize,
    /// 最大重试次数，默认 3。
    pub max_retries: usize,
}

impl Default for SseConfig {
    fn default() -> Self {
        Self {
            chunk_timeout_secs: 90,
            high_concurrency_timeout_secs: 150,
            high_concurrency_inflight: 8,
            max_retries: 3,
        }
    }
}

/// 统一性能治理配置（汇总各子结构）。
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct PerformanceConfig {
    pub pollution: PollutionConfig,
    pub session_limits: SizeLimitConfig,
    pub consolidation: ConsolidationConfig,
    pub truncation: TruncationConfig,
    pub concurrency: ConcurrencyConfig,
    pub subtask_timeout: SubtaskTimeoutConfig,
    pub empty_result: EmptyResultConfig,
    pub sse: SseConfig,
    /// 慢请求耗时阈值（秒），用于慢请求原因码分布统计（R14.4），默认 120。
    pub slow_request_threshold_secs: u64,
}

impl Default for PerformanceConfig {
    fn default() -> Self {
        Self {
            pollution: PollutionConfig::default(),
            session_limits: SizeLimitConfig::default(),
            consolidation: ConsolidationConfig::default(),
            truncation: TruncationConfig::default(),
            concurrency: ConcurrencyConfig::default(),
            subtask_timeout: SubtaskTimeoutConfig::default(),
            empty_result: EmptyResultConfig::default(),
            sse: SseConfig::default(),
            slow_request_threshold_secs: 120,
        }
    }
}

impl PerformanceConfig {
    /// 加载后对有下限/上限约束的字段做钳制：
    /// - 截断上限下限 ≥ 8000；
    /// - 全局/单用户并发 ≥ 1；
    /// - 单个 Node 超时 ≤ 5 分钟。
    pub fn clamp(&mut self) {
        self.truncation.exec_truncate_chars =
            self.truncation.exec_truncate_chars.max(TRUNCATE_FLOOR_CHARS);
        self.truncation.grep_truncate_chars =
            self.truncation.grep_truncate_chars.max(TRUNCATE_FLOOR_CHARS);
        self.concurrency.global_max_inflight = self.concurrency.global_max_inflight.max(1);
        self.concurrency.per_user_max_inflight = self.concurrency.per_user_max_inflight.max(1);
        self.subtask_timeout.node_max_duration_secs = self
            .subtask_timeout
            .node_max_duration_secs
            .min(NODE_TIMEOUT_CEIL_SECS);
    }
}

#[cfg(test)]
mod perf_config_property_tests {
    use super::*;
    use proptest::prelude::*;

    proptest! {
        #![proptest_config(ProptestConfig { cases: 100, ..ProptestConfig::default() })]

        // Feature: execution-performance-optimization, Property 18: 截断上限配置被 clamp 到下限以上
        #[test]
        fn prop18_truncation_clamped_above_floor(
            exec_chars in any::<usize>(),
            grep_chars in any::<usize>(),
        ) {
            let mut cfg = PerformanceConfig::default();
            cfg.truncation.exec_truncate_chars = exec_chars;
            cfg.truncation.grep_truncate_chars = grep_chars;

            cfg.clamp();

            prop_assert!(
                cfg.truncation.exec_truncate_chars >= TRUNCATE_FLOOR_CHARS,
                "exec_truncate_chars {} < floor {}",
                cfg.truncation.exec_truncate_chars,
                TRUNCATE_FLOOR_CHARS
            );
            prop_assert!(
                cfg.truncation.grep_truncate_chars >= TRUNCATE_FLOOR_CHARS,
                "grep_truncate_chars {} < floor {}",
                cfg.truncation.grep_truncate_chars,
                TRUNCATE_FLOOR_CHARS
            );
        }
    }
}

#[cfg(test)]
mod perf_config_default_tests {
    use super::*;

    // 验证各子结构 Default 值与需求一致。

    #[test]
    fn pollution_defaults_match_requirements() {
        // R1.3：默认污染关键词集合 + 短消息字符上限。
        let cfg = PollutionConfig::default();
        assert_eq!(
            cfg.keywords,
            vec![
                "I cannot make progress".to_string(),
                "error".to_string(),
                "blocked".to_string(),
            ]
        );
        assert_eq!(cfg.short_message_max_chars, 512);
    }

    #[test]
    fn size_limit_defaults_match_requirements() {
        // R3.1：会话规模上限默认值。
        let cfg = SizeLimitConfig::default();
        assert_eq!(cfg.max_messages, 500);
        assert_eq!(cfg.rolling_target, 400);
        assert_eq!(cfg.pairs_fixes_warn, 30);
        assert_eq!(cfg.pairs_fixes_force_reset, 60);
    }

    #[test]
    fn consolidation_defaults_match_requirements() {
        // R4.1：记忆合并体量上限默认值。
        let cfg = ConsolidationConfig::default();
        assert_eq!(cfg.max_messages_per_batch, 500);
        assert_eq!(cfg.max_rounds, 5);
    }

    #[test]
    fn truncation_defaults_match_requirements() {
        // R5.5：工具输出截断默认值。
        let cfg = TruncationConfig::default();
        assert_eq!(cfg.exec_truncate_chars, 20000);
        assert_eq!(cfg.grep_truncate_chars, 20000);
        assert_eq!(cfg.tail_ratio, 0.25);
    }

    #[test]
    fn concurrency_defaults_match_requirements() {
        // R6.1：并发控制默认值。
        let cfg = ConcurrencyConfig::default();
        assert_eq!(cfg.global_max_inflight, 4);
        assert_eq!(cfg.per_user_max_inflight, 3);
        assert_eq!(cfg.queue_timeout_secs, 300);
    }

    #[test]
    fn subtask_timeout_defaults_match_requirements() {
        // R7.1 / R7.3：子任务链超时默认值。
        let cfg = SubtaskTimeoutConfig::default();
        assert_eq!(cfg.node_max_duration_secs, 300);
        assert_eq!(cfg.dispatch_max_duration_secs, 600);
    }

    #[test]
    fn empty_result_defaults_match_requirements() {
        // R9：空结果任务快速返回时限。
        let cfg = EmptyResultConfig::default();
        assert_eq!(cfg.fast_return_secs, 30);
    }

    #[test]
    fn sse_defaults_match_requirements() {
        // R10.1：SSE 动态超时默认值。
        let cfg = SseConfig::default();
        assert_eq!(cfg.chunk_timeout_secs, 90);
        assert_eq!(cfg.high_concurrency_timeout_secs, 150);
        assert_eq!(cfg.high_concurrency_inflight, 8);
        assert_eq!(cfg.max_retries, 3);
    }

    #[test]
    fn performance_config_defaults_match_requirements() {
        // R6.4：慢请求耗时阈值，并校验聚合结构各段默认值串联正确。
        let cfg = PerformanceConfig::default();
        assert_eq!(cfg.slow_request_threshold_secs, 120);
        assert_eq!(cfg.pollution.short_message_max_chars, 512);
        assert_eq!(cfg.session_limits.max_messages, 500);
        assert_eq!(cfg.consolidation.max_messages_per_batch, 500);
        assert_eq!(cfg.truncation.exec_truncate_chars, 20000);
        assert_eq!(cfg.concurrency.global_max_inflight, 4);
        assert_eq!(cfg.subtask_timeout.node_max_duration_secs, 300);
        assert_eq!(cfg.empty_result.fast_return_secs, 30);
        assert_eq!(cfg.sse.chunk_timeout_secs, 90);
    }

    // clamp 钳制行为。

    #[test]
    fn clamp_raises_concurrency_to_at_least_one() {
        // R6.1：全局/单用户并发下限为 1。
        let mut cfg = PerformanceConfig::default();
        cfg.concurrency.global_max_inflight = 0;
        cfg.concurrency.per_user_max_inflight = 0;

        cfg.clamp();

        assert!(cfg.concurrency.global_max_inflight >= 1);
        assert!(cfg.concurrency.per_user_max_inflight >= 1);
    }

    #[test]
    fn clamp_caps_node_timeout_at_five_minutes() {
        // R7.1：单个 Node 超时上限为 5 分钟（300 秒）。
        let mut cfg = PerformanceConfig::default();
        cfg.subtask_timeout.node_max_duration_secs = 99999;

        cfg.clamp();

        assert!(cfg.subtask_timeout.node_max_duration_secs <= NODE_TIMEOUT_CEIL_SECS);
        assert_eq!(cfg.subtask_timeout.node_max_duration_secs, 300);
    }
}
