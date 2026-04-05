//! LLM 驱动的案例摘要提取器。
//!
//! 任务完成后，用一轮轻量 LLM 调用从 question + answer 生成结构化 case 摘要。
//! 替代旧的正则提取方式（extractor.rs），提取质量大幅提升。

use std::collections::HashMap;

use serde_json::Value;
use sha2::{Digest, Sha256};
use tracing::{info, warn};

use tyclaw_provider::LLMProvider;

use crate::case_store::CaseRecord;

/// 用 LLM 从完成的任务中提取结构化案例摘要。
///
/// 输入：
/// - `provider`: LLM provider（用于一轮调用，建议用主控模型或便宜模型）
/// - `user_message`: 用户原始输入
/// - `final_answer`: LLM 最终回复
/// - `tools_used`: 使用过的工具列表（全量）
/// - `iterations`: 迭代轮次
/// - `workspace_id` / `user_id` / `duration_seconds`: 元数据
///
/// 返回 CaseRecord（始终返回，不再由正则判断是否"看起来像已解决"）。
pub async fn llm_extract_case(
    provider: &dyn LLMProvider,
    user_message: &str,
    final_answer: &str,
    tools_used: &[String],
    iterations: usize,
    workspace_id: &str,
    user_id: &str,
    duration_seconds: f64,
) -> Option<CaseRecord> {
    // 过滤：太短的对话不值得保存
    if final_answer.len() < 50 || tools_used.is_empty() {
        return None;
    }

    // 去重工具列表
    let mut unique_tools: Vec<String> = Vec::new();
    for t in tools_used {
        if !unique_tools.contains(t) {
            unique_tools.push(t.clone());
        }
    }

    // 截取 answer 的前 2000 字符（节省 token）
    let answer_preview = if final_answer.len() > 2000 {
        &final_answer[..final_answer.floor_char_boundary(2000)]
    } else {
        final_answer
    };

    let system_prompt = tyclaw_prompt::prompt_store::get("case_extractor_prompt");

    let user_prompt = format!(
        "用户输入：\n{}\n\nAI 回复（截取）：\n{}\n\n使用工具：{}\n迭代轮次：{}",
        user_message,
        answer_preview,
        unique_tools.join(", "),
        iterations,
    );

    let messages = vec![
        {
            let mut m = HashMap::new();
            m.insert("role".into(), Value::String("system".into()));
            m.insert("content".into(), Value::String(system_prompt.clone()));
            m
        },
        {
            let mut m = HashMap::new();
            m.insert("role".into(), Value::String("user".into()));
            m.insert("content".into(), Value::String(user_prompt));
            m
        },
    ];

    let response = provider.chat_with_retry(messages, None, None, None).await;

    if response.finish_reason == "error" {
        warn!("LLM case extraction failed: {:?}", response.content);
        return None;
    }

    let content = response.content.unwrap_or_default();

    // 解析 JSON
    let json_str = content.trim().trim_start_matches("```json").trim_end_matches("```").trim();
    let parsed: Value = match serde_json::from_str(json_str) {
        Ok(v) => v,
        Err(e) => {
            warn!(error = %e, content = %content, "Failed to parse LLM case extraction JSON");
            return None;
        }
    };

    // 检查是否应跳过
    if parsed.get("skip").and_then(|v| v.as_bool()).unwrap_or(false) {
        info!("LLM case extraction: skipped (trivial conversation)");
        return None;
    }

    let task_desc = parsed.get("task_description")
        .and_then(|v| v.as_str())
        .unwrap_or(user_message)
        .to_string();
    let task_type = parsed.get("task_type")
        .and_then(|v| v.as_str())
        .unwrap_or("general")
        .to_string();
    let approach = parsed.get("approach")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let outcome = parsed.get("outcome")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let key_insight = parsed.get("key_insight")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    // 生成确定性 ID
    let mut hasher = Sha256::new();
    hasher.update(format!("{}\n{}", user_message, final_answer).as_bytes());
    let hash = hasher.finalize();
    let case_id = hex::encode(&hash[..6]);

    let mut record = CaseRecord::new(&task_desc, workspace_id);
    record.case_id = case_id;
    record.task_type = task_type;
    record.approach = approach;
    record.outcome = outcome;
    record.key_insight = key_insight;
    record.tools_used = unique_tools;
    record.iterations = iterations;
    record.user_id = user_id.to_string();
    record.duration_seconds = duration_seconds;

    info!(
        case_id = %record.case_id,
        task_type = %record.task_type,
        "LLM case extraction successful"
    );

    Some(record)
}
