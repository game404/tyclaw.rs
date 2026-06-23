//! 集成测试（任务 10.3）：子任务 node 与 dispatch 超时时序。
//!
//! 覆盖：
//! - R7.2：per-node 超时——node 执行超过其有效超时（`min(node.timeout_ms, node_max_duration)`）
//!   时被终止，产出 `Failed` + `error:"timeout"` 记录。
//! - R7.4：dispatch 整体超时——链路整体执行超过 `dispatch_max_duration` 时终止未完成 node，
//!   已完成 node 的部分结果被保留，未完成 node 记 `Failed` + `error:"chain_timeout"`。
//!
//! 节点执行时长通过一个**脚本化的 LLMProvider**控制：其 `chat` 按配置 sleep 指定时长，
//! 从而让真实的 `DagScheduler` + `NodeExecutor` + mini `AgentLoop` 链路在确定的时序下触发
//! per-node / dispatch 两级超时。整个测试以毫秒级时长运行，保持快速与确定性。

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;

use tyclaw_orchestration::subtasks::executor::NodeExecutor;
use tyclaw_orchestration::subtasks::prompt_loader;
use tyclaw_orchestration::subtasks::protocol::{
    FailurePolicy, NodeStatus, PlanMetadata, TaskNode, TaskPlan,
};
use tyclaw_orchestration::subtasks::routing::RoutingPolicy;
use tyclaw_orchestration::subtasks::scheduler::DagScheduler;
use tyclaw_orchestration::AppContext;

use tyclaw_provider::types::{ChatRequest, LLMResponse};
use tyclaw_provider::LLMProvider;
use tyclaw_types::TyclawError;

/// 脚本化 Provider：`chat` 先 sleep `delay`，再返回一个 finish_reason=stop 的终止响应。
///
/// - `delay == 0`：立即返回（用于"快速完成"节点）。
/// - `delay` 较大：模拟长耗时节点，配合上层超时被终止（实际不会 sleep 满，因为超时会
///   取消该 future）。
struct ScriptedProvider {
    model: String,
    delay: Duration,
}

#[async_trait]
impl LLMProvider for ScriptedProvider {
    async fn chat(&self, _request: ChatRequest) -> Result<LLMResponse, TyclawError> {
        if !self.delay.is_zero() {
            tokio::time::sleep(self.delay).await;
        }
        Ok(LLMResponse {
            content: Some("done".to_string()),
            tool_calls: Vec::new(),
            finish_reason: "stop".to_string(),
            usage: HashMap::new(),
            reasoning_content: None,
        })
    }

    fn default_model(&self) -> &str {
        &self.model
    }
}

/// 初始化全局 prompt_store（mini AgentLoop 构建消息时需要）。
///
/// 集成测试以独立进程运行（独立 test binary），全局 `OnceLock` 互不干扰；
/// 这里写入一份最小但完整的 prompts.yaml 并 init（幂等，首次生效）。
fn ensure_prompts() {
    use std::io::Write as _;
    let tmp = tempfile::TempDir::new().unwrap();
    let cfg_dir = tmp.path().join("config");
    std::fs::create_dir_all(&cfg_dir).unwrap();
    let mut f = std::fs::File::create(cfg_dir.join("prompts.yaml")).unwrap();
    // build_messages → system_prompt_for_node_type("general") 需要：
    //   guidelines_default / subagent_execution_baseline / node_types.general
    // 以及 workspace_hint（user_context）。
    write!(
        f,
        "guidelines_default: |\n  Test sub-agent guidelines.\n\
         workspace_hint: |\n  Workspace: {{workspace}}, context: {{context_file}}\n\
         subagent_execution_baseline: |\n  Test execution baseline.\n\
         node_types:\n  general: |\n    You are a general-purpose sub-agent.\n"
    )
    .unwrap();
    prompt_loader::init(tmp.path());
}

/// 构造一个 NodeExecutor：把每个 model 名映射到对应的脚本化 provider。
fn make_executor(providers: HashMap<String, Arc<dyn LLMProvider>>, routing: RoutingPolicy) -> Arc<NodeExecutor> {
    // 持久的临时 workspace（NodeExecutor 会 canonicalize 并读取目录），
    // 用进程内唯一计数避免与并行测试冲突；测试进程退出后由 OS 清理临时目录。
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let workspace = std::env::temp_dir().join(format!(
        "tyclaw_subtask_timeout_ws_{}_{}",
        std::process::id(),
        n
    ));
    std::fs::create_dir_all(&workspace).unwrap();
    let app = AppContext::new(
        workspace,
        routing.default_model.clone(),
        false,
        0,
        Default::default(),
        Default::default(),
    );
    Arc::new(NodeExecutor::new(providers, routing, app))
}

fn node(id: &str, model_override: Option<&str>, timeout_ms: Option<u64>) -> TaskNode {
    TaskNode {
        id: id.to_string(),
        node_type: "general".to_string(),
        prompt: format!("task for {id}"),
        dependencies: Vec::new(),
        model_override: model_override.map(|s| s.to_string()),
        timeout_ms,
        max_retries: None,
        acceptance_criteria: None,
    }
}

/// R7.2：per-node 超时。
///
/// 单个 node 的 provider sleep 远超其 `timeout_ms`，调度器在 node 有效超时
/// (`min(timeout_ms=150, node_max_duration=300s) = 150ms`) 处终止该 node，
/// 产出 `Failed` + `error:"timeout"` 记录。
#[tokio::test]
async fn per_node_timeout_marks_failed_timeout() {
    ensure_prompts();

    let mut providers: HashMap<String, Arc<dyn LLMProvider>> = HashMap::new();
    providers.insert(
        "slow-model".to_string(),
        Arc::new(ScriptedProvider {
            model: "slow-model".to_string(),
            delay: Duration::from_secs(30),
        }),
    );
    let routing = RoutingPolicy {
        rules: Vec::new(),
        default_model: "slow-model".to_string(),
    };
    let executor = make_executor(providers, routing);

    // dispatch 整体超时给足（默认 600s 不触发），仅验证 per-node 超时。
    let scheduler = DagScheduler::new(executor, Some(2), Some(120_000));

    let plan = TaskPlan {
        id: "p-node-timeout".to_string(),
        nodes: vec![node("slow", None, Some(150))],
        edges: Vec::new(),
        failure_policy: FailurePolicy::BestEffort,
        metadata: PlanMetadata::default(),
    };

    let dispatch_dir = tempfile::TempDir::new().unwrap();
    let records = scheduler.execute(&plan, dispatch_dir.path(), None).await;

    assert_eq!(records.len(), 1, "expected exactly one record for the single node");
    let rec = &records[0];
    assert_eq!(rec.node_id, "slow");
    assert_eq!(
        rec.status,
        NodeStatus::Failed,
        "node exceeding its per-node timeout must be Failed"
    );
    assert_eq!(
        rec.error.as_deref(),
        Some("timeout"),
        "per-node timeout error must be \"timeout\" (R7.2)"
    );
}

/// R7.4：dispatch 整体超时返回部分结果。
///
/// 两个独立 node：`fast` 立即完成；`slow` 的 provider sleep 远超 dispatch 整体超时。
/// dispatch 在 1200ms 处整体超时，终止未完成的 `slow`，但保留已完成 `fast` 的真实结果，
/// 并为 `slow` 补 `Failed` + `error:"chain_timeout"` 记录。
#[tokio::test]
async fn dispatch_timeout_returns_partial_results() {
    ensure_prompts();

    let mut providers: HashMap<String, Arc<dyn LLMProvider>> = HashMap::new();
    providers.insert(
        "fast-model".to_string(),
        Arc::new(ScriptedProvider {
            model: "fast-model".to_string(),
            delay: Duration::ZERO,
        }),
    );
    providers.insert(
        "slow-model".to_string(),
        Arc::new(ScriptedProvider {
            model: "slow-model".to_string(),
            delay: Duration::from_secs(60),
        }),
    );
    let routing = RoutingPolicy {
        rules: Vec::new(),
        default_model: "fast-model".to_string(),
    };
    let executor = make_executor(providers, routing);

    // node 级超时给足（默认 120s/300s 不触发），dispatch 整体超时设为 1.2s。
    let scheduler = DagScheduler::new(executor, Some(4), Some(120_000))
        .with_chain_timeouts(None, Some(1_200));

    let plan = TaskPlan {
        id: "p-dispatch-timeout".to_string(),
        nodes: vec![
            node("fast", None, None),
            node("slow", Some("slow-model"), None),
        ],
        edges: Vec::new(),
        failure_policy: FailurePolicy::BestEffort,
        metadata: PlanMetadata::default(),
    };

    let dispatch_dir = tempfile::TempDir::new().unwrap();
    let records = scheduler.execute(&plan, dispatch_dir.path(), None).await;

    // 每个 node 恰好一条记录（已完成 + 超时终止）。
    assert_eq!(records.len(), 2, "expected one record per node");

    let fast = records
        .iter()
        .find(|r| r.node_id == "fast")
        .expect("fast node record present");
    let slow = records
        .iter()
        .find(|r| r.node_id == "slow")
        .expect("slow node record present");

    // 已完成 node 的部分结果被保留（R7.4）。
    assert_eq!(
        fast.status,
        NodeStatus::Success,
        "completed node result must be preserved as Success"
    );
    assert_ne!(
        fast.error.as_deref(),
        Some("chain_timeout"),
        "completed node must not be marked as chain_timeout"
    );

    // 未完成 node 因 dispatch 整体超时被终止（R7.4）。
    assert_eq!(
        slow.status,
        NodeStatus::Failed,
        "unfinished node must be Failed after dispatch timeout"
    );
    assert_eq!(
        slow.error.as_deref(),
        Some("chain_timeout"),
        "unfinished node error must be \"chain_timeout\" (R7.4)"
    );
}
