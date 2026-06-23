use async_trait::async_trait;
use tracing::{info, warn};

use tyclaw_types::TyclawError;

use crate::concurrency::{self, ConcurrencyError};
use crate::types::{ChatRequest, GenerationSettings, LLMResponse};

/// 重试延迟时间序列（单位：秒）。
/// 采用指数退避策略：1s → 2s → 4s，共 3 次重试后执行最终尝试。
const RETRY_DELAYS: &[u64] = &[1, 2, 4];

/// 重试耗尽时返回给用户的可识别「请稍后重试」提示（R10.5 / Property 29）。
///
/// 当 SSE/HTTP 临时性错误重试达上限仍未成功时，向用户返回此提示，
/// 而非透传失败兜底文案（如 `I cannot make progress`）。该文案须满足：
/// - 可识别为「请稍后重试」语义；
/// - 不含任何失败兜底/污染关键词（如 `I cannot make progress`）。
///
/// 设为 `pub` 并经 `lib.rs` 重新导出，供属性测试（任务 14.4）直接引用断言。
pub const RETRY_LATER_MESSAGE: &str = "服务暂时繁忙，请稍后重试。";

/// 构建重试耗尽的「请稍后重试」提示文本（R10.5 / Property 29）。
///
/// 纯函数，无副作用，便于属性测试断言：返回值即 [`RETRY_LATER_MESSAGE`]，
/// 可识别为「请稍后重试」且不含失败兜底文案。
pub fn build_retry_later_message() -> String {
    RETRY_LATER_MESSAGE.to_string()
}

/// 构建重试耗尽时返回给用户的 [`LLMResponse`]（R10.5 / Property 29）。
///
/// 内容为可识别的「请稍后重试」提示而非失败兜底文案。`finish_reason` 设为
/// `"error"`，与并发排队超时（[`LLMResponse::error`]）保持一致的失败语义，
/// 但暴露给用户的文本为友好的稍后重试提示。
pub fn retry_exhausted_response() -> LLMResponse {
    LLMResponse::error(RETRY_LATER_MESSAGE)
}

/// 未显式提供 user_id 时使用的兜底标识。
///
/// 部分调用方（如记忆抽取、子任务规划）无明确归属用户，统一归入此 bucket，
/// 仍受全局闸门约束，per-user 维度共享同一信号量。
const DEFAULT_USER_ID: &str = "_anonymous";

tokio::task_local! {
    /// 当前请求归属的用户标识。
    ///
    /// 上层在进入 agent loop / 处理用户消息前用 `CURRENT_USER_ID.scope(uid, fut)`
    /// 设置；`chat_with_retry` 据此向并发控制器申请 per-user 许可（R6.2/R6.5）。
    /// 未设置时回退到 [`DEFAULT_USER_ID`]。
    pub static CURRENT_USER_ID: String;
}

/// 读取当前 task-local 的 user_id；未设置时返回 [`DEFAULT_USER_ID`]。
fn current_user_id() -> String {
    CURRENT_USER_ID
        .try_with(|uid| uid.clone())
        .unwrap_or_else(|_| DEFAULT_USER_ID.to_string())
}

/// 初始化 LLM 并发限制。应在启动时调用一次。
///
/// 兼容旧入口：将全局上限透传给并发控制器（R6.1），由控制器统一承载
/// 全局 + 单用户两级闸门与排队超时。
pub fn init_concurrency(max_concurrent: usize) {
    let mut config = concurrency::ConcurrencyConfig::default();
    if max_concurrent != 0 {
        config.global_max_inflight = max_concurrent;
    }
    let limit = config.global_max_inflight;
    concurrency::init_concurrency_controller(config);
    info!(max_concurrent = limit, "LLM concurrency limit initialized");
}

/// 临时性错误的特征字符串列表。
/// 当 LLM 返回的错误信息中包含这些关键词时，认为是可重试的临时错误。
/// 包括：速率限制（429）、服务器内部错误（500-504）、超时、连接问题等。
const TRANSIENT_MARKERS: &[&str] = &[
    "429",
    "rate limit",
    "500",
    "502",
    "503",
    "504",
    "overloaded",
    "timeout",
    "timed out",
    "connection",
    "server error",
    "temporarily unavailable",
];

/// 判断错误是否为临时性/可重试错误。
///
/// 将错误消息转为小写后，检查是否包含任何临时错误特征字符串。
fn is_transient_error(content: Option<&str>) -> bool {
    let err = content.unwrap_or("").to_lowercase();
    TRANSIENT_MARKERS.iter().any(|m| err.contains(m))
}

/// LLM 提供者 trait —— 所有 LLM 实现必须满足的接口。
///
/// 通过 `async_trait` 支持异步方法。
/// `Send + Sync` 约束确保可以在多线程环境中安全使用。
#[async_trait]
pub trait LLMProvider: Send + Sync {
    /// 发送聊天请求并返回 LLM 响应。
    /// 这是核心方法，各个具体实现（如 OpenAI、Anthropic）需要实现此方法。
    async fn chat(&self, request: ChatRequest) -> Result<LLMResponse, TyclawError>;

    /// 返回默认模型标识符（如 "gpt-4o"）。
    fn default_model(&self) -> &str;

    /// 返回 API 基础 URL（用于 multi-model 场景创建衍生 provider）。
    fn api_base(&self) -> String {
        String::new()
    }

    /// 返回 API 密钥（用于 multi-model 场景创建衍生 provider）。
    fn api_key(&self) -> String {
        String::new()
    }

    /// 返回默认生成参数（温度、最大 token 数等）。
    /// 提供默认实现，子类可按需覆盖。
    fn generation_settings(&self) -> GenerationSettings {
        GenerationSettings::default()
    }

    /// 清除指定 cache scope 的缓存状态。
    /// session 回收时调用，避免旧消息残留导致 tool_call 配对错误。
    fn clear_cache_scope(&self, _scope: &str) {}

    /// 返回指定 scope 上一次请求的 cache breakpoint 位置。
    /// 压缩时应保留此位置之前的消息不动，避免破坏 prompt cache 前缀。
    /// 默认返回 0（不保护）。
    fn cache_breakpoint_idx(&self, _scope: &str) -> usize {
        0
    }

    /// 带指数退避重试的聊天方法。
    ///
    /// 工作流程：
    /// 1. 按 RETRY_DELAYS 中的延迟时间进行最多 3 次重试
    /// 2. 每次重试前检查错误是否为临时性错误（如限流、超时）
    /// 3. 非临时性错误（如参数错误）会立即返回，不再重试
    /// 4. 所有重试失败后执行最终尝试
    ///
    /// 注意：此方法始终返回 LLMResponse（不会返回 Err），
    /// 错误信息通过 finish_reason="error" 传递。
    async fn chat_with_retry(
        &self,
        messages: Vec<std::collections::HashMap<String, serde_json::Value>>,
        tools: Option<Vec<serde_json::Value>>,
        model: Option<String>,
        cache_scope: Option<String>,
    ) -> LLMResponse {
        let settings = self.generation_settings();
        let request = ChatRequest {
            messages,
            tools,
            model,
            cache_scope,
            max_tokens: settings.max_tokens,
            temperature: settings.temperature,
        };

        // 经由并发控制器获取许可：同时受全局与单用户两级闸门约束，
        // 并具备排队超时能力（R6.2/R6.5）。user_id 由 task-local 提供（未设置走兜底）。
        let user_id = current_user_id();
        let _permit = match concurrency::acquire_permit(&user_id).await {
            Ok(permit) => permit,
            Err(ConcurrencyError::QueueTimeout { limit_kind }) => {
                warn!(
                    user_id = %user_id,
                    limit_kind = limit_kind.as_str(),
                    "LLM request rejected after concurrency queue timeout"
                );
                return LLMResponse::error(
                    "系统繁忙，请稍后重试（并发排队超时）".to_string(),
                );
            }
        };

        // 按延迟序列依次重试
        for (attempt, delay) in RETRY_DELAYS.iter().enumerate() {
            let response = match self.chat(request.clone()).await {
                Ok(r) => r,
                Err(e) => LLMResponse::error(format!("Error calling LLM: {e}")),
            };

            // 如果不是错误响应，直接返回成功结果
            if response.finish_reason != "error" {
                // 检测空回复（某些上游偶发返回 finish_reason=stop 但无内容）
                let is_empty = response.content.as_ref().map_or(true, |c| c.is_empty())
                    && response.tool_calls.is_empty();
                if is_empty && attempt < RETRY_DELAYS.len() - 1 {
                    warn!(
                        attempt = attempt + 1,
                        "LLM returned empty response (no content, no tool_calls), retrying"
                    );
                    tokio::time::sleep(std::time::Duration::from_secs(RETRY_DELAYS[attempt])).await;
                    continue;
                }
                return response;
            }
            // 如果不是临时性错误，不再重试
            if !is_transient_error(response.content.as_deref()) {
                return response;
            }

            // 记录重试日志
            warn!(
                attempt = attempt + 1,
                total = RETRY_DELAYS.len(),
                delay_s = delay,
                error = response.content.as_deref().unwrap_or(""),
                "LLM transient error, retrying"
            );
            // 等待指定延迟后重试
            tokio::time::sleep(std::time::Duration::from_secs(*delay)).await;
        }

        // 最终尝试（第4次调用），不再重试
        let final_resp = match self.chat(request).await {
            Ok(r) => r,
            Err(e) => LLMResponse::error(format!("Error calling LLM: {e}")),
        };

        // SSE/HTTP 重试耗尽（R10.5 / Property 29）：若最终仍为临时性错误
        // （SSE 超时、连接重置、上游 5xx 等），向用户返回可识别的「请稍后重试」
        // 提示，而非透传失败兜底文案。非临时性错误（如参数错误）保持原样以便排查。
        if final_resp.finish_reason == "error" && is_transient_error(final_resp.content.as_deref())
        {
            warn!(
                error = final_resp.content.as_deref().unwrap_or(""),
                "LLM retries exhausted on transient error, returning retry-later prompt"
            );
            return retry_exhausted_response();
        }

        final_resp
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    /// 失败兜底文案（污染关键词）样例，重试耗尽提示不得包含此类文本。
    const FAILURE_FALLBACK: &str = "I cannot make progress";

    /// 生成失败兜底/污染文案空间中的任意候选短语。
    ///
    /// 涵盖典型失败兜底关键词，以及任意 `[a-z ]{0,20}` 文本，
    /// 用于在该空间上验证「请稍后重试」提示恒不含失败兜底文案。
    fn failure_fallback_phrase() -> impl Strategy<Value = String> {
        prop_oneof![
            Just("I cannot make progress".to_string()),
            Just("error".to_string()),
            Just("blocked".to_string()),
            Just("I'm unable to".to_string()),
            "[a-z ]{0,20}".prop_map(|s| s),
        ]
    }

    proptest! {
        #![proptest_config(ProptestConfig { cases: 100, ..ProptestConfig::default() })]

        // Feature: execution-performance-optimization, Property 29: 重试耗尽返回稍后重试提示而非兜底文案
        #[test]
        fn prop_retry_exhausted_returns_retry_later_not_fallback(phrase in failure_fallback_phrase()) {
            let resp = retry_exhausted_response();
            let content = resp.content.as_deref().unwrap_or("");

            // 可识别的「请稍后重试」提示：非空且包含「稍后重试」
            prop_assert!(!content.is_empty(), "retry-later content must be non-empty");
            prop_assert!(
                content.contains("稍后重试"),
                "content must be identifiable as a retry-later prompt, got: {content}"
            );

            // 不得包含规范失败兜底文案
            prop_assert!(
                !content.contains("I cannot make progress"),
                "retry-later content must not contain the canonical failure-fallback text"
            );

            // 跨失败兜底文案空间：提示不等于任意候选短语；
            // 对规范兜底短语，提示亦不含之（常量恒成立，借属性测试固化该不变量）。
            prop_assert_ne!(content, phrase.as_str());
            if phrase == FAILURE_FALLBACK {
                prop_assert!(!content.contains(&phrase));
            }
        }
    }

    #[test]
    fn retry_later_message_is_identifiable_and_clean() {
        let msg = build_retry_later_message();
        // 非空
        assert!(!msg.trim().is_empty(), "retry-later message must be non-empty");
        // 可识别为「请稍后重试」语义
        assert!(
            msg.contains("稍后重试"),
            "retry-later message must be identifiable as a retry-later prompt, got: {msg}"
        );
        // 不含失败兜底文案
        assert!(
            !msg.contains(FAILURE_FALLBACK),
            "retry-later message must not contain failure-fallback text"
        );
        // 常量与 helper 一致
        assert_eq!(msg, RETRY_LATER_MESSAGE);
    }

    #[test]
    fn retry_exhausted_response_carries_retry_later_message() {
        let resp = retry_exhausted_response();
        let content = resp.content.as_deref().unwrap_or("");
        assert!(content.contains("稍后重试"));
        assert!(!content.contains(FAILURE_FALLBACK));
        assert!(resp.tool_calls.is_empty());
    }
}
