# Requirements Document

## Introduction

本特性旨在系统性解决 tyclaw.rs（基于 Rust 的钉钉 AI Agent 服务，采用 ReAct 循环 + 多 LLM Provider）在线上运行中暴露的「执行缓慢」问题。需求基于 `log/` 目录下 2026-05-11 至 2026-06-01 的线上运行日志，以及 `report/` 目录下的多份分析报告综合提炼而成。

分析显示，执行缓慢并非单一原因，而是由若干相互关联的根因叠加放大造成。本文档按子系统/关注点将其拆分为可独立验证的需求，并标注优先级（P0/P1/P2），供后续设计与实现阶段排期。

证据来源：
- `report/analysis-report-0520-0525.md`（0520–0525 四日合并分析）
- `report/analysis-report-0526-0601.md`（0526–0601 周度分析）
- `report/incident-cannot-make-progress-0519.md`（「I cannot make progress」级联事件深挖）
- `report/tyclaw_slow_skill_analysis_20260511.md`（0504–0509 慢 Skill 分析）
- `docs/react_analysis_report.md`（ReAct 架构与已落地优化）

代码现状校准（用于澄清报告与实现的差异）：
- 工具输出截断常量当前为 `crates/tyclaw-tools/src/shell.rs` 中的 `MAX_OUTPUT_CHARS = 20_000`（报告引用的 3500 为更早期基线，已部分调整，但仍需进一步评估）。
- `read_file` 上限为 `crates/tyclaw-tools/src/filesystem.rs` 中的 `MAX_READ_CHARS = 128_000`。
- 子任务超时默认值为 `crates/tyclaw-orchestration/src/subtasks/config.rs` 中的 `default_timeout_ms = 120_000`（120s），调度层已有 `NodeStatus::Failed + error: "timeout"` 分支。
- 速率限制器（`crates/tyclaw-control/src/rate_limiter.rs`）当前为滑动窗口「请求频率」限制，尚无「并发数（in-flight）」限制与排队机制。
- 历史污染剔除：`fresh user turn` reset 当前仅重置迭代计数，`crates/tyclaw-orchestration/src/history.rs` 与 `memory_filter.rs` 尚无基于失败关键词的污染剔除逻辑。

> 说明：本需求文档聚焦「定义系统应达到的可验证行为」，不锁定具体实现方案；阈值类参数（如截断字符数、并发上限）以可配置项形式给出，便于线上调参。

## Glossary

- **System（系统）**：tyclaw.rs 服务整体。
- **Orchestrator（编排器）**：`tyclaw-orchestration` 中负责请求全生命周期的组件（限流、角色、会话、技能、案例、运行时、审计、记忆合并）。
- **Agent_Loop（Agent 循环）**：`tyclaw-agent` 中实现 ReAct 迭代状态机的组件（`AgentLoop`）。
- **Session（会话）**：某一 workspace 维度上累积的消息历史。
- **Workspace（工作区）**：以 `workspace_key` 标识的会话上下文边界（可为个人或群聊）。
- **Session_Manager（会话管理器）**：`tyclaw-orchestration/src/session_manager.rs`，管理会话历史的加载、保存与重置。
- **History_Processor（历史处理器）**：`tyclaw-orchestration/src/history.rs`，负责历史去重、token 预算裁剪、tool_call 配对修复。
- **Memory_Consolidator（记忆合并器）**：`tyclaw-memory/src/consolidator.rs`，按 token 预算将历史合并入记忆。
- **Subtask_Dispatcher（子任务调度器）**：`tyclaw-orchestration/src/subtasks/` 下的 planner/scheduler/executor，对应 `dispatch_subtasks` 工具。
- **Node（子任务节点）**：子任务计划中的单个执行单元，含 `NodeStatus`（Success/Failed/Skipped/Timeout 等）。
- **Tool_Output_Limiter（工具输出限制器）**：工具结果按字符数截断的逻辑（如 `shell.rs` 的 `truncate_by_chars`）。
- **Concurrency_Controller（并发控制器）**：限制同时进行中（in-flight）的 LLM 调用/请求数量并支持排队的组件（待新增）。
- **Rate_Limiter（速率限制器）**：`tyclaw-control/src/rate_limiter.rs`，基于滑动窗口的请求频率限制器。
- **SSE_Stream（SSE 流）**：`tyclaw-provider`/`tyclaw-channel` 中与上游 LLM 之间的流式响应通道。
- **Sandbox（沙盒）**：执行用户脚本的隔离环境（Docker），见 `docker/sandbox/Dockerfile`。
- **Skill（技能）**：可被路由调用的能力单元（脚本 + 元数据）。
- **Pollution_Message（污染消息）**：历史中包含失败兜底语义（如 `I cannot make progress`、`error`、`blocked`）但被当作有效结果保留，从而诱导 LLM 复读的 tool/assistant 消息。
- **Pollution_Keyword（污染关键词）**：用于识别污染消息的关键词集合（可配置）。
- **Reset_Marker（重置标记）**：新用户回合（fresh user turn）注入的标记，当前仅重置迭代计数器。
- **Dedup_Count（去重次数）**：`Deduplicating tool_call id on save` 在单位时间内出现的次数。
- **Pairs_Fixes（配对修复数）**：`ensure_tool_call_pairs` 单次运行修复的 tool_call/tool_result 配对数量。
- **Empty_Result_Task（空结果任务）**：查询类任务首次数据源调用返回 0 行有效数据的任务。

---

## Requirements

### Requirement 1: 会话历史污染识别与剔除（P0）

**User Story:** 作为运维与终端用户，我希望在新的用户回合开始时清除上一轮残留的失败兜底消息，以便 LLM 不再复读「I cannot make progress」之类的失败文案造成无效请求和缓慢。

> 证据：`incident-cannot-make-progress-0519.md` §3.3 —— `fresh user turn` 仅重置计数器、未剔除污染历史，导致 11 分钟内连续 7 次复读同一失败文案。

#### Acceptance Criteria

1. WHEN 一个新的用户回合（fresh user turn）开始，THE Orchestrator SHALL 在构建本回合 prompt 前扫描该 Workspace 历史中 role 为 tool 的消息，并将满足污染候选判定规则的 tool 消息标记为 Pollution_Message。
2. WHEN 历史消息被标记为 Pollution_Message，THE History_Processor SHALL 在送入 Agent_Loop 的消息序列中移除该 Pollution_Message，同时保持剩余 tool_call 与 tool_result 的配对完整性。
3. THE System SHALL 从可配置项读取 Pollution_Keyword 集合，且默认集合包含 `I cannot make progress`、`error`、`blocked` 三个关键词。
4. IF 移除 Pollution_Message 会导致某个 assistant 的 tool_call 失去对应 tool_result，THEN THE History_Processor SHALL 为该 tool_call 补齐占位 tool_result（其内容须包含可识别的「已因污染剔除」标识）而非保留 Pollution_Message。
5. WHEN 本回合完成 Pollution_Message 剔除，THE Orchestrator SHALL 在审计日志中记录被剔除的消息数量与该 Workspace 标识，且当被剔除数量为 0 时仍记录该条审计（数量记为 0）。
6. THE History_Processor SHALL 保留所有未被标记为 Pollution_Message 的历史消息，使其内容与顺序原样不变。
7. THE Orchestrator SHALL 仅在某消息同时满足以下全部条件时将其判定为污染候选：role 为 tool；字符长度不超过可配置的污染候选短消息上限（默认 512 字符）；以不区分大小写的完整短语（而非子串）方式匹配到 Pollution_Keyword 集合中的至少一个关键词。

---

### Requirement 2: 子任务状态与正文语义一致性（P0）

**User Story:** 作为系统维护者，我希望子任务的状态码与其正文语义保持一致，以便「认输的子任务」不会以「成功」身份污染主会话历史。

> 证据：`incident-cannot-make-progress-0519.md` §3.2 —— `fix_brief_window` 吐出失败兜底文案却被标记 `status=ok`，成为污染源。

#### Acceptance Criteria

1. IF 一个 Node 的输出正文包含 Pollution_Keyword（关键词匹配不区分大小写），THEN THE Subtask_Dispatcher SHALL 将该 Node 的状态判定为 `Failed` 或 `Blocked`，且不得判定为 `Success`。
2. WHEN 一个 Node 因命中 Agent_Loop 的 `max_iterations` 上限而结束且未产出有效结果（即输出正文为空白，或输出正文包含 Pollution_Keyword），THE Subtask_Dispatcher SHALL 将该 Node 状态判定为 `Failed`。
3. WHEN `dispatch_subtasks` 向主会话写回工具结果，THE Subtask_Dispatcher SHALL 使写回结果中的声明状态（`declared_result_status`）与 Node 实际状态（`NodeStatus`）满足以下语义映射：`NodeStatus::Success` 对应成功语义；`NodeStatus::Failed`、`NodeStatus::Blocked`、`NodeStatus::Skipped` 对应失败/受阻语义。
4. IF 某个 Node 被判定为 `Failed` 或 `Blocked`，THEN THE Subtask_Dispatcher SHALL 在工具结果中以独立于正文文本的专用字段附带失败原因，且不得仅返回失败兜底文案的纯文本。
5. IF 某个 Node 被判定为 `Failed` 或 `Blocked`，THEN THE Subtask_Dispatcher SHALL 始终附带失败原因，无论系统是否已通过其他途径提供错误上下文。
6. IF 某个 Node 的实际状态（`NodeStatus`）为 `Failed`、`Blocked` 或 `Skipped`，THEN THE Subtask_Dispatcher SHALL 禁止将写回结果的声明状态（`declared_result_status`）设为成功语义值（如 `ok` 或 `success`）。

---

### Requirement 3: 会话规模上限与强制重置/滚动截断（P0）

**User Story:** 作为系统维护者，我希望对单个 Workspace 的会话规模设置硬性上限并在超限时强制截断或重置，以便阻断 tool_call 去重与配对修复进入指数级恶化区间。

> 证据：`analysis-report-0520-0525.md` §四.1/§四.2 —— 0521 单日去重 11,377 次（基线 9.7 倍）、`ensure_pairs` max fixes 108、单次记忆合并 1,115 条消息；当单用户日请求量超过 ~100 时进入指数级恶化。

#### Acceptance Criteria

1. THE System SHALL 提供可配置的会话消息数硬上限（默认值为 500 条）。
2. WHEN 某 Workspace 的会话消息数超过会话消息数硬上限，THE Session_Manager SHALL 先触发强制记忆合并，按时间顺序将最早的消息合并入记忆。
3. IF 强制记忆合并完成后该 Workspace 的会话消息数仍超过会话消息数硬上限，THEN THE Session_Manager SHALL 对该会话执行滚动截断，保留最近消息使会话消息数回落至可配置的滚动截断目标水位（默认值为会话消息数硬上限的 80%，即 400 条）以内。
4. WHEN 单次运行的 Pairs_Fixes 超过可配置的告警阈值（默认 30），THE System SHALL 输出 WARN 级别日志并记录该 Workspace 标识。
5. WHEN 单次运行的 Pairs_Fixes 超过可配置的强制重置阈值（默认 60），THE Session_Manager SHALL 对该 Workspace 会话执行强制重置，使活动会话仅保留最近一个完整用户回合的消息并将其余消息合并入记忆。
6. WHEN 会话被执行滚动截断或强制重置，THE History_Processor SHALL 保持保留下来的消息序列中 tool_call 与 tool_result 的配对完整性，且保留序列中不得存在缺失对应 tool_call 的孤立 tool_result。
7. IF 滚动截断或强制重置的边界落在一对未完成的 tool_call/tool_result 之间（即保留窗口最早一条消息为缺失对应 tool_call 的孤立 tool_result），THEN THE History_Processor SHALL 将截断边界向更早方向调整至最近一个完整用户回合的起点，使保留序列不以孤立 tool_result 开始。
8. WHEN 会话被执行滚动截断、强制记忆合并或强制重置，THE Orchestrator SHALL 在审计日志中记录触发原因（消息数超限 / Pairs_Fixes 超限）与 Workspace 标识。

---

### Requirement 4: 记忆合并体量上限（P0）

**User Story:** 作为系统维护者，我希望限制单次记忆合并处理的消息体量，以便避免巨量 token 与时间开销拖慢请求。

> 证据：`analysis-report-0520-0525.md` §四.2 —— 0521 单次合并 1,115 条消息；`analysis-report-0526-0601.md` §八 —— 0527 当日合并 6,170 条、单次峰值 753 条。

#### Acceptance Criteria

1. THE System SHALL 提供可配置的单次记忆合并最大消息数上限（默认值为 500 条）。
2. WHEN 待合并的消息数超过单次记忆合并最大消息数上限，THE Memory_Consolidator SHALL 将合并任务分片为多个批次依次处理，且在不拆分单个用户回合（user turn）的前提下使每个批次的消息数不超过该上限。
3. WHILE 记忆合并正在进行，THE Memory_Consolidator SHALL 在单次合并调用内最多处理 5 个分片批次（沿用现有「最多 5 轮」约束）以防止无限合并。
4. WHEN 一次记忆合并完成，THE Memory_Consolidator SHALL 在日志中记录本次处理的消息数与分片批次数。
5. IF 单次合并调用在达到 5 个分片批次上限后仍存在未合并的消息，THEN THE Memory_Consolidator SHALL 将这些消息保留为未合并状态（不推进其合并边界），留待下一次合并触发时继续处理。
6. IF 某个分片批次的合并操作失败，THEN THE Memory_Consolidator SHALL 停止处理后续批次、将该失败批次及其之后的消息保留为未合并状态，并在日志中记录失败批次的序号与失败原因。

---

### Requirement 5: 工具输出截断阈值优化（P0）

**User Story:** 作为终端用户，我希望工具输出在被截断前保留足够上下文，以便 LLM 不必为获取被截断信息反复调用同一工具。

> 证据：`analysis-report-0526-0601.md` §五.4 —— 全周 98 次截断，主要为 exec/grep_search；0601 最慢请求（8.7 分钟）由截断恶性循环导致。代码现状：`shell.rs` 当前 `MAX_OUTPUT_CHARS = 20_000`（头部截断，仅保留前缀并标注剩余字符数），`read_file` 为 `128_000`。

#### Acceptance Criteria

1. THE System SHALL 将 exec 工具的输出截断上限设为可配置项，默认值为 20000 字符，且可配置下限不低于 8000 字符。
2. THE System SHALL 将 grep_search 工具的输出截断上限设为可配置项，默认值为 20000 字符，且可配置下限不低于 8000 字符。
3. WHEN exec 或 grep_search 工具的输出字符数超过对应截断上限，THE Tool_Output_Limiter SHALL 同时保留输出的头部段与尾部段两部分，使保留字符总数不超过该截断上限，且尾部保留段的字符数不少于该截断上限的 25%。
4. WHEN 工具输出被截断，THE Tool_Output_Limiter SHALL 在保留的头部段与尾部段之间插入截断标记，并在该标记中标明被省略的中间字符数。
5. THE System SHALL 保持 read_file 工具的现有截断上限（`MAX_READ_CHARS = 128_000`）不变。
6. IF 工具输出长度未超过对应截断上限，THEN THE Tool_Output_Limiter SHALL 返回完整输出且不附加任何截断标记。

---

### Requirement 6: LLM 调用并发控制与排队（P0）

**User Story:** 作为系统维护者，我希望在高并发时段对 LLM 调用进行并发控制和排队，以便避免类似 0527 17:00 的系统过载雪崩。

> 证据：`analysis-report-0526-0601.md` §五.1 —— 0527 17:00 时段集中 67% handler、79% tool_call 去重、7 次 SSE 超时，12+ 用户同时活跃导致过载。

#### Acceptance Criteria

1. THE System SHALL 提供可配置的全局并发 LLM 调用上限（in-flight 上限）。
2. WHEN 进行中的 LLM 调用数达到全局并发上限，THE Concurrency_Controller SHALL 将后续 LLM 调用放入队列等待而非立即全部发起。
3. WHILE 某 LLM 调用在队列中等待，THE Concurrency_Controller SHALL 在等待超过可配置的排队超时（默认 5 分钟）后以可识别的「超时/繁忙」错误终止该等待。
4. THE System SHALL 提供可配置的单用户并发请求上限（默认值为 3）。
5. WHEN 某用户进行中的请求数达到单用户并发请求上限，THE Concurrency_Controller SHALL 将该用户的后续请求排队，并在排队超时（默认 5 分钟）后拒绝。
6. WHEN 一个请求因并发上限被排队或拒绝，THE System SHALL 在审计日志中记录用户标识与触发的上限类型。

---

### Requirement 7: 子任务链超时控制（P1）

**User Story:** 作为终端用户，我希望子任务链在超过最大执行时间后自动终止并返回部分结果，以便不会出现 25 分钟级别的超长请求。

> 证据：`analysis-report-0526-0601.md` §三/§五.2 —— 0527 出现 1531s（25.5 分钟）请求，子任务链串行执行无超时保护。代码现状：`subtasks/config.rs` 默认 `default_timeout_ms = 120_000`，调度层已有 timeout 分支。

#### Acceptance Criteria

1. THE System SHALL 提供可配置的单个 Node 最大执行时间（默认值不超过 5 分钟）。
2. WHEN 某个 Node 的执行时间超过单个 Node 最大执行时间，THE Subtask_Dispatcher SHALL 终止该 Node 并将其状态标记为超时（`Failed` 且原因为 `timeout`）。
3. THE System SHALL 提供可配置的 `dispatch_subtasks` 整体最大执行时间。
4. WHEN 子任务链整体执行时间超过整体最大执行时间，THE Subtask_Dispatcher SHALL 终止剩余未完成 Node 并返回已完成 Node 的部分结果。
5. WHEN 子任务因超时被终止，THE Subtask_Dispatcher SHALL 在返回结果中标明哪些 Node 已完成、哪些因超时被终止。

---

### Requirement 8: 沙盒环境约束预检（P1）

**User Story:** 作为终端用户，我希望涉及文件修改或依赖配置文件的任务在执行前先检查环境可写性与配置可达性，以便不会陷入只读文件系统或找不到配置文件的反复调试循环。

> 证据：`analysis-report-0526-0601.md` §五.3/§九 —— 0601 `.config/ty.config.toml` 沙盒中无法定位导致 64 次无效沙盒执行、8.7 分钟请求；`analysis-report-0520-0525.md` §三.3 —— 0525 试图修改只读 skill 目录浪费 395s。

#### Acceptance Criteria

1. WHEN 一个任务计划包含对某路径的写入或编辑操作，THE System SHALL 在执行写入前检查该目标路径的可写性。
2. IF 目标路径不可写（如只读文件系统），THEN THE System SHALL 在执行写入操作前返回可识别的「路径不可写」错误并停止对该路径的重复写入尝试。
3. WHEN Sandbox 启动用于执行依赖配置文件的脚本，THE System SHALL 确保所需配置文件（如 `.config/ty.config.toml`）在 Sandbox 内可被脚本按约定路径访问。
4. IF 所需配置文件在 Sandbox 内无法定位，THEN THE System SHALL 返回可识别的「配置文件缺失」错误并停止对该配置路径的重复探查。
5. WHERE 任务涉及定时触发的 Skill，THE System SHALL 通过稳定的路径解析（glob 或别名）定位 Skill，使 Skill 路径变动时仍能命中。

---

### Requirement 9: 空结果任务快速返回（P1）

**User Story:** 作为终端用户，我希望查询类任务在首次数据源调用即返回 0 行时尽快告知我，以便不会等待十几分钟却得到一个空结果。

> 证据：`analysis-report-0520-0525.md` §三.1/§四.5 —— 0522 耗时 747s（12.5 分钟）却产出仅含表头的空 Excel。

#### Acceptance Criteria

1. WHEN 一个查询类任务在首次数据源调用后返回 0 行有效数据，THE System SHALL 将该任务识别为 Empty_Result_Task。
2. WHEN 一个任务被识别为 Empty_Result_Task，THE System SHALL 在可配置的快速返回时限（默认 30 秒）内向用户返回「无匹配数据」的提示而非继续完整生成流程。
3. IF 快速返回时限已到期而「无匹配数据」提示尚未发出，THEN THE System SHALL 在时限到期后仍向用户发送该提示，确保用户始终获得空结果反馈。
4. IF 一个 Empty_Result_Task 已向用户返回「无匹配数据」提示，THEN THE System SHALL 不再为该任务执行后续的文件生成与发送步骤。
5. WHERE 首次数据源调用返回了非空数据，THE System SHALL 继续正常执行后续生成流程。

---

### Requirement 10: SSE 流中断处理与动态超时（P2）

**User Story:** 作为终端用户，我希望在 SSE 流中途卡顿时系统能更稳健地处理超时与重试，以便高并发时段不会因 90 秒级停顿放大请求耗时。

> 证据：`analysis-report-0526-0601.md` §四 —— 全周 14 次 SSE 超时，全部 `has_content=true`（流中途停顿），跨模型持续存在，属上游 Provider 通用问题。

#### Acceptance Criteria

1. THE System SHALL 将 SSE_Stream 的 chunk 超时阈值设为可配置项（当前基线 90 秒）。
2. WHILE 系统处于高并发状态（in-flight LLM 调用数超过可配置阈值），THE System SHALL 将 SSE_Stream 的 chunk 超时阈值提升至可配置的高并发阈值（默认 150 秒）。
3. WHEN SSE_Stream 在已开始接收数据后超过 chunk 超时阈值无新数据，THE System SHALL 以指数退避策略发起重试。
4. WHILE SSE_Stream 等待上游数据，THE System SHALL 周期性发送心跳帧以维持连接。
5. WHEN SSE_Stream 重试次数达到可配置上限仍未成功，THE System SHALL 向用户返回可识别的「请稍后重试」提示而非失败兜底文案。

---

### Requirement 11: Prompt Token 体量压缩（P2）

**User Story:** 作为系统维护者，我希望压缩 prompt 中的工具 schema 描述与历史上下文，以便降低占比高达 99.2% 的 prompt token 成本并间接加速请求。

> 证据：`analysis-report-0526-0601.md` §十 —— 0527 单日 prompt token 9,751,412，占总量 99.2%，最重单会话 prompt 848,838 token；`analysis-report-0520-0525.md` —— 0521 prompt token 27,410,516。

#### Acceptance Criteria

1. THE System SHALL 提供工具 schema 描述的精简表示，使发送给 LLM 的工具定义 token 量低于精简前。
2. WHEN 构建请求 prompt，THE System SHALL 在历史上下文超过 token 预算时按既有 token 预算裁剪策略截断历史。
3. THE System SHALL 记录每次请求 prompt 中 history、cases、skills、tool 定义各部分的 token 占比观测指标。
4. WHERE 稳定前缀缓存命中条件满足（identity + bootstrap files + MEMORY.md 指纹未变），THE System SHALL 复用缓存前缀而不重复读取与拼接。

---

### Requirement 12: Skill 依赖冷启动预装（P2）

**User Story:** 作为终端用户，我希望图表与表格类 Skill 所需的依赖预装在沙盒镜像中，以便首次运行不必花费 18–74 秒安装依赖。

> 证据：`tyclaw_slow_skill_analysis_20260511.md` §四.3 —— matplotlib/openpyxl 首次执行 pip install 耗时 18–74s 冷启动。

#### Acceptance Criteria

1. THE Sandbox 镜像（`docker/sandbox/Dockerfile`）SHALL 预装常用绘图与表格依赖（至少包含 matplotlib 与 openpyxl）。
2. WHEN 一个 Skill 在 Sandbox 中首次运行并使用已预装依赖，THE System SHALL 不再触发该依赖的运行时 pip install。
3. WHERE 某 Skill 需要未预装的依赖，THE System SHALL 仍允许运行时安装该依赖。

---

### Requirement 13: 长链路 Skill 分段返回与缓存（P1）

**User Story:** 作为终端用户，我希望持仓抓取与截图这类长链路任务能分段返回结果并复用前一日缓存，以便不必为单个请求等待 5–9 分钟。

> 证据：`analysis-report-0526-0601.md` §六（tiger 账户日度工作流）与 `tyclaw_slow_skill_analysis_20260511.md` §一（report-analysis/exchange-rate 多次 5–9 分钟链路）。

#### Acceptance Criteria

1. WHERE 一个任务包含「数据文本结果」与「附件（截图/文件）」两类产出，THE System SHALL 先返回文本结果，再返回附件产出。
2. THE System SHALL 为长链路任务的可缓存中间结果（如前一日持仓数据）提供可配置 TTL 的缓存。
3. WHEN 长链路任务的某可缓存步骤存在未过期缓存，THE System SHALL 使用缓存值而非重新抓取。
4. WHEN 长链路任务的某个子步骤超过其可配置的步骤超时，THE System SHALL 终止该子步骤并返回已完成部分的结果。

---

### Requirement 14: 失败原因码与可观测性（P1）

**User Story:** 作为运维人员，我希望失败卡片与慢请求附带可区分的原因码并打点告警，以便快速定位是「撞迭代上限」「SSE 超时」还是「污染复读」。

> 证据：`incident-cannot-make-progress-0519.md` §五 —— 建议失败卡片附原因码（`hit_max_iterations` / `sse_timeout` / `pollution_replay`），并将「迭代上限重置」提升为 WARN 打点告警（当日 10 次未触发任何告警）。

#### Acceptance Criteria

1. WHEN 系统向用户返回失败结果，THE System SHALL 在审计日志中为该失败附带原因码，且原因码取值至少覆盖 `hit_max_iterations`、`sse_timeout`、`pollution_replay`、`subtask_timeout`、`empty_result`、`readonly_path`、`config_missing`。
2. WHEN Agent_Loop 因命中 `max_iterations` 而重置迭代计数器，THE System SHALL 以 WARN 级别记录该事件并附带 Workspace 标识。
3. WHEN 同一 turn 同时命中 SSE 超时与 `max_iterations`，THE System SHALL 标记原因码为 `sse_timeout` 优先，并向用户返回「请稍后重试」类提示而非失败兜底文案。
4. THE System SHALL 记录每日慢请求（超过可配置耗时阈值）的原因码分布以供运维统计。
