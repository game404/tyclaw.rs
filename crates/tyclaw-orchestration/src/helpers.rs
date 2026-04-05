//! 编排器辅助函数：技能路由、案例优化、上下文预算计算。

use std::collections::HashSet;

use crate::history::normalize_text_for_dedupe;
use crate::types::{
    ContextBudgetPlan, HANDOFF_MAX_CONTENT_CHARS, HANDOFF_MAX_MESSAGES, HISTORY_BUDGET_RATIO,
    MAX_DYNAMIC_INJECTED_SKILLS, MAX_DYNAMIC_SIMILAR_CASES_CHARS, MAX_HISTORY_BUDGET_RATIO,
    MAX_INJECTED_SKILLS, MAX_SIMILAR_CASES_CHARS,
};
use tyclaw_prompt::strip_non_task_user_message;

/// 压缩 similar cases 段：
/// 1) 行级去重；2) 长度截断。
/// 目标是在保留案例信号的前提下避免"案例段吞噬主问题 token"。
pub(crate) fn optimize_similar_cases(cases: &str, max_chars: usize) -> String {
    if cases.is_empty() {
        return String::new();
    }
    let mut seen = HashSet::new();
    let mut lines: Vec<String> = Vec::new();
    for line in cases.lines() {
        let normalized = normalize_text_for_dedupe(line);
        if normalized.is_empty() {
            lines.push(String::new());
            continue;
        }
        if seen.insert(normalized) {
            lines.push(line.to_string());
        }
    }
    let mut merged = lines.join("\n");
    if merged.chars().count() > max_chars {
        merged = truncate_by_chars(&merged, max_chars);
        merged.push_str("\n... (similar cases truncated)");
    }
    merged
}

pub(crate) fn truncate_by_chars(input: &str, max_chars: usize) -> String {
    if max_chars == 0 {
        return String::new();
    }
    match input.char_indices().nth(max_chars) {
        Some((idx, _)) => input[..idx].to_string(),
        None => input.to_string(),
    }
}

pub(crate) fn build_handoff_markdown(
    session_key: &str,
    messages: &[std::collections::HashMap<String, serde_json::Value>],
) -> String {
    let mut out = String::new();
    let now = chrono::Utc::now().to_rfc3339();
    let start = messages.len().saturating_sub(HANDOFF_MAX_MESSAGES);
    let selected = &messages[start..];

    out.push_str("# TyClaw Handoff\n\n");
    out.push_str(&format!("- 时间: {now}\n"));
    out.push_str(&format!("- 会话: `{session_key}`\n"));
    out.push_str(&format!("- 总消息数: {}\n", messages.len()));
    out.push_str(&format!("- 导出消息数: {}\n\n", selected.len()));

    for (i, msg) in selected.iter().enumerate() {
        let role = msg
            .get("role")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");
        let mut content = msg
            .get("content")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        if role == "user" {
            if let Some(cleaned) = strip_non_task_user_message(&content) {
                content = cleaned;
            } else {
                continue;
            }
        }
        content = truncate_by_chars(&content, HANDOFF_MAX_CONTENT_CHARS);

        out.push_str(&format!("## {}. role={role}\n", i + 1));
        if let Some(tool_call_id) = msg.get("tool_call_id").and_then(|v| v.as_str()) {
            out.push_str(&format!("- tool_call_id: `{tool_call_id}`\n"));
        }
        if let Some(tool_calls) = msg.get("tool_calls").and_then(|v| v.as_array()) {
            out.push_str(&format!("- tool_calls: {}\n", tool_calls.len()));
        }
        out.push_str("```\n");
        out.push_str(&content);
        out.push_str("\n```\n\n");
    }
    out
}

/// query 驱动的动态预算分配器。
///
/// 这是启发式策略，不依赖模型分类器，优点是稳定和易调参：
/// - 排障类：提升 cases 预算
/// - 连续追问类：提升 history 预算
/// - 实现/编码类：提升 skills 预算
///
/// 最终会做 clamp，确保预算落在可控范围内。
pub(crate) fn compute_context_budget_plan(query: &str) -> ContextBudgetPlan {
    let q = query.to_lowercase();
    let has_any = |words: &[&str]| words.iter().any(|w| q.contains(w));

    // 默认：历史 45%，技能 8，案例 2500 chars。
    let mut plan = ContextBudgetPlan {
        history_ratio: HISTORY_BUDGET_RATIO,
        max_skills: MAX_INJECTED_SKILLS,
        max_cases_chars: MAX_SIMILAR_CASES_CHARS,
    };

    // 调试排障：给 cases 更高预算，history 适中。
    if has_any(&[
        "报错",
        "错误",
        "异常",
        "失败",
        "traceback",
        "error",
        "timeout",
        "排查",
        "日志",
    ]) {
        plan.history_ratio = 40;
        plan.max_skills = 7;
        plan.max_cases_chars = 3600;
    }

    // 连续对话：提高 history 预算，降低 cases。
    if has_any(&[
        "继续",
        "刚才",
        "上次",
        "前面",
        "这个问题",
        "同一个",
        "再补充",
        "延续",
    ]) {
        plan.history_ratio = 60;
        plan.max_skills = 6;
        plan.max_cases_chars = 1800;
    }

    // 实现/编码任务：技能预算提高，history 维持中等。
    if has_any(&[
        "实现",
        "改代码",
        "重构",
        "写一个",
        "patch",
        "fix",
        "refactor",
        "代码",
    ]) {
        plan.history_ratio = 45;
        plan.max_skills = 10;
        plan.max_cases_chars = 1500;
    }

    // clamp，防止越界。
    plan.history_ratio = plan
        .history_ratio
        .clamp(HISTORY_BUDGET_RATIO, MAX_HISTORY_BUDGET_RATIO);
    plan.max_skills = plan.max_skills.clamp(3, MAX_DYNAMIC_INJECTED_SKILLS);
    plan.max_cases_chars = plan
        .max_cases_chars
        .clamp(800, MAX_DYNAMIC_SIMILAR_CASES_CHARS);
    plan
}
