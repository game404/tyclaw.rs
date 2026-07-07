use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{Mutex, Semaphore};
use tokio::task::JoinSet;
use tracing::{info, warn};

use super::executor::NodeExecutor;
use super::protocol::{ExecutionRecord, FailurePolicy, NodeStatus, TaskNode, TaskPlan};

/// 默认并发上限。
const DEFAULT_MAX_CONCURRENCY: usize = 4;
/// 默认节点超时（ms）。
const DEFAULT_TIMEOUT_MS: u64 = 120_000;
/// 默认单 Node 最大执行时间（ms）= 5 分钟（R7.1）。
const DEFAULT_NODE_MAX_DURATION_MS: u64 = 300_000;
/// 默认 dispatch 整体最大执行时间（ms）= 10 分钟（R7.3）。
const DEFAULT_DISPATCH_MAX_DURATION_MS: u64 = 600_000;

/// DAG 调度器：按拓扑序调度 ready 节点并行执行。
pub struct DagScheduler {
    executor: Arc<NodeExecutor>,
    max_concurrency: usize,
    default_timeout_ms: u64,
    /// 单 Node 有效超时上限（ms）：node 有效超时 = min(node.timeout_ms.unwrap_or(default), node_max_duration_ms)。
    node_max_duration_ms: u64,
    /// dispatch 整体超时（ms）：超过则终止未完成 node 并返回部分结果（R7.4/R7.5）。
    dispatch_max_duration_ms: u64,
}

impl DagScheduler {
    pub fn new(
        executor: Arc<NodeExecutor>,
        max_concurrency: Option<usize>,
        default_timeout_ms: Option<u64>,
    ) -> Self {
        Self {
            executor,
            max_concurrency: max_concurrency.unwrap_or(DEFAULT_MAX_CONCURRENCY),
            default_timeout_ms: default_timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS),
            node_max_duration_ms: DEFAULT_NODE_MAX_DURATION_MS,
            dispatch_max_duration_ms: DEFAULT_DISPATCH_MAX_DURATION_MS,
        }
    }

    /// 配置子任务链超时上限（ms）：单 Node 最大执行时间与 dispatch 整体最大执行时间。
    ///
    /// `None` 时保持默认（node 300s / dispatch 600s）。返回 `self` 便于链式构造，
    /// 不改变 `new` 的签名以兼容既有调用点。
    pub fn with_chain_timeouts(
        mut self,
        node_max_duration_ms: Option<u64>,
        dispatch_max_duration_ms: Option<u64>,
    ) -> Self {
        if let Some(ms) = node_max_duration_ms {
            self.node_max_duration_ms = ms;
        }
        if let Some(ms) = dispatch_max_duration_ms {
            self.dispatch_max_duration_ms = ms;
        }
        self
    }

    /// 获取底层 NodeExecutor 的引用（用于单任务短路优化）。
    pub fn executor(&self) -> &NodeExecutor {
        &self.executor
    }

    /// 执行整个 TaskPlan，返回所有节点的 ExecutionRecord。
    pub async fn execute(
        &self,
        plan: &TaskPlan,
        dispatch_dir: &std::path::Path,
        main_context: Option<&str>,
    ) -> Vec<ExecutionRecord> {
        let node_map: HashMap<&str, &TaskNode> =
            plan.nodes.iter().map(|n| (n.id.as_str(), n)).collect();

        // 构建依赖图：node_id → 上游依赖集合
        let mut deps: HashMap<String, HashSet<String>> = HashMap::new();
        // 构建下游图：node_id → 依赖它的节点
        let mut downstream: HashMap<String, Vec<String>> = HashMap::new();

        for node in &plan.nodes {
            deps.entry(node.id.clone()).or_default();
            downstream.entry(node.id.clone()).or_default();
        }
        for (from, to) in &plan.edges {
            deps.entry(to.clone()).or_default().insert(from.clone());
            downstream.entry(from.clone()).or_default().push(to.clone());
        }
        for node in &plan.nodes {
            for dep in &node.dependencies {
                deps.entry(node.id.clone()).or_default().insert(dep.clone());
                downstream
                    .entry(dep.clone())
                    .or_default()
                    .push(node.id.clone());
            }
        }

        let records: Arc<Mutex<Vec<ExecutionRecord>>> = Arc::new(Mutex::new(Vec::new()));
        let outputs: Arc<Mutex<HashMap<String, String>>> = Arc::new(Mutex::new(HashMap::new()));
        let statuses: Arc<Mutex<HashMap<String, NodeStatus>>> = Arc::new(Mutex::new(
            plan.nodes
                .iter()
                .map(|n| (n.id.clone(), NodeStatus::Pending))
                .collect(),
        ));

        let semaphore = Arc::new(Semaphore::new(self.max_concurrency));

        // 维护待调度节点
        let pending: Arc<Mutex<HashSet<String>>> = Arc::new(Mutex::new(
            plan.nodes.iter().map(|n| n.id.clone()).collect(),
        ));

        // 整个调度循环（含收割与循环依赖兜底）包在 dispatch 整体超时内。
        // 超时则取消所有在途任务（终止未完成 node），随后由 classify 标注 chain_timeout。
        //
        // join_set 提升到超时块之外：超时取消 `scheduling` 后（future 被 drop，仅释放对
        // join_set 的可变借用，不会 abort 任务——JoinSet 本身在此作用域存活），可再对
        // join_set 做一次非阻塞收割，把「已成功但尚未被收割」的 node 纳入结果，避免被
        // classify 误判为 chain_timeout（R7.4/R7.5）。
        let dispatch_timeout = Duration::from_millis(self.dispatch_max_duration_ms);
        let mut join_set: JoinSet<(String, ExecutionRecord)> = JoinSet::new();
        let scheduling = async {
            loop {
            // 检查是否全部完成
            {
                let p = pending.lock().await;
                let st = statuses.lock().await;
                let active = st.values().any(|s| *s == NodeStatus::Running);
                if p.is_empty() && !active {
                    break;
                }
            }

            // 找出 ready 节点（注意：所有 Mutex 必须在同一个 block 内获取和释放，避免死锁）
            let (ready_nodes, skipped_nodes) = {
                let mut p = pending.lock().await;
                let st = statuses.lock().await;

                // FailFast：任一已失败/受阻 → 标记剩余为 Skipped
                if plan.failure_policy == FailurePolicy::FailFast
                    && st
                        .values()
                        .any(|s| *s == NodeStatus::Failed || *s == NodeStatus::Blocked)
                {
                    let skipped: Vec<String> = p.drain().collect();
                    drop(st);
                    drop(p);
                    let mut st = statuses.lock().await;
                    let mut rec = records.lock().await;
                    for id in skipped {
                        st.insert(id.clone(), NodeStatus::Skipped);
                        rec.push(ExecutionRecord {
                            node_id: id,
                            model: String::new(),
                            input_tokens: 0,
                            output_tokens: 0,
                            duration_ms: 0,
                            status: NodeStatus::Skipped,
                            output: None,
                            error: Some("skipped due to fail-fast".into()),
                            retries: 0,
                            messages: None,
                            tools_used: Vec::new(),
                            tool_events: Vec::new(),
                            decision_events: Vec::new(),
                            diagnostics_summary: None,
                            skills_used: Vec::new(),
                        });
                    }
                    break;
                }

                let mut ready = Vec::new();
                let mut to_remove = Vec::new();
                let mut skipped = Vec::new();

                for id in p.iter() {
                    let node_deps = deps.get(id.as_str()).cloned().unwrap_or_default();
                    let all_deps_done = node_deps
                        .iter()
                        .all(|d| matches!(st.get(d.as_str()), Some(NodeStatus::Success)));
                    let any_dep_failed = node_deps.iter().any(|d| {
                        matches!(
                            st.get(d.as_str()),
                            Some(NodeStatus::Failed)
                                | Some(NodeStatus::Blocked)
                                | Some(NodeStatus::Skipped)
                        )
                    });

                    if any_dep_failed && plan.failure_policy == FailurePolicy::BestEffort {
                        to_remove.push(id.clone());
                        skipped.push(id.clone());
                        continue;
                    }

                    if all_deps_done {
                        if let Some(node) = node_map.get(id.as_str()) {
                            ready.push((*node).clone());
                            to_remove.push(id.clone());
                        }
                    }
                }

                for id in &to_remove {
                    p.remove(id);
                }

                // 释放 st 和 p 锁后再处理 skipped
                (ready, skipped)
            };

            // 在锁释放后记录跳过的节点
            if !skipped_nodes.is_empty() {
                let mut st = statuses.lock().await;
                let mut rec = records.lock().await;
                for id in &skipped_nodes {
                    st.insert(id.clone(), NodeStatus::Skipped);
                    rec.push(ExecutionRecord {
                        node_id: id.clone(),
                        model: String::new(),
                        input_tokens: 0,
                        output_tokens: 0,
                        duration_ms: 0,
                        status: NodeStatus::Skipped,
                        output: None,
                        error: Some("skipped: upstream dependency failed".into()),
                        retries: 0,
                        messages: None,
                        tools_used: Vec::new(),
                        tool_events: Vec::new(),
                        decision_events: Vec::new(),
                        diagnostics_summary: None,
                        skills_used: Vec::new(),
                    });
                }
            }

            if ready_nodes.is_empty() {
                // 等待某个正在运行的任务完成
                if let Some(result) = join_set.join_next().await {
                    if let Ok((node_id, record)) = result {
                        let mut st = statuses.lock().await;
                        st.insert(node_id.clone(), record.status);
                        if record.status == NodeStatus::Success {
                            if let Some(ref out) = record.output {
                                outputs.lock().await.insert(node_id.clone(), out.clone());
                            }
                        }
                        records.lock().await.push(record);
                    }
                } else {
                    // JoinSet 为空且无 ready 节点 → 可能是死锁或全部完成
                    break;
                }
                continue;
            }

            // 为 ready 节点启动并发执行
            tracing::debug!(count = ready_nodes.len(), "Scheduler spawning ready nodes");
            for node in ready_nodes {
                let executor = Arc::clone(&self.executor);
                let sem = Arc::clone(&semaphore);
                let outputs_ref = Arc::clone(&outputs);
                let statuses_ref = Arc::clone(&statuses);
                // node 有效超时 = min(node.timeout_ms.unwrap_or(default), node_max_duration)（R7.2）。
                let effective_timeout_ms = node
                    .timeout_ms
                    .unwrap_or(self.default_timeout_ms)
                    .min(self.node_max_duration_ms);
                let timeout = Duration::from_millis(effective_timeout_ms);
                let node_id = node.id.clone();
                let node_deps = node.dependencies.clone();

                {
                    let mut st = statuses_ref.lock().await;
                    st.insert(node_id.clone(), NodeStatus::Running);
                }

                let dispatch_dir_owned = dispatch_dir.to_path_buf();
                let main_ctx_owned = main_context.map(|s| s.to_string());
                // 捕获当前 sandbox scope（task_local 不跨 spawn 传递，需要手动传）
                let sandbox_for_spawn = tyclaw_sandbox::current_sandbox();
                // 同理捕获 user_id（provider task_local 不跨 spawn）：子代理的 LLM 调用据此
                // 向并发控制器申请 per-user 许可，否则会全部落到 `_anonymous` 桶（R6.2/R6.5）。
                let user_id_for_spawn =
                    tyclaw_provider::CURRENT_USER_ID.try_with(|u| u.clone()).ok();
                tracing::debug!(node_id = %node_id, "Scheduler spawning task");
                join_set.spawn(async move {
                    // 在 spawned task 中重建 sandbox scope
                    let inner = async {
                    tracing::debug!(node_id = %node_id, "Task started, acquiring semaphore");
                    let _permit = sem.acquire().await.expect("semaphore closed");
                    tracing::debug!(node_id = %node_id, "Semaphore acquired, executing");

                    // 收集上游输出
                    let upstream: Vec<(String, String)> = {
                        let out = outputs_ref.lock().await;
                        node_deps
                            .iter()
                            .filter_map(|d| out.get(d).map(|o| (d.clone(), o.clone())))
                            .collect()
                    };

                    let record = match tokio::time::timeout(timeout, executor.execute(&node, &upstream, &dispatch_dir_owned, main_ctx_owned.as_deref())).await {
                        Ok(rec) => rec,
                        Err(_) => {
                            warn!(node_id = %node.id, timeout_ms = timeout.as_millis(), "Node timed out");
                            ExecutionRecord {
                                node_id: node.id.clone(),
                                model: String::new(),
                                input_tokens: 0,
                                output_tokens: 0,
                                duration_ms: timeout.as_millis() as u64,
                                status: NodeStatus::Failed,
                                output: None,
                                error: Some("timeout".into()),
                                retries: 0,
                                messages: None,
                                tools_used: Vec::new(),
                                tool_events: Vec::new(),
                                decision_events: Vec::new(),
                                diagnostics_summary: None,
                            skills_used: Vec::new(),
                            }
                        }
                    };

                    (node_id, record)
                    }; // end inner async
                    // 在 spawned task 中重建 user_id scope（同 sandbox，task_local 不跨 spawn）。
                    let with_user = async move {
                        match user_id_for_spawn {
                            Some(uid) => tyclaw_provider::CURRENT_USER_ID.scope(uid, inner).await,
                            None => inner.await,
                        }
                    };
                    if let Some(sb) = sandbox_for_spawn {
                        tyclaw_sandbox::CURRENT_SANDBOX.scope(sb, with_user).await
                    } else {
                        with_user.await
                    }
                });
            }

            // 收割已完成的任务
            while let Some(result) = join_set.try_join_next() {
                if let Ok((node_id, record)) = result {
                    let mut st = statuses.lock().await;
                    st.insert(node_id.clone(), record.status);
                    if record.status == NodeStatus::Success {
                        if let Some(ref out) = record.output {
                            outputs.lock().await.insert(node_id.clone(), out.clone());
                        }
                    }
                    records.lock().await.push(record);
                }
            }
        }

        // 收割所有剩余任务
        while let Some(result) = join_set.join_next().await {
            if let Ok((node_id, record)) = result {
                let mut st = statuses.lock().await;
                st.insert(node_id.clone(), record.status);
                if record.status == NodeStatus::Success {
                    if let Some(ref out) = record.output {
                        outputs.lock().await.insert(node_id.clone(), out.clone());
                    }
                }
                records.lock().await.push(record);
            }
        }

        // 检查是否所有节点都已完成。如果有未完成的节点，说明可能存在循环依赖。
        {
            let st = statuses.lock().await;
            let p = pending.lock().await;
            let incomplete: Vec<String> = st
                .iter()
                .filter(|(_, s)| matches!(s, NodeStatus::Pending | NodeStatus::Running))
                .map(|(id, _)| id.clone())
                .collect();
            if !incomplete.is_empty() {
                warn!(
                    nodes = ?incomplete,
                    "DAG scheduling ended with incomplete nodes — possible circular dependency"
                );
                // 将未完成的节点标记为失败
                drop(st);
                drop(p);
                let mut st = statuses.lock().await;
                let mut rec = records.lock().await;
                for id in &incomplete {
                    st.insert(id.clone(), NodeStatus::Failed);
                    rec.push(ExecutionRecord {
                        node_id: id.clone(),
                        model: String::new(),
                        input_tokens: 0,
                        output_tokens: 0,
                        duration_ms: 0,
                        status: NodeStatus::Failed,
                        output: None,
                        error: Some(
                            "node not completed: possible circular dependency in DAG".into(),
                        ),
                        retries: 0,
                        messages: None,
                        tools_used: Vec::new(),
                        tool_events: Vec::new(),
                        decision_events: Vec::new(),
                        diagnostics_summary: None,
                        skills_used: Vec::new(),
                    });
                }
            }
        }
        }; // end scheduling async block

        // 应用 dispatch 整体超时：超时则取消在途任务（终止未完成 node）。
        let timed_out = tokio::time::timeout(dispatch_timeout, scheduling)
            .await
            .is_err();
        if timed_out {
            warn!(
                dispatch_max_duration_ms = self.dispatch_max_duration_ms,
                "dispatch overall timeout reached — terminating unfinished nodes (chain_timeout)"
            );
            // 尽力收割：调度被取消时，可能已有任务完成但结果还留在 join_set 未被收割。
            // 非阻塞地取出这些真实结果并入 records，使其不被 classify 误判为 chain_timeout。
            // 仍在运行的任务不会在此完成，随 join_set 在函数结束时 drop 而被 abort。
            while let Some(result) = join_set.try_join_next() {
                if let Ok((node_id, record)) = result {
                    let mut st = statuses.lock().await;
                    st.insert(node_id.clone(), record.status);
                    if record.status == NodeStatus::Success {
                        if let Some(ref out) = record.output {
                            outputs.lock().await.insert(node_id.clone(), out.clone());
                        }
                    }
                    records.lock().await.push(record);
                }
            }
        }

        // 汇总已完成记录，对未完成 node 补 chain_timeout 失败记录（R7.4/R7.5）。
        let completed = { records.lock().await.clone() };
        let all_node_ids: Vec<String> = plan.nodes.iter().map(|n| n.id.clone()).collect();
        let result = classify_dispatch_nodes(&all_node_ids, completed, timed_out);
        info!(total_nodes = result.len(), "DAG execution completed");
        result
    }
}

/// 构造一条 `chain_timeout` 失败记录（dispatch 整体超时终止的未完成 node）。
fn make_chain_timeout_record(node_id: &str) -> ExecutionRecord {
    ExecutionRecord {
        node_id: node_id.to_string(),
        model: String::new(),
        input_tokens: 0,
        output_tokens: 0,
        duration_ms: 0,
        status: NodeStatus::Failed,
        output: None,
        error: Some("chain_timeout".into()),
        retries: 0,
        messages: None,
        tools_used: Vec::new(),
        tool_events: Vec::new(),
        decision_events: Vec::new(),
        diagnostics_summary: None,
        skills_used: Vec::new(),
    }
}

/// 纯函数：根据完整 node id 集合、已完成记录集与 dispatch 超时标志，
/// 产出覆盖每个 node 的最终分类记录列表（R7.4/R7.5）。
///
/// 分类规则：
/// - 已完成 node（出现在 `completed` 中）：保留其真实 `ExecutionRecord`（含真实状态）。
/// - 未完成 node（不在 `completed` 中）且 `timed_out == true`：判为「因超时被终止」，
///   补一条 `NodeStatus::Failed + error:"chain_timeout"` 记录。
/// - `timed_out == false`：调度正常结束，所有 node 均应已在 `completed` 中，原样返回。
///
/// 该函数不依赖真实异步时序，便于属性测试确定性地验证「每个 node 被正确分类为
/// 已完成 vs 超时终止」。
pub(crate) fn classify_dispatch_nodes(
    all_node_ids: &[String],
    completed: Vec<ExecutionRecord>,
    timed_out: bool,
) -> Vec<ExecutionRecord> {
    let mut result = completed;
    if !timed_out {
        return result;
    }
    let done: HashSet<&str> = result.iter().map(|r| r.node_id.as_str()).collect();
    let missing: Vec<String> = all_node_ids
        .iter()
        .filter(|id| !done.contains(id.as_str()))
        .cloned()
        .collect();
    for id in missing {
        result.push(make_chain_timeout_record(&id));
    }
    result
}

#[cfg(test)]
mod scheduler_timeout_classification_tests {
    use super::{classify_dispatch_nodes, ExecutionRecord, NodeStatus};
    use proptest::prelude::*;

    /// 构造一条最小 ExecutionRecord（仅设置 node_id 与 status，其余取零/空默认）。
    fn make_record(node_id: &str, status: NodeStatus) -> ExecutionRecord {
        ExecutionRecord {
            node_id: node_id.to_string(),
            model: String::new(),
            input_tokens: 0,
            output_tokens: 0,
            duration_ms: 0,
            status,
            output: None,
            error: None,
            retries: 0,
            messages: None,
            tools_used: Vec::new(),
            tool_events: Vec::new(),
            decision_events: Vec::new(),
            diagnostics_summary: None,
            skills_used: Vec::new(),
        }
    }

    /// 生成 (节点总数 n, 每个节点是否已完成的布尔向量)。
    /// n ∈ [1, 10]，flags[i] == true 表示节点 `n{i}` 已完成（Success）。
    fn nodes_and_completion() -> impl Strategy<Value = (usize, Vec<bool>)> {
        (1usize..=10).prop_flat_map(|n| {
            proptest::collection::vec(any::<bool>(), n).prop_map(move |flags| (n, flags))
        })
    }

    proptest! {
        #![proptest_config(ProptestConfig { cases: 100, ..Default::default() })]

        // Feature: execution-performance-optimization, Property 23: 子任务超时结果正确分类每个节点
        #[test]
        fn prop_timeout_classifies_each_node((n, flags) in nodes_and_completion()) {
            let all_ids: Vec<String> = (0..n).map(|i| format!("n{i}")).collect();

            // 已完成节点构造 Success 记录。
            let completed: Vec<ExecutionRecord> = all_ids
                .iter()
                .zip(flags.iter())
                .filter(|(_, &done)| done)
                .map(|(id, _)| make_record(id, NodeStatus::Success))
                .collect();

            // ── dispatch 超时分支：每个节点都应被正确分类 ──
            let result = classify_dispatch_nodes(&all_ids, completed.clone(), true);

            // 结果恰好覆盖全部 node id，每个出现且仅出现一次。
            let mut result_ids: Vec<String> =
                result.iter().map(|r| r.node_id.clone()).collect();
            result_ids.sort();
            let mut expected_ids = all_ids.clone();
            expected_ids.sort();
            prop_assert_eq!(&result_ids, &expected_ids);
            prop_assert_eq!(result.len(), all_ids.len());

            // 按节点核对状态：已完成保留 Success；未完成判为 Failed + chain_timeout。
            for (id, &done) in all_ids.iter().zip(flags.iter()) {
                let rec = result
                    .iter()
                    .find(|r| &r.node_id == id)
                    .expect("each node id must be present in result");
                if done {
                    prop_assert_eq!(rec.status, NodeStatus::Success);
                } else {
                    prop_assert_eq!(rec.status, NodeStatus::Failed);
                    prop_assert_eq!(rec.error.as_deref(), Some("chain_timeout"));
                }
            }

            // ── 非超时分支：原样返回，不增不改 ──
            let unchanged = classify_dispatch_nodes(&all_ids, completed.clone(), false);
            prop_assert_eq!(unchanged.len(), completed.len());
            for (got, exp) in unchanged.iter().zip(completed.iter()) {
                prop_assert_eq!(&got.node_id, &exp.node_id);
                prop_assert_eq!(got.status, exp.status);
                prop_assert_eq!(&got.error, &exp.error);
            }
        }
    }
}
