# LLM Provider 超时两级预警 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 为 TyClaw 增加不会阻塞 Agent 主链路的 LLM Provider 超时普通告警、最终耗尽严重告警和恢复通知，并通过钉钉主动消息发送给固定管理员。

**Architecture:** `tyclaw-provider` 只生成脱敏、非阻塞的结构化事件；`tyclaw-orchestration` 传播 Interactive/Timer/Subtask/Memory 调用来源；`tyclaw-app` 在独立 Tokio 任务中聚合窗口、维护状态和冷却；`tyclaw-channel` 提供不记录响应正文的安全管理员批量发送接口。队列满、配置错误、通知超时或消费者退出均不得改变 Provider 返回或阻止 TyClaw 主服务。

**Tech Stack:** Rust 2021、Tokio `mpsc`/`watch`/虚拟时间、Reqwest、Serde/YAML、Tracing、现有 DingTalk `TokenManager` 与 monitor HTTP 服务。

**Design spec:** `docs/superpowers/specs/2026-08-26-llm-timeout-alerting-design.md`

**Git note:** 当前仓库已有用户未跟踪文件。实施时只暂存本计划列出的文件；除非用户另行明确要求，不创建 commit。

---

## File map

- Create `crates/tyclaw-provider/src/events.rs`：Provider 事件、调用上下文、workload task-local 和无锁读取的非阻塞 sink。
- Modify `crates/tyclaw-provider/Cargo.toml`：复用 workspace `arc-swap`，避免事件热路径获取全局锁。
- Modify `crates/tyclaw-provider/src/lib.rs`、`provider.rs`、`openai_compat.rs`：导出 API，在顶层逻辑调用建立 `call_id`，在真实超时、恢复和最终耗尽处发事件。
- Modify `crates/tyclaw-orchestration/src/bus.rs`、`subtasks/scheduler.rs`：传播请求来源。
- Modify `crates/tyclaw-memory/src/llm_extractor.rs`、`memory_store.rs`：记忆调用局部标记为 Memory。
- Create `crates/tyclaw-channel/src/dingtalk/admin.rs`；modify `dingtalk/mod.rs`：安全批量管理员通知。
- Create `crates/tyclaw-app/src/llm_alerts.rs`；modify `main.rs`、`monitor.rs`、`Cargo.toml`：配置、状态机、运行器、装配和监控。
- Modify `workspace/config/config.example.yaml`：中文配置样例。

## Task 1: Provider 事件模型和失败开放 sink

**Files:**
- Create: `crates/tyclaw-provider/src/events.rs`
- Modify: `crates/tyclaw-provider/Cargo.toml`
- Modify: `crates/tyclaw-provider/src/lib.rs`
- Test: `crates/tyclaw-provider/src/events.rs`

- [ ] **Step 1: 写失败测试**

先测试 endpoint 只保留 origin、满/关闭通道不阻塞且增加丢弃计数：

```rust
#[test]
fn endpoint_keeps_only_origin() {
    assert_eq!(
        sanitize_endpoint("https://user:pass@example.com:8990/v1/chat?key=secret"),
        "https://example.com:8990"
    );
    assert_eq!(sanitize_endpoint("not a url"), "invalid-endpoint");
}

#[tokio::test]
async fn full_or_closed_sink_never_blocks_or_panics() {
    let (tx, mut rx) = tokio::sync::mpsc::channel(1);
    let dropped = Arc::new(AtomicU64::new(0));
    let sink = ProviderEventSink::new(tx, dropped.clone());
    let event = ProviderEvent::test_timeout(1);
    sink.try_emit(event.clone());
    sink.try_emit(event.clone());
    assert_eq!(dropped.load(Ordering::Relaxed), 1);
    assert_eq!(rx.recv().await.unwrap().call_id, 1);
    drop(rx);
    sink.try_emit(event);
    assert_eq!(dropped.load(Ordering::Relaxed), 2);
}
```

- [ ] **Step 2: 验证测试先失败**

Run: `cargo test -p tyclaw-provider events::tests -- --nocapture`

Expected: FAIL，缺少 `ProviderEventSink` 或 `sanitize_endpoint`。

- [ ] **Step 3: 实现最小事件 API**

定义下列类型；事件不得包含 Prompt、响应、用户 ID 或密钥：

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkloadKind { Interactive, Timer, Subtask, Memory, Unknown }
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransportKind { Sse, NonStream }
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderEventKind { SendTimeout, RecoveredAfterTimeout, RetryExhausted }

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderEvent {
    pub occurred_at: SystemTime,
    pub call_id: u64,
    pub kind: ProviderEventKind,
    pub transport: Option<TransportKind>,
    pub attempt: Option<usize>,
    pub model: String,
    pub provider_origin: String,
    pub workload: WorkloadKind,
}

tokio::task_local! {
    pub static CURRENT_WORKLOAD_KIND: WorkloadKind;
    pub(crate) static CURRENT_LLM_CALL: Arc<LlmCallContext>;
}
```

实现 `LlmCallContext { id, saw_timeout: AtomicBool }`、进程内 `AtomicU64` call ID、`OnceLock<arc_swap::ArcSwapOption<ProviderEventSink>>`，并在 `tyclaw-provider/Cargo.toml` 增加 `arc-swap = { workspace = true }`。sink 安装/卸载用 `store`，热路径用 `load_full`，不得获取全局 `Mutex`/`RwLock`。同时实现：

```rust
pub fn install_provider_event_sink(sink: Option<Arc<ProviderEventSink>>);
pub fn provider_event_dropped_count() -> u64;
pub(crate) fn next_call_context() -> Arc<LlmCallContext>;
pub(crate) fn emit_send_timeout(transport: TransportKind, attempt: usize, model: &str, endpoint: &str);
pub(crate) fn emit_call_outcome(kind: ProviderEventKind, model: &str, endpoint: &str);
pub fn sanitize_endpoint(endpoint: &str) -> String;
```

`ProviderEventSink::try_emit` 只能使用 `mpsc::Sender::try_send`；失败只增加共享 `AtomicU64`，不得日志输出事件内容。所有会安装全局 sink 的单元测试必须先持有测试模块内的静态 `std::sync::Mutex<()>` guard，并用 RAII guard 在测试结束（包括 panic unwind）时恢复 `None`，防止 Rust 并行测试互相替换 sink。

- [ ] **Step 4: 导出上层 API并验证**

在 `lib.rs` 导出事件类型、sink、计数和 `CURRENT_WORKLOAD_KIND`。

Run:

```bash
cargo test -p tyclaw-provider events::tests -- --nocapture
git diff --check -- crates/tyclaw-provider/src/events.rs crates/tyclaw-provider/src/lib.rs
```

Expected: PASS；diff check exit 0。

## Task 2: 在真实超时与逻辑结果处发事件

**Files:**
- Modify: `crates/tyclaw-provider/src/provider.rs`
- Modify: `crates/tyclaw-provider/src/openai_compat.rs`
- Test: 两文件现有测试模块

- [ ] **Step 1: 写 call ID、恢复和耗尽测试**

用 fake provider 覆盖两个场景：前两次 `send timeout` 后成功；四次外层调用全部超时。fake 的 `chat` 在返回 timeout error 前显式调用 crate 内部测试 helper 标记当前调用发生了超时并发出合成 `SendTimeout`，否则 fake 不会经过 OpenAI HTTP 超时分支。所有测试先持有全局 sink 测试锁并安装 RAII reset guard。断言：

```rust
#[tokio::test(start_paused = true)]
async fn timeout_then_success_emits_one_recovery_for_one_call() {
    let response = CURRENT_WORKLOAD_KIND
        .scope(WorkloadKind::Interactive,
            RecoveringProvider::new().chat_with_retry(vec![], None, None, None))
        .await;
    assert_ne!(response.finish_reason, "error");
    let events = drain_events();
    let recovered: Vec<_> = events.iter()
        .filter(|e| e.kind == ProviderEventKind::RecoveredAfterTimeout).collect();
    assert_eq!(recovered.len(), 1);
    assert!(events.iter().all(|e| e.call_id == recovered[0].call_id));
}

#[tokio::test(start_paused = true)]
async fn exhausted_call_emits_once_and_keeps_retry_later_response() {
    // RetryExhausted 恰好一次；响应仍等于 RETRY_LATER_MESSAGE。
}
```

低层测试用本地 `TcpListener` 接受连接但不返回响应，分别覆盖 SSE/NonStream transport、attempt、model 和脱敏 origin，不访问真实网络。

- [ ] **Step 2: 验证测试先失败**

Run: 分别运行上述两个完整测试名。

Expected: FAIL，尚未产生生命周期事件。

- [ ] **Step 3: 贯穿顶层 `call_id`**

在 `chat_with_retry` 进入时创建一次 context，用 `CURRENT_LLM_CALL.scope(ctx, async { 原主体 })` 包住现有实现。不要新增会迫使所有 `LLMProvider` 实现改写的方法；原重试次数、许可释放、退避和返回值保持不变。

成功返回前若 `saw_timeout` 为真，发送一次 `RecoveredAfterTimeout`；最终临时错误返回 `retry_exhausted_response()` 前发送一次 `RetryExhausted`。非临时错误、空响应和并发排队超时不纳入第一版。

- [ ] **Step 4: 在两个真实超时分支发事件**

在 `openai_compat.rs` 的 `SSE send timeout` 和 `Non-stream send timeout` WARN 前分别调用：

```rust
emit_send_timeout(
    TransportKind::Sse, // 另一处为 NonStream
    attempt,
    request.model.as_deref().unwrap_or(self.default_model.as_str()),
    &self.api_base,
);
```

不得在 fresh connection、fallback 或外层 transient retry 处重复发送。

- [ ] **Step 5: 验证 sink 故障不改变结果**

对同一 fake provider 在 sink disabled、正常、关闭、满载四种条件运行，断言完整 `LLMResponse` 相等；成功恢复和最终耗尽各一组。

Run:

```bash
cargo test -p tyclaw-provider provider_result_is_identical_with_disabled_full_or_closed_alert_sink -- --nocapture
cargo test -p tyclaw-provider
```

Expected: PASS，既有 retry-later、SSE retry 和并发测试不变。

## Task 3: 传播调用来源

**Files:**
- Modify: `crates/tyclaw-orchestration/src/bus.rs`
- Modify: `crates/tyclaw-orchestration/src/subtasks/scheduler.rs`
- Modify: `crates/tyclaw-memory/src/llm_extractor.rs`
- Modify: `crates/tyclaw-memory/src/memory_store.rs`

- [ ] **Step 1: 写消息来源映射测试**

```rust
fn provider_workload(is_timer: bool) -> WorkloadKind {
    if is_timer { WorkloadKind::Timer } else { WorkloadKind::Interactive }
}

#[test]
fn timer_and_interactive_messages_map_to_provider_workloads() {
    assert_eq!(provider_workload(true), WorkloadKind::Timer);
    assert_eq!(provider_workload(false), WorkloadKind::Interactive);
}
```

Run: `cargo test -p tyclaw-orchestration timer_and_interactive_messages_map_to_provider_workloads`

Expected: FAIL，映射函数尚不存在。

- [ ] **Step 2: 包裹整次消息处理**

在 `MessageBus::run` 构造 `run_future` 后加入：

```rust
let workload = provider_workload(msg.is_timer);
let run_future = tyclaw_provider::CURRENT_WORKLOAD_KIND.scope(workload, run_future);
```

保留 Timer job、heartbeat 等既有 scope 行为。

- [ ] **Step 3: spawn 后显式重建 Subtask scope**

在 scheduler 的 `JoinSet::spawn` 内，与 user ID、sandbox 一样显式包裹：

```rust
let with_workload = CURRENT_WORKLOAD_KIND.scope(WorkloadKind::Subtask, inner);
```

测试 spawned node 内读到 `Subtask`，而非父级 `Interactive` 或 `Unknown`。

- [ ] **Step 4: 局部覆盖 Memory 调用**

将 `crates/tyclaw-memory/src` 中每个 `chat_with_retry` 包进：

```rust
CURRENT_WORKLOAD_KIND
    .scope(WorkloadKind::Memory, provider.chat_with_retry(messages, tools, model, None))
    .await
```

不得把 handler 整体标成 Memory，否则用户主 Agent 会误分类。

- [ ] **Step 5: 验证覆盖**

Run:

```bash
cargo test -p tyclaw-memory
cargo test -p tyclaw-orchestration provider_workload
rg -n 'chat_with_retry' crates/tyclaw-memory/src
```

Expected: 测试 PASS；每个 memory 调用均位于 Memory scope。

## Task 4: 配置与纯状态机

**Files:**
- Create: `crates/tyclaw-app/src/llm_alerts.rs`
- Modify: `crates/tyclaw-app/src/main.rs`（仅声明模块）
- Test: `crates/tyclaw-app/src/llm_alerts.rs`

- [ ] **Step 1: 写默认值、非法配置和 Debug 脱敏测试**

```rust
#[test]
fn defaults_are_disabled_and_match_approved_thresholds() {
    let cfg: LlmAlertsConfig = serde_yaml::from_str("{}").unwrap();
    assert!(!cfg.enabled);
    assert_eq!(cfg.warning.window_secs, 900);
    assert_eq!(cfg.warning.min_send_timeouts, 15);
    assert_eq!(cfg.critical.background_exhausted, 2);
}

#[test]
fn invalid_config_does_not_expose_admin_ids() {
    let cfg: LlmAlertsConfig = serde_yaml::from_str("enabled: true\nwarning: { window_secs: 0 }\nnotification: { admin_user_ids: [secret-admin-id] }").unwrap();
    assert!(cfg.validate().is_err());
    assert!(!format!("{cfg:?}").contains("secret-admin-id"));
}
```

- [ ] **Step 2: 写状态边界测试**

覆盖并明确断言：14次无告警、第15次 Warning；2小时冷却；Interactive 第1次耗尽立即 Critical；后台60分钟第2次耗尽 Critical；同一 `call_id` 去重；Warning 可立即升级；29:59不恢复、30:00且超时不超过2次且无耗尽才 Recovery；Critical 不降级到 Warning。

Run: `cargo test -p tyclaw-app llm_alerts::tests -- --nocapture`

Expected: FAIL，缺少配置和状态机类型。

- [ ] **Step 3: 实现配置类型**

字段严格对应设计：`LlmAlertsConfig`、`WarningConfig`、`CriticalConfig`、`RecoveryConfig`、`NotificationConfig`。管理员列表 trim、拒绝空值、精确去重并检查 `max_recipients`；手写 Debug 只显示 `admin_count`。校验错误只含配置键和固定错误码。

- [ ] **Step 4: 实现纯状态机**

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AlertLevel { Healthy, Warning, Critical }

pub(crate) enum AlertAction {
    NotifyWarning(AlertReport),
    NotifyCritical(AlertReport),
    NotifyRecovery(AlertReport),
}

impl AlertStateMachine {
    pub fn on_event(&mut self, now: Instant, event: ProviderEvent) -> Vec<AlertAction>;
    pub fn on_tick(&mut self, now: Instant) -> Vec<AlertAction>;
    pub fn snapshot(&self) -> AlertSnapshot;
}
```

用 `VecDeque` 保存15/30/60分钟窗口，`HashMap<u64, CallDisposition>` 去重。每次事件/tick先淘汰；call 状态在最长窗口后5分钟淘汰并设硬上限，超限增加 `state_evictions`。

- [ ] **Step 5: 添加历史基线合成回放**

```rust
#[test]
fn historical_baseline_only_warns_on_strong_windows() {
    let peaks = [(12, false), (13, false), (12, false), (6, false),
                 (13, false), (18, true), (29, true)];
    for (count, should_warn) in peaks {
        assert_eq!(replay_window(count).has_warning(), should_warn);
    }
}
```

不得把生产日志提交为 fixture。

- [ ] **Step 6: 运行状态机测试**

Run: `cargo test -p tyclaw-app llm_alerts::tests -- --nocapture`

Expected: PASS；不依赖 wall clock 或网络。

## Task 5: 安全钉钉管理员批量发送

**Files:**
- Create: `crates/tyclaw-channel/src/dingtalk/admin.rs`
- Modify: `crates/tyclaw-channel/src/dingtalk/mod.rs`
- Test: `crates/tyclaw-channel/src/dingtalk/admin.rs`

- [ ] **Step 1: 写单批请求和错误脱敏测试**

使用本地 `TcpListener` mock，断言两个管理员只产生一次 `batchSend`，payload 的 `userIds` 同时包含两者。mock 500 body 写入伪造管理员 ID/token，捕获返回和 tracing 日志并断言均不包含伪敏感值。

Run: `cargo test -p tyclaw-channel dingtalk::admin::tests -- --nocapture`

Expected: FAIL，发送器不存在。

- [ ] **Step 2: 实现安全接口**

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdminSendErrorKind { Unauthorized, RemoteStatus, Transport, InvalidInput }

#[derive(Debug)]
pub struct AdminSendError { kind: AdminSendErrorKind }

pub struct AdminMarkdownSender { client: reqwest::Client, endpoint: String }

impl AdminMarkdownSender {
    pub fn new() -> Self;
    pub async fn send(
        &self, token: &str, robot_code: &str, admin_user_ids: &[String],
        title: &str, text: &str, timeout: Duration,
    ) -> Result<(), AdminSendError>;
}
```

使用一次 `oToMessages/batchSend`、`sampleMarkdown`。非成功响应只读取并丢弃 body；日志仅含 status 和固定 error kind。401 映射 Unauthorized，由 app reset token 后重试一次。

- [ ] **Step 3: 导出并验证**

在 `dingtalk/mod.rs` 导出新类型。

Run: `cargo test -p tyclaw-channel dingtalk::admin::tests -- --nocapture`

Expected: PASS，日志不含 fake token、管理员 ID 或响应正文。

## Task 6: 告警运行器与通知适配器

**Files:**
- Modify: `crates/tyclaw-app/src/llm_alerts.rs`
- Modify: `crates/tyclaw-app/Cargo.toml`
- Test: `crates/tyclaw-app/src/llm_alerts.rs`

- [ ] **Step 1: 写通知超时和非法配置失败开放测试**

引入：

```rust
#[async_trait::async_trait]
trait AlertNotifier: Send + Sync {
    async fn notify(&self, action: &AlertAction) -> Result<(), NotificationError>;
}
```

在 `tyclaw-app/Cargo.toml` 增加 `async-trait = { workspace = true }` 和 `parking_lot = { workspace = true }`。snapshot 统一使用 `Arc<parking_lot::RwLock<AlertSnapshot>>`，monitor 和 runner 不得混用不同锁类型。

用 `HangingNotifier` 配合暂停时间断言：通知超过 `timeout_secs` 后 snapshot 为 Failed，但 manager task 仍运行。非法配置返回 disabled/config_invalid runtime，不向 main 返回 fatal error。

- [ ] **Step 2: 实现 runner 和 snapshot**

`run_alert_manager` 用 `tokio::select!` 消费 receiver 和30秒 interval tick。每个 action 的通知调用包裹 timeout；失败只更新 snapshot 和 WARN。receiver 关闭时设置 `event_source_closed` 并正常退出。

Snapshot 至少包括：level、available/reason、窗口超时/耗尽数、dropped events、state evictions、notification status、最近通知时间和固定错误类别。

- [ ] **Step 3: 实现 DingTalk notifier**

持有 `AdminMarkdownSender`、现有 `TokenManager`、robot code、管理员 ID 和 timeout。获取 token后批量发送；Unauthorized 时 reset token 并只重试一次。`format_alert` 为纯函数，只使用聚合统计和实例标识。

缺少凭证、robot code 或管理员 ID 时返回 notification_unavailable，不创建循环失败任务。

- [ ] **Step 4: 验证运行器**

Run: `cargo test -p tyclaw-app llm_alerts::tests -- --nocapture`

Expected: PASS；HangingNotifier 通过虚拟时间完成。

## Task 7: 应用配置、启动与关闭装配

**Files:**
- Modify: `crates/tyclaw-app/src/main.rs`
- Modify: `crates/tyclaw-app/src/llm_alerts.rs`
- Test: `crates/tyclaw-app/src/main.rs`

- [ ] **Step 1: 写 AppConfig 和摘要脱敏测试**

解析含两个伪管理员的 YAML；断言长度为2，启动摘要含 `admin_count: 2` 且不含 ID 原值。

- [ ] **Step 2: 增加 AppConfig 字段**

```rust
#[derive(Debug, Default, Deserialize)]
struct AppConfig {
    dingtalk: DingTalkConfig,
    monitor: MonitorConfig,
    analytics: tyclaw_control::AnalyticsConfig,
    privacy: PrivacyConfig,
    #[serde(default)]
    llm_alerts: llm_alerts::LlmAlertsConfig,
}
```

`format_effective_config` 仅输出阈值、channel 和管理员数量。

- [ ] **Step 3: Provider 创建前准备通道**

实现 `prepare_alert_runtime`：合法且 enabled 时创建容量1024的有界通道并安装 sink；关闭时安装 None；非法时 snapshot=config_invalid、安装 None，main 继续。

合法配置但无钉钉凭证时仍采集聚合，snapshot=notification_unavailable。

- [ ] **Step 4: 装配 CLI 和 hybrid 生命周期**

`RunConfig` 携带 alert snapshot/receiver/config。启动顺序：sink → Provider/Orchestrator → manager → monitor/bus/channel。退出顺序：卸载 sink、drop sender、最多等待1秒，超时 abort manager。不得延长现有退出超过1秒。

- [ ] **Step 5: 验证三种非致命配置**

覆盖 enabled=false、非法阈值、合法但无钉钉凭证，断言均不使启动准备返回 fatal error。

Run: `cargo test -p tyclaw-app config_tests llm_alerts::tests -- --nocapture`；若 Cargo 过滤限制，分别运行。

Expected: PASS。

## Task 8: Monitor 与配置样例

**Files:**
- Modify: `crates/tyclaw-app/src/monitor.rs`
- Modify: `workspace/config/config.example.yaml`
- Test: `crates/tyclaw-app/src/monitor.rs`

- [ ] **Step 1: 写 monitor 脱敏测试**

抽出 `alert_snapshot_json`，断言 Warning、15次超时、通知状态等字段存在，序列化结果不含管理员 ID或完整 endpoint。

- [ ] **Step 2: 接入 `/api/stats` 和页面**

`MonitorOptions` 接收 `Arc<RwLock<AlertSnapshot>>`。JSON 新增 `llm_alerts`：level、available/reason、窗口计数、dropped、notification status、last error kind。概览增加“LLM告警”卡片；异常或通知不可用复用现有 alert 样式。

- [ ] **Step 3: 更新配置样例**

加入完整注释配置：warning 900/15/7200；critical 1、2/3600/7200；recovery 1800/2；notification dingtalk、`YOUR_DINGTALK_ADMIN_USER_ID`、20、15。注明真实 config 和 userId 不得提交或输出日志。

- [ ] **Step 4: 验证 monitor/config**

Run:

```bash
cargo test -p tyclaw-app monitor::tests -- --nocapture
cargo test -p tyclaw-app config_tests -- --nocapture
```

Expected: PASS。

## Task 9: 集成回归和性能护栏

**Files:**
- Modify only when failures expose defects in files listed above.
- Do not touch `workspace/config/config.yaml`、生产日志、`workspace/work/`、`target/` 或其他用户文件。

- [ ] **Step 1: 添加满队列性能护栏**

安装容量1且永不消费的 sink，投递10,000个事件。用 `tokio::time::timeout(Duration::from_millis(200), ...)` 证明不会等待容量，断言 dropped=9,999；不要使用更脆弱的微秒基准。

- [ ] **Step 2: 再次验证 Agent 结果等价**

对成功恢复、最终耗尽两类 fake provider，在 disabled/正常/关闭/满 sink 下分别断言完整 `LLMResponse` 相同。

- [ ] **Step 3: 运行格式和受影响 crate 测试**

Run:

```bash
cargo fmt --all -- --check
cargo test -p tyclaw-provider
cargo test -p tyclaw-memory
cargo test -p tyclaw-orchestration
cargo test -p tyclaw-channel
cargo test -p tyclaw-app
```

Expected: 全部 exit 0。若需 `cargo fmt --all`，先确认不会格式化无关用户修改。

- [ ] **Step 4: 运行 workspace 检查**

Run:

```bash
cargo check --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

Expected: exit 0。既有无关 warning 记录文件/行号，不修改无关代码；修复本改动引入的所有 warning。

- [ ] **Step 5: 最终安全与差异检查**

Run:

```bash
git diff --check
git status --short
git diff -- crates/tyclaw-provider crates/tyclaw-memory crates/tyclaw-orchestration crates/tyclaw-channel crates/tyclaw-app workspace/config/config.example.yaml docs/superpowers/specs/2026-08-26-llm-timeout-alerting-design.md docs/superpowers/plans/2026-08-26-llm-timeout-alerting.md
rg -n 'admin_user_ids|access_token|api_key|response body' crates/tyclaw-app/src/llm_alerts.rs crates/tyclaw-channel/src/dingtalk/admin.rs
```

Expected: diff check exit 0；其他既有 `??` 原样保留；diff 不含真实管理员 ID、token、API key、Prompt 或生产日志。搜索命中只允许字段定义、脱敏测试和禁止记录的注释。

- [ ] **Step 6: 记录外部验证边界**

不发送真实钉钉消息，除非用户另行明确授权。最终报告必须说明：已用本地 mock 验证 payload、401刷新、500和超时；未执行真实送达；未启动/停止生产 TyClaw、Docker 或外部 Provider；列出实际修改文件和全部验证结果。
