//! `dispatch_subtasks` 工具：让主控 LLM 在 ReAct 循环中按需调用多模型并行执行。
//!
//! 主控 LLM 自己就是 Planner —— 当它判断任务足够复杂时，
//! 构造子任务列表调用此工具，由 scheduler → executor → reducer 管线执行，
//! 结果作为 tool result 返回给主控 LLM 继续推理。

use std::collections::HashMap;
use std::sync::Arc;
// dispatch_subtasks 不需要全局锁——不同 workspace 的 dispatch 可以并发执行

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tracing::{info, warn};

use tyclaw_tools::Tool;

use crate::app_context::AppContext;
use super::executor::NodeExecutor;
use super::protocol::{FailurePolicy, NodeStatus, TaskNode, TaskPlan};
use super::reducer::RuleReducer;
use super::scheduler::DagScheduler;

const DISPATCH_SUMMARY_START: &str = "[[TYCLAW_DISPATCH_SUMMARY]]";
const DISPATCH_SUMMARY_END: &str = "[[/TYCLAW_DISPATCH_SUMMARY]]";

#[derive(Debug, Clone, Serialize)]
struct DispatchStructuredSummary {
    plan_id: String,
    succeeded: usize,
    failed: usize,
    skipped: usize,
    has_conflicts: bool,
    wall_time_ms: u64,
    nodes: Vec<DispatchNodeSummary>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    skills_used: Vec<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize)]
struct DispatchNodeSummary {
    node_id: String,
    node_status: String,
    declared_result_status: String,
    verified_after_last_edit: Option<bool>,
    ended_with_unverified_changes: bool,
    tools_used_count: usize,
    detail_path: String,
    /// 失败原因 —— 与正文 `output` 文本分离的独立字段（R2.4/R2.5）。
    /// 对 `Failed`/`Blocked` 节点始终非空（无论是否存在其他错误上下文），
    /// 由 `failure_reason_for` 从节点状态 + `record.error` 确定性派生。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    failure_reason: Option<String>,
    /// 子 agent 的全部工具调用记录（用于审计追溯）。
    tool_events: Vec<tyclaw_agent::runtime::ToolExecutionEvent>,
}

/// 多模型分发工具 —— 注册到主控 AgentLoop 的 ToolRegistry 中。
///
/// 主控 LLM 通过调用此工具将复杂任务拆分为多个子任务并行执行：
/// - LLM 在 tool_call 的 arguments 中提供子任务列表
/// - 工具内部构建 DAG → 调度执行 → 归并结果
/// - 归并后的文本作为 tool result 返回给 LLM
pub struct DispatchSubtasksTool {
    scheduler: Arc<DagScheduler>,
    reducer: Arc<RuleReducer>,
    /// 动态生成的 description，包含可用模型和路由规则信息。
    description: String,
    /// 不可变的应用级上下文。
    app: Arc<AppContext>,
}

use super::routing::RoutingPolicy;

impl DispatchSubtasksTool {
    pub fn new(
        executor: Arc<NodeExecutor>,
        reducer: RuleReducer,
        max_concurrency: usize,
        default_timeout_ms: u64,
        routing: &RoutingPolicy,
        app: Arc<AppContext>,
    ) -> Self {
        let scheduler = Arc::new(DagScheduler::new(
            executor,
            Some(max_concurrency),
            Some(default_timeout_ms),
        ));
        let description = Self::build_description(routing, max_concurrency);
        Self {
            scheduler,
            reducer: Arc::new(reducer),
            description,
            app,
        }
    }

    /// 保存 dispatch session 到 `logs/snap/dispatch/dispatch_<timestamp>_<plan_id>/`。
    ///
    /// 目录结构：
    /// - `plan.json` — 任务计划（DAG 结构）
    /// - `node_<id>.json` — 每个节点的 ExecutionRecord（含消息历史）
    fn save_dispatch_session(&self, plan: &TaskPlan, records: &[super::protocol::ExecutionRecord]) {
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        let dir = self
            .app.workspace
            .join("logs")
            .join("snap")
            .join("dispatch")
            .join(format!("dispatch_{ts}_{}", plan.id));
        if let Err(e) = std::fs::create_dir_all(&dir) {
            warn!(error = %e, "Failed to create dispatch session dir");
            return;
        }

        // 写 plan.json
        if let Ok(plan_json) = serde_json::to_string_pretty(plan) {
            let _ = std::fs::write(dir.join("plan.json"), plan_json);
        }

        // 写每个节点的 record（含 messages）
        for record in records {
            let filename = format!("node_{}.json", record.node_id);
            if let Ok(record_json) = serde_json::to_string_pretty(record) {
                let _ = std::fs::write(dir.join(&filename), record_json);
            }
        }

        info!(
            dir = %dir.display(),
            node_count = records.len(),
            "Saved dispatch session snapshot"
        );
    }

    /// 构建包含路由表和模型信息的动态 description。
    fn build_description(routing: &RoutingPolicy, max_concurrency: usize) -> String {
        // 构建路由表文本
        let mut routing_table = String::new();
        for rule in &routing.rules {
            routing_table.push_str(&format!(
                "  - {} -> {}\n",
                rule.node_type_pattern, rule.target_model
            ));
        }
        routing_table.push_str(&format!(
            "  - (other) -> {} (default)\n",
            routing.default_model
        ));

        // 从 config/prompts/ 加载模板并替换变量
        super::prompt_loader::dispatch_tool_description()
            .replace("{routing_table}", &routing_table)
            .replace("{max_concurrency}", &max_concurrency.to_string())
    }
}

/// LLM 传入的单个子任务描述。
#[derive(Debug, Deserialize)]
struct SubtaskInput {
    /// 子任务唯一 ID（如 "task_1", "analyze", "code_review"）。
    id: String,
    /// 任务类型，用于路由到目标模型（coding/reasoning/search/summarize/review/general）。
    #[serde(default = "default_node_type")]
    node_type: String,
    /// 子任务的详细指令。
    prompt: String,
    /// 依赖的上游任务 ID 列表（这些任务完成后才执行当前任务）。
    #[serde(default)]
    dependencies: Vec<String>,
    /// 可选的模型覆盖（跳过路由规则，直接指定模型）。
    #[serde(default)]
    model_override: Option<String>,
}

fn default_node_type() -> String {
    "general".into()
}

#[async_trait]
impl Tool for DispatchSubtasksTool {
    fn name(&self) -> &str {
        "dispatch_subtasks"
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "subtasks": {
                    "type": "array",
                    "description": "List of subtasks. Each subtask will be dispatched to the best-matched LLM model for execution.",
                    "items": {
                        "type": "object",
                        "properties": {
                            "id": {
                                "type": "string",
                                "description": "Unique subtask ID, used for dependency references (e.g., 'research_1', 'code_impl')"
                            },
                            "node_type": {
                                "type": "string",
                                "description": "Task type that determines model routing. Prefer canonical types: coding, reasoning, search, review, summarize, planning, general. Compatibility aliases: analysis, research, synthesis, critique, design, coding_deep.",
                                "enum": ["coding", "coding_deep", "reasoning", "analysis", "search", "research", "summarize", "synthesis", "review", "critique", "planning", "design", "general"]
                            },
                            "prompt": {
                                "type": "string",
                                "description": "Detailed instructions for the subtask. Clearly specify the goal and expected output format."
                            },
                            "dependencies": {
                                "type": "array",
                                "items": { "type": "string" },
                                "description": "List of upstream task IDs this task depends on. Their outputs will be injected as context into this task. IMPORTANT: If a subtask needs another subtask's output (e.g., docs need code), it MUST declare the dependency here, otherwise it will execute without that context."
                            },
                            "model_override": {
                                "type": "string",
                                "description": "Optional: bypass routing rules and directly specify the target model (e.g., 'openai/claude-opus-4.6')"
                            }
                        },
                        "required": ["id", "node_type", "prompt"]
                    }
                },
                "context": {
                    "type": "string",
                    "description": "Key context, findings, and constraints from your analysis that sub-agents should know. \
                        Include: data structure insights, known issues, important constraints, file format details, \
                        previous attempts and lessons learned. This will be written to main_llm.md for all sub-agents to reference. \
                        The more specific and actionable, the better — sub-agents won't have your conversation history."
                },
                "failure_policy": {
                    "type": "string",
                    "enum": ["fail_fast", "best_effort"],
                    "description": "Failure policy. fail_fast: abort all if any subtask fails; best_effort: continue despite partial failures. Defaults to best_effort."
                }
            },
            "required": ["subtasks"]
        })
    }

    fn risk_level(&self) -> tyclaw_tools::RiskLevel {
        tyclaw_tools::RiskLevel::Write
    }

    async fn execute(&self, params: HashMap<String, Value>) -> String {
        // 解析子任务列表
        let subtasks_val = match params.get("subtasks") {
            Some(v) => v.clone(),
            None => return "Error: Missing required parameter 'subtasks'".into(),
        };

        // 兼容 LLM 把数组序列化为字符串的 bug：
        // 有些模型（Claude Opus 偶发）会传 subtasks: "[{...}]"（字符串）而非 [{...}]（数组）。
        // 检测到字符串时尝试二次解析。
        let subtasks_parsed = if let Value::String(s) = &subtasks_val {
            match serde_json::from_str::<Value>(s) {
                Ok(v) => {
                    warn!("subtasks was a string, auto-parsed to array (LLM tool_call format bug)");
                    v
                }
                Err(_) => subtasks_val,
            }
        } else {
            subtasks_val
        };

        let subtasks: Vec<SubtaskInput> = match serde_json::from_value(subtasks_parsed) {
            Ok(v) => v,
            Err(e) => return format!("Error: Failed to parse subtasks: {e}"),
        };

        if subtasks.is_empty() {
            return "Error: subtasks array is empty".into();
        }

        let failure_policy = params
            .get("failure_policy")
            .and_then(|v| v.as_str())
            .and_then(|s| match s {
                "fail_fast" => Some(FailurePolicy::FailFast),
                "best_effort" => Some(FailurePolicy::BestEffort),
                _ => None,
            })
            .unwrap_or(FailurePolicy::BestEffort);

        // 构建 TaskPlan
        let mut nodes: Vec<TaskNode> = subtasks
            .iter()
            .map(|s| TaskNode {
                id: s.id.clone(),
                node_type: s.node_type.clone(),
                prompt: s.prompt.clone(),
                dependencies: s.dependencies.clone(),
                model_override: s.model_override.clone(),
                timeout_ms: None,
                max_retries: None,
                acceptance_criteria: None,
            })
            .collect();

        // 从 dependencies 构建 edges
        let mut edges: Vec<(String, String)> = subtasks
            .iter()
            .flat_map(|s| {
                s.dependencies
                    .iter()
                    .map(move |dep| (dep.clone(), s.id.clone()))
            })
            .collect();

        // 安全兜底：如果 LLM 没有设置任何依赖（edge_count=0）且有 2+ 个子任务，
        // 自动按提交顺序串联。串行比错误并行安全——LLM 经常忘记设依赖。
        if edges.is_empty() && subtasks.len() >= 2 {
            warn!(
                subtask_count = subtasks.len(),
                "No dependencies declared — auto-chaining subtasks sequentially (LLM likely forgot)"
            );
            for i in 1..subtasks.len() {
                edges.push((subtasks[i - 1].id.clone(), subtasks[i].id.clone()));
            }
            // 同步更新 nodes 的 dependencies
            for i in 1..nodes.len() {
                nodes[i].dependencies.push(subtasks[i - 1].id.clone());
            }
        }

        let plan = TaskPlan {
            id: format!(
                "{:x}",
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_nanos()
            ),
            nodes,
            edges,
            failure_policy,
            metadata: Default::default(),
        };

        // 校验计划
        if let Err(e) = plan.validate() {
            return format!("Error: Invalid task plan: {e}");
        }

        // 主 LLM 的上下文笔记，per-dispatch 传入 executor
        let main_context: Option<String> = params
            .get("context")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .filter(|s| !s.is_empty());

        let subtask_summary: Vec<String> = plan
            .nodes
            .iter()
            .map(|n| format!("{}({})", n.id, n.node_type))
            .collect();
        info!(
            plan_id = %plan.id,
            subtasks = ?subtask_summary,
            "[dispatch] executing plan"
        );
        info!(
            plan_id = %plan.id,
            subtask_count = plan.nodes.len(),
            edge_count = plan.edges.len(),
            "dispatch_subtasks: executing plan"
        );

        // 为本次 dispatch 创建隔离的运行目录。
        // 有 sandbox 时放在用户 workdir 里（volume mount 可见），否则放项目根目录。
        let dispatch_instance = format!(
            "dispatch_{}_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis(),
            sanitize_dispatch_component(&plan.id),
        );
        let dispatch_rel = std::path::PathBuf::from("dispatches").join(&dispatch_instance);
        let dispatch_dir = if let Some(sandbox) = tyclaw_sandbox::current_sandbox() {
            let workspace_key = sandbox.workspace_key();
            tyclaw_control::workspace_path(&self.app.workspace, workspace_key)
                .join("work")
                .join(&dispatch_rel)
        } else {
            self.app.workspace.join(&dispatch_rel)
        };
        let _ = std::fs::create_dir_all(&dispatch_dir);

        // ── 单任务短路优化 ──
        // 只有 1 个子任务且无依赖时，跳过 DAG 调度 + 归并开销，
        // 直接执行并返回结果，省掉不必要的编排层。
        if plan.nodes.len() == 1 && plan.edges.is_empty() {
            info!(plan_id = %plan.id, node_id = %plan.nodes[0].id, "Single-task shortcut: bypassing DAG scheduler");
            let record = self
                .scheduler
                .executor()
                .execute(&plan.nodes[0], &[], &dispatch_dir, main_context.as_deref())
                .await;
            let status_str = format!("{:?}", record.status);
            let output = record.output.clone().unwrap_or_default();
            let error = record.error.clone();
            let duration_ms = record.duration_ms;

            if self.app.write_snapshot {
                self.save_dispatch_session(&plan, &[record.clone()]);
            }

            // 单任务也写临时文件
            let detail_file = dispatch_dir.join(format!("{}.md", plan.nodes[0].id));

            let duration_s = duration_ms as f64 / 1000.0;

            return if record.status == super::protocol::NodeStatus::Success {
                let detail_content = format!(
                    "# {} (Success)\n\nDuration: {:.1}s\nTools: {:?}\n\n---\n\n{}\n",
                    plan.nodes[0].id, duration_s, record.tools_used, output
                );
                let display_path =
                    write_dispatch_file(&detail_file, &detail_content, &dispatch_dir);
                let summary = DispatchStructuredSummary {
                    plan_id: plan.id.clone(),
                    succeeded: 1,
                    failed: 0,
                    skipped: 0,
                    has_conflicts: false,
                    wall_time_ms: duration_ms,
                    nodes: vec![build_dispatch_node_summary(&record, display_path.clone())],
                    skills_used: record.skills_used.clone(),
                };

                let preview = if output.len() > 300 {
                    let boundary = output.floor_char_boundary(300);
                    format!("{}...", &output[..boundary])
                } else {
                    output.clone()
                };
                let hint = super::prompt_loader::dispatch_single_result_hint();
                append_dispatch_summary_metadata(
                    format!("✅ **{}** ({:.0}s): {}\n   Detail: `{}`\n\n---\nStats: 1 succeeded | {:.1}s\n{}",
                        plan.nodes[0].id, duration_s, preview, display_path, duration_s, hint.trim()),
                    &summary,
                )
            } else {
                let err_msg = error.unwrap_or_else(|| "unknown error".into());
                let detail_content =
                    format!("# {} (FAILED)\n\nError: {}\n", plan.nodes[0].id, err_msg);
                let display_path =
                    write_dispatch_file(&detail_file, &detail_content, &dispatch_dir);
                let summary = DispatchStructuredSummary {
                    plan_id: plan.id.clone(),
                    succeeded: 0,
                    failed: 1,
                    skipped: 0,
                    has_conflicts: false,
                    wall_time_ms: duration_ms,
                    nodes: vec![build_dispatch_node_summary(&record, display_path.clone())],
                    skills_used: record.skills_used.clone(),
                };

                append_dispatch_summary_metadata(
                    format!("❌ **{}** ({:.0}s): {}\n   Detail: `{}`\n\n---\nStats: 0 succeeded / 1 failed ({status_str}) | {:.1}s",
                        plan.nodes[0].id, duration_s, err_msg, display_path, duration_s),
                    &summary,
                )
            };
        }

        // 执行（多任务走 DAG 调度）
        let records = self
            .scheduler
            .execute(&plan, &dispatch_dir, main_context.as_deref())
            .await;

        // 归并
        let report = self.reducer.reduce(&records).await;

        // 构建结构化输出
        let succeeded = records
            .iter()
            .filter(|r| r.status == super::protocol::NodeStatus::Success)
            .count();
        let failed = records
            .iter()
            .filter(|r| r.status == super::protocol::NodeStatus::Failed)
            .count();
        let blocked = records
            .iter()
            .filter(|r| r.status == super::protocol::NodeStatus::Blocked)
            .count();
        let skipped = records
            .iter()
            .filter(|r| r.status == super::protocol::NodeStatus::Skipped)
            .count();

        let total_input_tokens: u64 = records.iter().map(|r| r.input_tokens).sum();
        let total_output_tokens: u64 = records.iter().map(|r| r.output_tokens).sum();
        let total_duration_ms: u64 = records.iter().map(|r| r.duration_ms).max().unwrap_or(0);

        info!(
            plan_id = %plan.id,
            succeeded, failed, blocked, skipped,
            total_input_tokens, total_output_tokens,
            wall_time_ms = total_duration_ms,
            "dispatch_subtasks: completed"
        );

        // === 保存 dispatch session（snapshot 开启时） ===
        if self.app.write_snapshot {
            self.save_dispatch_session(&plan, &records);
        }

        // 将每个子任务的完整输出写到临时文件，返回给主控的只有摘要 + 路径。
        // 这样避免大输出被 compress_tool_results 截断导致主控丢失关键信息。

        let mut result = String::new();
        let mut structured_nodes = Vec::new();

        if report.has_conflicts {
            result.push_str("[WARNING: subtask outputs may contain contradictions]\n\n");
        }

        // 每个子任务一行摘要 + 详情文件路径
        for rec in &records {
            let status_icon = match rec.status {
                super::protocol::NodeStatus::Success => "✅",
                super::protocol::NodeStatus::Failed => "❌",
                super::protocol::NodeStatus::Blocked => "🚫",
                super::protocol::NodeStatus::Skipped => "⏭️",
                _ => "❓",
            };
            let duration_s = rec.duration_ms as f64 / 1000.0;
            let output = rec.output.as_deref().unwrap_or("");
            let tools_count = rec.tools_used.len();

            // 写完整输出到临时文件
            let detail_file = dispatch_dir.join(format!("{}.md", rec.node_id));
            let detail_content = if let Some(ref err) = rec.error {
                let mut detail = format!("# {} (FAILED)\n\nError: {}\n", rec.node_id, err);
                // 超时或失败时，扫描工作区看 sub-agent 是否已经写了目标文件，
                // 帮助主 LLM 了解 partial progress 而不是只看到 "timeout"。
                if err == "timeout" || err.contains("max iterations") {
                    detail.push_str("\n## Partial Progress\n");
                    detail.push_str(
                        "The sub-agent timed out but may have written files to the workspace.\n",
                    );
                    // 扫描 works 目录下最近修改的文件（可能包含 workspace 产物）
                    let personal_dir = self.app.workspace.join("works");
                    if personal_dir.exists() {
                        let mut recent_files = Vec::new();
                        scan_recent_files(&personal_dir, &mut recent_files, 2);
                        if !recent_files.is_empty() {
                            detail.push_str("\nFiles found in workspace (may be partial):\n");
                            for (path, size) in &recent_files {
                                detail.push_str(&format!("- `{}` ({} bytes)\n", path, size));
                            }
                            detail.push_str("\n**The main LLM should read and verify these files before deciding \
                                whether to re-dispatch or fix them directly.**\n");
                        }
                    }
                    // 也扫描 tmp/ 下的辅助脚本
                    let tmp_dir = self.app.workspace.join("tmp");
                    if tmp_dir.exists() {
                        let mut tmp_files = Vec::new();
                        scan_recent_files(&tmp_dir, &mut tmp_files, 1);
                        if !tmp_files.is_empty() {
                            detail.push_str(&format!(
                                "\nTemp/scratch files: {}\n",
                                tmp_files
                                    .iter()
                                    .map(|(p, _)| p.as_str())
                                    .collect::<Vec<_>>()
                                    .join(", ")
                            ));
                        }
                    }
                }
                detail
            } else {
                format!(
                    "# {} ({})\n\nModel: {}\nDuration: {:.1}s\nTools: {:?}\n\n---\n\n{}\n",
                    rec.node_id,
                    format!("{:?}", rec.status),
                    rec.model,
                    duration_s,
                    rec.tools_used,
                    output,
                )
            };
            let display_path = write_dispatch_file(&detail_file, &detail_content, &dispatch_dir);
            structured_nodes.push(build_dispatch_node_summary(rec, display_path.clone()));

            // 输出的前 200 chars 作为预览
            let preview = if output.len() > 200 {
                let boundary = output.floor_char_boundary(200);
                format!("{}...", &output[..boundary])
            } else {
                output.to_string()
            };

            result.push_str(&format!(
                "{} **{}** ({:.0}s, {} tools): {}\n   Detail: `{}`\n\n",
                status_icon, rec.node_id, duration_s, tools_count, preview, display_path,
            ));
        }

        let hint = super::prompt_loader::dispatch_multi_result_hint();
        result.push_str(&format!(
            "---\nStats: {succeeded} succeeded / {failed} failed / {skipped} skipped | \
             wall time {:.1}s\n{}",
            total_duration_ms as f64 / 1000.0,
            hint.trim(),
        ));
        let all_sub_skills: Vec<serde_json::Value> = records
            .iter()
            .flat_map(|r| r.skills_used.iter().cloned())
            .collect();

        let summary = DispatchStructuredSummary {
            plan_id: plan.id.clone(),
            succeeded,
            failed,
            skipped,
            has_conflicts: report.has_conflicts,
            wall_time_ms: total_duration_ms,
            nodes: structured_nodes,
            skills_used: all_sub_skills,
        };

        append_dispatch_summary_metadata(result, &summary)
    }
}

/// 写 dispatch 文件到 host（volume mount 模式下容器自动可见）。
/// 返回 LLM 应使用的路径（有 sandbox 时返回容器内路径，否则返回 host 路径）。
fn write_dispatch_file(
    host_path: &std::path::Path,
    content: &str,
    dispatch_dir: &std::path::Path,
) -> String {
    let _ = std::fs::write(host_path, content);
    if let Some(sandbox) = tyclaw_sandbox::current_sandbox() {
        // 返回容器内路径：/user/work/dispatches/{dispatch_id}/{filename}
        let dispatch_rel = dispatch_container_rel(dispatch_dir);
        let filename = host_path
            .file_name()
            .map(|f| f.to_string_lossy().to_string())
            .unwrap_or_default();
        format!("{}/{}/{}", sandbox.workspace_root(), dispatch_rel, filename)
    } else {
        host_path.display().to_string()
    }
}

fn dispatch_container_rel(dispatch_dir: &std::path::Path) -> String {
    let parent = dispatch_dir
        .parent()
        .and_then(|p| p.file_name())
        .map(|f| f.to_string_lossy().to_string())
        .unwrap_or_else(|| "dispatches".to_string());
    let name = dispatch_dir
        .file_name()
        .map(|f| f.to_string_lossy().to_string())
        .unwrap_or_else(|| "dispatch".to_string());
    format!("{parent}/{name}")
}

fn sanitize_dispatch_component(input: &str) -> String {
    let mut out = String::with_capacity(input.len().min(48));
    for ch in input.chars() {
        if out.len() >= 48 {
            break;
        }
        if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_') {
            out.push(ch);
        } else {
            out.push('_');
        }
    }
    if out.is_empty() {
        "dispatch".to_string()
    } else {
        out
    }
}

fn append_dispatch_summary_metadata(
    mut human_text: String,
    summary: &DispatchStructuredSummary,
) -> String {
    if let Ok(summary_json) = serde_json::to_string(summary) {
        human_text.push_str("\n\n");
        human_text.push_str(DISPATCH_SUMMARY_START);
        human_text.push('\n');
        human_text.push_str(&summary_json);
        human_text.push('\n');
        human_text.push_str(DISPATCH_SUMMARY_END);
    }
    human_text
}

fn build_dispatch_node_summary(
    record: &super::protocol::ExecutionRecord,
    detail_path: String,
) -> DispatchNodeSummary {
    let diagnostics = record.diagnostics_summary.as_ref();
    DispatchNodeSummary {
        node_id: record.node_id.clone(),
        node_status: match record.status {
            super::protocol::NodeStatus::Pending => "pending",
            super::protocol::NodeStatus::Running => "running",
            super::protocol::NodeStatus::Success => "success",
            super::protocol::NodeStatus::Failed => "failed",
            super::protocol::NodeStatus::Blocked => "blocked",
            super::protocol::NodeStatus::Skipped => "skipped",
        }
        .into(),
        declared_result_status: node_status_to_declared_status(record.status).to_string(),
        verified_after_last_edit: diagnostics.and_then(|d| d.verified_after_last_edit),
        ended_with_unverified_changes: diagnostics
            .map(|d| d.ended_with_unverified_changes)
            .unwrap_or(false),
        tools_used_count: record.tools_used.len(),
        detail_path,
        failure_reason: failure_reason_for(record.status, record.error.as_deref()),
        tool_events: record.tool_events.clone(),
    }
}

/// 从节点真实状态 + 可选错误文本确定性派生独立的失败原因（R2.4/R2.5）。
///
/// 语义约束：
/// - `Failed` / `Blocked`：**始终**返回非空原因，独立于正文 `output` 文本。
///   若 `error` 提供了非空文本则采用之，否则回退到由状态决定的确定性默认值。
/// - `Skipped` / `Pending` / `Running`：仅当显式携带非空 `error` 时填充，否则 `None`。
/// - `Success`：永远返回 `None`。
pub(crate) fn failure_reason_for(status: NodeStatus, error: Option<&str>) -> Option<String> {
    let explicit = error
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());
    match status {
        NodeStatus::Failed => Some(explicit.unwrap_or_else(|| "node failed".to_string())),
        NodeStatus::Blocked => Some(explicit.unwrap_or_else(|| "node blocked".to_string())),
        NodeStatus::Skipped | NodeStatus::Pending | NodeStatus::Running => explicit,
        NodeStatus::Success => None,
    }
}

/// `NodeStatus` → `declared_result_status` 语义映射（R2.3, R2.6）。
///
/// 声明状态直接由节点的真实 `NodeStatus` 推导，而非从正文文本解析。
/// 语义约束：仅 `Success` 映射到成功语义 `"success"`；其余所有状态
/// （`Failed`/`Blocked`/`Skipped` 以及非终态 `Pending`/`Running`）均映射到
/// 失败语义 `"failed"`，绝不返回成功语义值（如 `ok`/`success`）。
pub(crate) fn node_status_to_declared_status(status: NodeStatus) -> &'static str {
    match status {
        NodeStatus::Success => "success",
        NodeStatus::Failed
        | NodeStatus::Blocked
        | NodeStatus::Skipped
        | NodeStatus::Pending
        | NodeStatus::Running => "failed",
    }
}

/// 递归扫描目录下的文件，收集 (相对路径, 字节数)。
/// `max_depth` 控制递归深度，避免遍历太深。
fn scan_recent_files(dir: &std::path::Path, out: &mut Vec<(String, u64)>, max_depth: usize) {
    if max_depth == 0 {
        return;
    }
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with('.') || name == "__pycache__" || name == "tmp" {
            continue;
        }
        if path.is_dir() {
            scan_recent_files(&path, out, max_depth - 1);
        } else {
            let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
            if size > 0 {
                out.push((path.to_string_lossy().to_string(), size));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::failure_reason_for;
    use super::node_status_to_declared_status;
    use super::NodeStatus;
    use proptest::prelude::*;

    /// NodeStatus 生成器：覆盖全部 6 个变体。
    fn node_status_strategy() -> impl Strategy<Value = NodeStatus> {
        prop_oneof![
            Just(NodeStatus::Pending),
            Just(NodeStatus::Running),
            Just(NodeStatus::Success),
            Just(NodeStatus::Failed),
            Just(NodeStatus::Blocked),
            Just(NodeStatus::Skipped),
        ]
    }

    proptest! {
        #![proptest_config(ProptestConfig { cases: 100, ..Default::default() })]

        // Feature: execution-performance-optimization, Property 6: 状态到声明状态的映射保持失败语义
        #[test]
        fn prop_declared_status_preserves_failure_semantics(
            status in node_status_strategy()
        ) {
            let declared = node_status_to_declared_status(status);
            if status == NodeStatus::Success {
                prop_assert_eq!(declared, "success");
            } else {
                // 失败/受阻/跳过/非终态：绝不返回成功语义值
                prop_assert_ne!(declared, "success");
                prop_assert_ne!(declared, "ok");
            }
        }

        // Feature: execution-performance-optimization, Property 7: 失败节点始终附带独立失败原因
        #[test]
        fn prop_failed_nodes_always_have_independent_reason(
            status in node_status_strategy(),
            error in prop_oneof![
                Just(None),
                Just(Some(String::new())),
                Just(Some("   ".to_string())),
                "[a-z ]{0,20}".prop_map(Some),
            ],
        ) {
            let r = failure_reason_for(status, error.as_deref());
            if status == NodeStatus::Failed || status == NodeStatus::Blocked {
                // 始终非空，独立于（可能为空/空白/缺失的）error 上下文
                prop_assert!(r.is_some());
                prop_assert!(!r.unwrap().trim().is_empty());
            } else if status == NodeStatus::Success {
                prop_assert!(r.is_none());
            }
        }
    }

    #[test]
    fn success_maps_to_success_semantics() {
        assert_eq!(node_status_to_declared_status(NodeStatus::Success), "success");
    }

    #[test]
    fn failure_states_never_map_to_success_semantics() {
        for status in [
            NodeStatus::Failed,
            NodeStatus::Blocked,
            NodeStatus::Skipped,
            NodeStatus::Pending,
            NodeStatus::Running,
        ] {
            let declared = node_status_to_declared_status(status);
            assert_eq!(
                declared, "failed",
                "non-success status {status:?} must map to failure semantics"
            );
            assert_ne!(declared, "success");
            assert_ne!(declared, "ok");
        }
    }

    #[test]
    fn failed_and_blocked_always_have_non_empty_reason() {
        for status in [NodeStatus::Failed, NodeStatus::Blocked] {
            // 无错误上下文时仍填充确定性默认值
            let reason = failure_reason_for(status, None);
            assert!(
                reason.as_deref().map(|s| !s.trim().is_empty()).unwrap_or(false),
                "{status:?} must yield a non-empty failure_reason even without error context"
            );
            // 空白错误文本也回退到默认值
            let reason_blank = failure_reason_for(status, Some("   "));
            assert!(
                reason_blank.as_deref().map(|s| !s.trim().is_empty()).unwrap_or(false),
                "{status:?} must fall back to a default reason for blank error text"
            );
        }
    }

    #[test]
    fn explicit_error_is_used_when_present() {
        assert_eq!(
            failure_reason_for(NodeStatus::Failed, Some("boom")).as_deref(),
            Some("boom")
        );
        assert_eq!(
            failure_reason_for(NodeStatus::Blocked, Some("tainted output")).as_deref(),
            Some("tainted output")
        );
    }

    #[test]
    fn success_never_has_reason_and_others_only_with_error() {
        assert_eq!(failure_reason_for(NodeStatus::Success, None), None);
        assert_eq!(failure_reason_for(NodeStatus::Success, Some("ignored")), None);
        assert_eq!(failure_reason_for(NodeStatus::Skipped, None), None);
        assert_eq!(
            failure_reason_for(NodeStatus::Skipped, Some("dependency failed")).as_deref(),
            Some("dependency failed")
        );
    }
}
