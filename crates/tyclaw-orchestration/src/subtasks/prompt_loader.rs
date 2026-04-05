//! 编排层提示词加载（基于 tyclaw_agent::prompt_store）。

use std::path::Path;

pub fn init(workspace: &Path) {
    tyclaw_prompt::prompt_store::init(workspace);
}

pub fn sub_agent_guidelines() -> String {
    tyclaw_prompt::prompt_store::get("guidelines_default")
}

pub fn sub_agent_guidelines_coding() -> String {
    tyclaw_prompt::prompt_store::get("guidelines_coding")
}

pub fn node_type_prompt(node_type: &str) -> String {
    let canonical = match node_type {
        "coding_deep" => "coding",
        "research" => "search",
        "synthesis" => "reasoning",
        "critique" => "review",
        "design" => "planning",
        "analysis" => "reasoning",
        other => other,
    };
    tyclaw_prompt::prompt_store::get_node_type(canonical)
}

pub fn workspace_hint() -> String {
    tyclaw_prompt::prompt_store::get("workspace_hint")
}

pub fn subagent_execution_baseline() -> String {
    tyclaw_prompt::prompt_store::get("subagent_execution_baseline")
}

pub fn dispatch_tool_description() -> String {
    tyclaw_prompt::prompt_store::get("dispatch_tool_description")
}

pub fn reducer_prompt() -> String {
    tyclaw_prompt::prompt_store::get("reducer_prompt")
}

pub fn planner_system_prompt() -> String {
    tyclaw_prompt::prompt_store::get("planner_system_prompt")
}

pub fn case_extractor_prompt() -> String {
    tyclaw_prompt::prompt_store::get("case_extractor_prompt")
}

pub fn memory_consolidation_prompt() -> String {
    tyclaw_prompt::prompt_store::get("memory_consolidation_prompt")
}

pub fn dispatch_single_result_hint() -> String {
    tyclaw_prompt::prompt_store::get("dispatch_single_result_hint")
}

pub fn dispatch_multi_result_hint() -> String {
    tyclaw_prompt::prompt_store::get("dispatch_multi_result_hint")
}

pub fn upstream_truncated_hint(detail_path: &str) -> String {
    tyclaw_prompt::prompt_store::get("upstream_truncated_hint")
        .replace("{detail_path}", detail_path)
}

pub fn upstream_full_hint(detail_path: &str) -> String {
    tyclaw_prompt::prompt_store::get("upstream_full_hint")
        .replace("{detail_path}", detail_path)
}
