# LLM Provider 超时两级预警设计

## 1. 背景与目标

TyClaw 当前会在 `tyclaw-provider` 内记录 SSE、非流式请求超时、重试恢复和最终重试耗尽，但只有离散日志，没有跨请求聚合、告警状态和主动通知。一次逻辑 LLM 调用还可能经历 SSE 内层重试、重新建连、非流式降级和 Provider 外层重试，直接统计 WARN 行会严重放大故障次数。

本设计增加进程内两级预警：

- 普通告警：识别一段时间内大量底层发送超时，反映 Provider 链路明显退化。
- 严重告警：识别逻辑 LLM 调用最终耗尽，并按交互请求和后台任务采用不同灵敏度。
- 恢复通知：异常状态持续一段安静窗口后只发送一次恢复消息。

目标是及时发现真实故障，同时避免单次网络抖动和嵌套重试造成频繁告警。告警功能不得改变 LLM 重试、用户回复或业务任务执行结果；采集、聚合或通知失败只能记录脱敏日志。

## 2. 历史基线与初始化阈值

初始化配置基于 `/Users/tu/Downloads/duoduo-log/` 中 2026-08-19、20、22 至 26 日日志。部分日期不是全天，8 月 25 日包含重叠文件；分析时按完整事件行去重。

历史上每 15 分钟的底层发送超时峰值分别为 12、13、12、6、13、18、29。将普通告警阈值设为 15 后，只会命中 8 月 25 日和 26 日的两个最强异常窗口，不会命中其余日期的常见抖动。历史共有 8 次超时型最终耗尽，均与 Memory consolidation 相邻；因此后台耗尽不适合逐次立即通知。

初始化配置：

```yaml
llm_alerts:
  enabled: true

  warning:
    window_secs: 900
    min_send_timeouts: 15
    cooldown_secs: 7200

  critical:
    interactive_exhausted: 1
    background_exhausted: 2
    background_window_secs: 3600
    cooldown_secs: 7200

  recovery:
    quiet_window_secs: 1800
    max_send_timeouts: 2
    notify: true

  notification:
    channel: dingtalk
    admin_user_ids:
      - "YOUR_DINGTALK_ADMIN_USER_ID"
    max_recipients: 20
    timeout_secs: 15
```

阈值全部可配置，但第一版不增加按模型、Provider 或 workspace 的独立阈值。运行一周后依据实际触发次数、交互请求耗尽数和恢复耗时再调整。

## 3. 方案选择

采用“Provider 结构化事件 + 应用层进程内聚合 + 钉钉主动通知”：

1. `tyclaw-provider` 在真实重试分支发出结构化事件。
2. `tyclaw-app` 启动有界事件通道和单独的告警聚合任务。
3. 聚合任务维护滑动窗口、状态、冷却和恢复条件。
4. 通知适配器复用现有钉钉 token 与主动发送 API，将消息发送给配置白名单中的管理员。

不采用日志扫描作为正式方案，因为日志轮转、重复文件、多行 Prompt 和文案变化容易造成误计。不在第一版接入 Prometheus/Alertmanager，因为当前项目没有相应基础设施，部署成本高于本需求。结构化事件保留未来导出指标的可能性。

## 4. 架构与职责边界

### 4.1 Provider 事件定义

`tyclaw-provider` 定义与通道无关的事件类型和非阻塞事件出口。Provider 不依赖 `tyclaw-app`、`tyclaw-channel`、`tyclaw-tools` 或 `tyclaw-control`，也不直接发送通知。

事件至少包含：

- `occurred_at`：事件时间。
- `call_id`：一次顶层 `chat_with_retry` 逻辑调用的稳定标识，内层重试和外层重试共享。
- `event_kind`：`SendTimeout`、`RecoveredAfterTimeout`、`RetryExhausted`。
- `transport`：`Sse` 或 `NonStream`。仅对相关事件提供。
- `attempt`：当前传输层尝试序号。仅对相关事件提供。
- `model`：实际请求模型。
- `provider_endpoint`：只保留 scheme、host、port，不包含 path query、凭证或请求体。
- `workload_kind`：`Interactive`、`Timer`、`Subtask`、`Memory`、`Unknown`。

不在事件中包含 Prompt、响应正文、API key、用户姓名、staff ID、workspace 路径或业务数据。

`SendTimeout` 在每次真实 SSE 60 秒或非流式 120 秒发送超时时产生，用于普通告警的历史兼容口径。`RetryExhausted` 只在 `chat_with_retry` 的最终临时性错误分支产生，同一次 `call_id` 最多一次。若调用曾经超时但最终成功，则产生一次 `RecoveredAfterTimeout`。

`call_id` 在每次进入顶层 `chat_with_retry` 时生成一次，并通过 Provider 内部 task-local 调用上下文包裹该方法发起的每一次 `self.chat`。`OpenAICompatProvider::chat_stream`、SSE 重新建连、非流式降级和 Provider 外层重试只能读取该 ID，不能重新生成。ID 使用进程内不透明递增值或等价的低成本标识，不进入用户响应，也不要求跨进程稳定。测试及少数直接调用 `chat` 而绕过 `chat_with_retry` 的路径允许使用一次性兜底 ID，但必须保证事件仍可去重。

### 4.2 调用来源传播

在 Provider 现有 task-local 请求上下文旁增加 `workload_kind`，由 orchestration 在进入不同工作负载时设置：

- 钉钉、CLI 用户消息：`Interactive`。
- Timer 触发任务：`Timer`。
- 多模型子任务：`Subtask`。
- 记忆抽取、归档和 consolidation：`Memory`。

未显式设置时为 `Unknown`。`Unknown` 在严重告警策略中按后台任务计数，同时在告警正文标出未知来源，并产生独立 WARN，便于补齐遗漏的调用点。这样不会因新增后台调用方而立刻制造告警风暴，也不会静默丢失事件。

来源注入遵循“最外层设默认、专用调用局部覆盖”：消息总线在执行整次请求前根据 `is_timer` 注入 `Timer` 或 `Interactive`；subtask scheduler 创建 Tokio task 时与现有 user ID、sandbox 一样显式重建 `Subtask` scope；所有记忆抽取、归档和 consolidation 的 LLM 调用在调用点局部覆盖为 `Memory`。task-local 不会自动跨 `tokio::spawn` 传播，因此新增 spawn 点必须显式选择来源，不能静默继承为 `Unknown`。

### 4.3 告警聚合器

告警聚合器属于应用运行时基础设施，由 `tyclaw-app` 创建和持有。它通过有界 `tokio::mpsc` 接收 Provider 事件，单任务串行更新状态，不在 Provider 请求路径执行网络 I/O。

内部维护：

- 最近 15 分钟的 `SendTimeout` 时间队列。
- 最近 60 分钟的后台 `RetryExhausted` 时间队列。
- 当前告警状态：`Healthy`、`Warning`、`Critical`。
- 各级别最近通知时间。
- 最近一次发送超时、最终耗尽和告警状态变化时间。
- 每个活动 `call_id` 的最小状态，用于保证恢复和耗尽事件幂等。

时间窗口使用单调时钟判断，消息展示时间使用配置时区。队列定期淘汰过期元素并限制容量；达到容量上限时优先保留最新事件并记录一次聚合器健康告警日志。

聚合任务除消费事件外，还需至少每 30 秒执行一次状态评估 tick。这样即使异常结束后没有任何新 Provider 事件，也能在安静窗口到期时触发恢复。tick 间隔作为实现常量，不开放为配置。

### 4.4 通知适配器

通知由 `tyclaw-app` 中独立适配器执行，复用现有钉钉应用凭证、token 缓存和主动发送 API，但不通过 Agent Tool 调用，避免告警依赖 LLM 本身。

管理员钉钉 userId 由应用级 `llm_alerts.notification.admin_user_ids` 显式配置。不能复用现有 Agent Tool 从“当前用户 workspace 的 `.config/ty.config.toml`”读取收件人的方式，因为系统级告警不属于某个用户 workspace，绑定任意当前 workspace 会造成漏报、串用或 workspace 被回收后无法通知。真实 `config.yaml` 本身已被视为敏感配置，不得提交；配置样例只使用虚构占位符。

启动时对 `admin_user_ids` 执行去除首尾空白、拒绝空值和精确去重，收件人数不得超过 `max_recipients`。当 `llm_alerts.enabled=true` 且通知渠道为钉钉时，管理员列表必须至少包含一个 ID，否则告警模块进入 `notification_unavailable`，但不阻止 LLM 主服务启动。管理员 ID 不允许由 Provider 事件、用户消息、Prompt、Agent Tool 参数、环境中的当前用户或任意 workspace 配置覆盖。

通知适配器必须复用或下沉现有消息大小限制、HTTP 超时、token 刷新和响应脱敏逻辑，不复制一套弱化实现。

现有部分钉钉通用发送函数会在失败时记录原始响应正文，不能直接供系统告警使用。实现应在通道层提供面向固定 `admin_user_ids` 的批量 Markdown 发送接口：只返回结构化、脱敏的成功/失败类别，日志不得包含请求 payload、管理员 ID、access token 或远端响应正文。告警模块不得逐个管理员调用单发函数，以免扩大请求数和产生部分发送时的重复告警。

通知发送失败不反向阻塞 Provider，也不递归产生 LLM 告警。失败以结构化 WARN 记录，包含告警级别和失败类别，不包含 access token、收件人 ID 或钉钉响应正文中的敏感字段。

## 5. 判定与状态机

### 5.1 普通告警

任意滑动 15 分钟内累计至少 15 次 `SendTimeout`，触发普通告警并进入 `Warning`。计数采用底层真实发送超时，与历史回放口径一致；不把“重新建连”“降级到非流式”或外层重试日志再次计数。

同一轮持续异常中，普通告警 2 小时内不重复发送。冷却结束后，若条件仍满足，可以发送一条“异常持续”普通告警。

### 5.2 严重告警

满足任一条件即进入 `Critical`：

- `Interactive` 调用出现 1 次 `RetryExhausted`，立即发送严重告警。
- `Timer`、`Subtask`、`Memory` 或 `Unknown` 在滑动 60 分钟内累计 2 次 `RetryExhausted`，发送严重告警。

从 `Warning` 升级到 `Critical` 时立即通知，不受普通告警冷却影响。严重告警自身 2 小时内不重复发送；冷却结束后仍有新的满足条件事件时，可以发送“严重异常持续”通知。

严重告警依据 `call_id` 去重。同一个调用即使产生多条低层超时和外层重试日志，也只算一次最终耗尽。

### 5.3 恢复通知

系统处于 `Warning` 或 `Critical` 后，连续观察 30 分钟；如果该滑动窗口内底层发送超时不超过 2 次，且窗口内没有新的 `RetryExhausted`，则回到 `Healthy` 并发送一条恢复通知。

`Critical` 不因严重条件的计数自然滑出窗口而自动降级到 `Warning`；只有满足上述统一恢复条件后才直接回到 `Healthy`。如果恢复前普通或严重条件再次满足，只更新本轮异常统计，并按各自冷却规则决定是否发送持续异常通知。

恢复通知每轮异常只发送一次。新的异常在恢复后可以重新触发告警，不受上一轮告警冷却影响。若 `recovery.notify=false`，仍更新内部状态但不发送恢复消息。

### 5.4 功能关闭和配置异常

- `enabled=false` 时不启动聚合与通知，但现有 Provider 日志保持不变。
- 阈值为 0、窗口为 0、恢复上限大于普通告警阈值等无效组合在启动时拒绝启用告警模块，错误信息指出具体配置键；TyClaw 主服务继续启动，monitor 显示 `config_invalid`。
- 钉钉凭证缺失或 `admin_user_ids` 为空、无效、超过上限时，LLM 服务仍可启动；告警模块进入 `notification_unavailable` 状态并明确记录错误。监控页需要展示该状态，避免“没有通知”被误认为“没有故障”。

## 6. 通知内容

普通告警示例字段：

- 标题：`[TyClaw][LLM告警] Provider 超时升高`。
- 告警开始时间和统计窗口。
- 15 分钟发送超时数，按 SSE/非流式拆分。
- 受影响模型和脱敏 Provider host。
- 已恢复逻辑调用数、仍在重试调用数（可获得时）。
- 当前 in-flight 数和并发上限。
- 告警实例标识。

严重告警额外包含：

- 最终耗尽次数。
- 调用来源分布，不包含用户身份。
- 是否由普通告警升级。
- 建议检查的网关时段和组件。

恢复通知包含异常开始/结束时间、持续时长、期间峰值和最终耗尽总数。所有通知禁止附带 Prompt、响应内容、API key 前缀、staff ID、原始 endpoint query 或业务数据。

## 7. 可观测性与监控页

告警聚合器暴露只读快照供现有 monitor 使用：

- 当前状态和状态开始时间。
- 当前窗口发送超时数。
- 当前窗口最终耗尽数。
- 最近一次通知时间和结果。
- Provider 事件通道丢弃数。
- 通知能力是否可用及最近错误类别。

第一版不改变现有 analytics 数据库 schema；告警事件先通过结构化日志和 monitor 快照观察。待运行数据稳定后，再决定是否持久化日级 Provider 指标。进程重启会清空滑动窗口和冷却状态，这是第一版接受的限制；启动后只统计新事件，不扫描旧日志补发告警。

## 8. 并发、背压和故障隔离

- Provider 使用无锁读取的可替换 sink 和非阻塞 `try_send` 发事件；告警消费者变慢时不得拖慢 LLM 请求。不得为每次事件获取全局 `Mutex`/`RwLock`。
- 通道满时累加丢弃计数，并用限频 WARN 暴露，不逐条打印。
- 通知 HTTP 请求使用独立超时，不能占用 LLM 并发许可。
- 聚合任务取消时停止通知并安全退出，不影响 orchestrator 关闭。
- 钉钉通知失败不做无限重试；仅允许一次 token 刷新后的重试，之后等待下一次状态变化或持续异常提醒。
- 多实例部署时每个实例独立告警，通知中必须包含实例标识。第一版不做跨实例聚合。

## 9. 配置与兼容性

新增配置结构同步到：

- `tyclaw-app` 的配置反序列化与启动校验。
- `workspace/config/config.example.yaml` 的中文示例。
- 启动配置摘要，且只输出启用状态、阈值和管理员数量，不输出凭证、管理员姓名或 userId。配置结构的 `Debug` 实现也不得展示 `admin_user_ids` 原值。

缺少 `llm_alerts` 配置时默认关闭，保证历史配置和 CLI 模式兼容。配置优先级继续遵循命令行参数、环境变量、配置文件和默认值的现有约定；第一版不增加对应命令行参数或环境变量，避免扩大范围。

## 10. 测试与验证

### 10.1 Provider 单元测试

- SSE/非流式发送超时产生正确事件，且无敏感字段。
- 同一 `chat_with_retry` 的所有事件共享 `call_id`。
- 超时后恢复只产生一次 `RecoveredAfterTimeout`。
- 最终临时性错误只产生一次 `RetryExhausted`。
- 非临时性错误、空响应和并发排队超时按明确约定处理，不误计为发送超时；其中并发排队超时可作为后续独立告警类型，不纳入第一版阈值。
- 事件通道满不会阻塞或改变 LLM 返回。

### 10.2 聚合器确定性测试

使用暂停的 Tokio 时间或注入时钟验证：

- 15 分钟第 14 次不告警，第 15 次告警。
- 窗口边界事件正确淘汰。
- 普通告警 2 小时冷却和持续异常提醒。
- 交互耗尽 1 次立即严重告警。
- 后台耗尽 60 分钟内第 2 次严重告警，窗口外不累计。
- Warning 到 Critical 的升级绕过普通冷却。
- 30 分钟安静窗口、最多 2 次发送超时且无耗尽时恢复。
- 同一 `call_id` 重复事件不重复计数。
- Unknown 来源按后台策略处理并留下可观测信号。

### 10.3 通知和配置测试

- 使用本地 mock HTTP 服务验证 payload、token 刷新、超时和错误脱敏，不发送真实钉钉消息。
- mock 返回包含伪造敏感 userId 和 token 的错误正文时，接口返回值及捕获日志均不包含这些值。
- 管理员 userId 只能来自应用级 `llm_alerts.notification.admin_user_ids`，不能由事件、模型输出、Tool 参数或用户 workspace 覆盖。
- 管理员列表执行去空、去重和数量上限校验；测试错误和 Debug 输出不包含 userId 原值。
- 缺少通知配置时 LLM 主流程正常，监控快照显示通知不可用。
- 默认关闭时保持现有行为。
- 配置反序列化、默认值和非法组合校验。

### 10.4 历史回放验收

提供离线测试 fixture 或回放工具，将脱敏后的结构化事件输入聚合器。以本次历史样本为验收基线：

- 普通告警命中 8 月 25 日和 26 日的强异常窗口，不命中其余样本日期。
- 后台严重告警只在窗口内达到两次耗尽时触发。
- 不读取或提交原始生产日志作为测试 fixture。

实现完成后至少运行：

```bash
cargo fmt --all -- --check
cargo test -p tyclaw-provider
cargo test -p tyclaw-app
cargo check --workspace
git diff --check
```

## 11. 非目标

第一版不包含：

- 自动切换备用 Provider、动态修改超时或并发参数。
- 根据 Prompt、用户或业务内容决定告警级别。
- 跨实例统一窗口、分布式去重和告警持久化。
- Prometheus、Alertmanager、短信或电话通知。
- 对历史日志持续扫描或补发告警。
- 为每个模型、workspace 或 Provider 单独配置阈值。

## 12. 完成标准

- Provider 能非阻塞地产生无敏感信息的结构化超时生命周期事件。
- 普通、严重和恢复判定满足本设计阈值、去重、冷却与升级规则。
- 前台和后台最终耗尽按不同策略处理，未知来源可观测。
- 钉钉通知不依赖 LLM，管理员 userId 来自应用级固定配置且全程脱敏，失败不影响业务请求。
- monitor 能显示告警状态、事件丢弃和通知可用性。
- 配置样例、针对性测试、workspace 检查和 diff 检查通过；未运行的外部通知测试明确说明。
