# Implementation Plan: Execution Performance Optimization

## Overview

本实现计划将 design.md 的四层治理框架（历史卫生 / 资源治理 / 快速失败 / 成本与可观测性）拆解为增量、测试驱动的 Rust 编码任务。实现原则遵循设计：最大化复用现有结构、所有阈值收敛到统一的 `PerformanceConfig`。

任务顺序：先落地统一配置与纯函数化核心模块（便于属性测试），再逐层接入编排/工具/Provider，最后在 `tyclaw-app/src/main.rs` 与 `orchestrator.rs` 中完成全链路 wiring。

测试约定（来自 design.md「Testing Strategy」）：
- 属性测试使用 Rust `proptest` 库，`ProptestConfig { cases: 100, .. }`（最少 100 次迭代），每条 Correctness Property 由**单个**属性测试实现。
- 每个属性测试以注释标注：`// Feature: execution-performance-optimization, Property {number}: {property_text}`。
- 集成测试覆盖并发/时序/沙盒类（1–3 例）；烟雾测试覆盖镜像依赖预装（单次）。

## Tasks

- [x] 1. 建立统一性能配置基座（PerformanceConfig）
  - 在 `crates/tyclaw-orchestration/src/config.rs` 的 `BaseConfig` 新增 `performance: PerformanceConfig` 段
  - 定义 `PerformanceConfig` 及子结构：`PollutionConfig`、`SizeLimitConfig`、`ConsolidationConfig`、`TruncationConfig`、`ConcurrencyConfig`、`SubtaskTimeoutConfig`、`EmptyResultConfig`、`SseConfig`、`slow_request_threshold_secs`
  - 每个子结构实现 `Default`，缺省值取需求中的默认值（污染关键词默认 `["I cannot make progress", "error", "blocked"]`、`short_message_max_chars=512`、`max_messages=500`、`rolling_target=400`、`pairs_fixes_warn=30`、`pairs_fixes_force_reset=60`、`max_messages_per_batch=500`、`max_rounds=5`、`exec_truncate_chars=20000`、`grep_truncate_chars=20000`、`per_user_max_inflight=3`、`queue_timeout=5min`、`fast_return_secs=30`、`chunk_timeout_secs=90`、`high_concurrency_timeout_secs=150`）
  - 实现加载时的 `clamp`：截断下限 ≥ 8000、并发 ≥ 1、node 超时 ≤ 5min
  - _Requirements: 1.3, 3.1, 4.1, 5.1, 5.2, 6.1, 6.4, 7.1, 7.3, 10.1, 14.4_

  - [x] 1.1 编写截断配置 clamp 的属性测试
    - **Property 18: 截断上限配置被 clamp 到下限以上**
    - **Validates: Requirements 5.1, 5.2**

  - [x] 1.2 编写配置默认值单元测试
    - 验证各子结构 `Default` 值与需求一致（1.3 默认关键词、3.1/4.1/5.5/6.1/6.4/7.1/7.3/10.1 默认值）
    - 验证并发 ≥1、node 超时 ≤5min 的 clamp
    - _Requirements: 1.3, 3.1, 4.1, 6.1, 6.4, 7.1, 7.3, 10.1_

- [x] 2. 实现污染过滤模块（Pollution Filter）
  - [x] 2.1 创建 `crates/tyclaw-orchestration/src/pollution_filter.rs` 纯函数核心
    - 实现 `is_pollution_candidate(msg, cfg)`：role==tool ∧ len ≤ short_message_max_chars ∧ 不区分大小写完整短语匹配关键词，三条件合取
    - 实现 `contains_pollution_phrase(text, cfg)` 供子任务状态复用
    - 在 `lib.rs` 注册模块
    - _Requirements: 1.1, 1.7_

  - [x] 2.2 编写污染候选判定属性测试
    - **Property 1: 污染候选判定等价于三条件合取**
    - **Validates: Requirements 1.1, 1.7**

  - [x] 2.3 实现 `filter_pollution` 剔除与占位补齐
    - 剔除污染 tool 消息后，为失去 tool_result 的 tool_call 补齐含 `[pollution-removed]` 标识的占位 tool_result
    - 保留所有非污染消息内容与相对顺序
    - 返回 `PollutionFilterResult { cleaned, removed_count, placeholder_ids }`
    - _Requirements: 1.2, 1.4, 1.6_

  - [x] 2.4 编写配对完整性与占位补齐属性测试
    - **Property 2: 污染剔除保持配对完整性并补齐占位**
    - **Validates: Requirements 1.2, 1.4**

  - [x] 2.5 编写非污染消息原样保留属性测试
    - **Property 3: 非污染消息内容与顺序原样保留**
    - **Validates: Requirements 1.6**

  - [x] 2.6 实现污染剔除审计记录
    - 每次 `filter_pollution` 恰好产出一条审计记录，含 `removed_count`（含 0）与 Workspace 标识
    - _Requirements: 1.5_

  - [x] 2.7 编写审计计数准确性属性测试
    - **Property 4: 污染剔除审计计数准确（含 0）**
    - **Validates: Requirements 1.5**

- [x] 3. 实现子任务状态一致性（Subtask Status Consistency）
  - [x] 3.1 扩展 `NodeStatus` 枚举增加 `Blocked` 变体
    - 修改 `crates/tyclaw-orchestration/src/subtasks/protocol.rs`，处理所有 match 分支
    - _Requirements: 2.1_

  - [x] 3.2 实现 `reconcile_node_status` 状态校正
    - 在 `crates/tyclaw-orchestration/src/subtasks/executor.rs` 实现：正文含污染词 → {Failed, Blocked} 绝不 Success；命中 max_iterations 且正文空白或含污染词 → Failed
    - 复用 `pollution_filter::contains_pollution_phrase`
    - _Requirements: 2.1, 2.2_

  - [x] 3.3 编写状态校正属性测试
    - **Property 5: 污染/撞顶节点状态绝不判为成功**
    - **Validates: Requirements 2.1, 2.2**

  - [x] 3.4 实现 `node_status_to_declared_status` 映射并接入写回
    - 在 `crates/tyclaw-orchestration/src/subtasks/tool.rs` 新增映射函数取代 `extract_declared_result_status`：仅 Success → 成功语义，Failed/Blocked/Skipped → 失败语义
    - _Requirements: 2.3, 2.6_

  - [x] 3.5 编写声明状态映射属性测试
    - **Property 6: 状态到声明状态的映射保持失败语义**
    - **Validates: Requirements 2.3, 2.6**

  - [x] 3.6 在 `DispatchNodeSummary` 增加独立 `failure_reason` 字段
    - 在 `subtasks/tool.rs` / `protocol.rs` 中为 Failed/Blocked 节点始终填充失败原因，独立于正文文本
    - _Requirements: 2.4, 2.5_

  - [x] 3.7 编写失败原因附带属性测试
    - **Property 7: 失败节点始终附带独立失败原因**
    - **Validates: Requirements 2.4, 2.5**

- [x] 4. Checkpoint - 历史卫生核心模块
  - Ensure all tests pass, ask the user if questions arise.

- [x] 5. 实现会话规模上限（Session Size Limits）
  - [x] 5.1 实现 `History_Processor::adjust_truncation_boundary`
    - 在 `crates/tyclaw-orchestration/src/history.rs`：给定起始下标向更早方向回退，使保留窗口不以孤立 tool_result 开头（落在完整 user turn 起点）
    - _Requirements: 3.7_

  - [x] 5.2 编写截断边界调整属性测试
    - **Property 11: 截断边界调整后不以孤立 tool_result 开头**
    - **Validates: Requirements 3.7**

  - [x] 5.3 实现 `SessionManager::enforce_size_limits`
    - 在 `crates/tyclaw-orchestration/src/session_manager.rs`：顺序为强制合并 → 仍超限则滚动截断回落至 `rolling_target`；Pairs_Fixes 超 reset 阈值则强制重置保留最近一个完整 user turn
    - 返回 `SizeLimitAction`，截断/重置后调用 `adjust_truncation_boundary` 保证配对完整性
    - _Requirements: 3.2, 3.3, 3.4, 3.5, 3.6_

  - [x] 5.4 编写强制合并边界属性测试
    - **Property 8: 强制合并边界自最早消息按时序推进**
    - **Validates: Requirements 3.2**

  - [x] 5.5 编写滚动截断水位属性测试
    - **Property 9: 滚动截断回落至目标水位以内**
    - **Validates: Requirements 3.3**

  - [x] 5.6 编写 Pairs_Fixes 阈值触发属性测试
    - **Property 10: Pairs_Fixes 阈值触发对应动作**
    - **Validates: Requirements 3.4, 3.5**

  - [x] 5.7 编写截断/重置配对完整性属性测试
    - **Property 12: 截断/重置后保持配对完整性**
    - **Validates: Requirements 3.6**

  - [x] 5.8 实现 size-limit 动作审计记录
    - 在 `orchestrator.rs` 接入处记录触发原因（消息数超限 / Pairs_Fixes 超限）与 Workspace 标识
    - _Requirements: 3.8_

  - [x] 5.9 编写 size-limit 审计一致性属性测试
    - **Property 13: size-limit 动作审计原因与动作一致**
    - **Validates: Requirements 3.8**

- [x] 6. 实现记忆合并体量上限（Memory Consolidation Cap）
  - [x] 6.1 实现 `pick_batch_boundary` 分片边界
    - 在 `crates/tyclaw-memory/src/consolidator.rs`：在 user turn 边界内切出 ≤ `max_messages_per_batch` 的批次，不拆分单个 user turn
    - _Requirements: 4.2_

  - [x] 6.2 编写分片边界属性测试
    - **Property 14: 合并分片不超上限且不拆分 user turn**
    - **Validates: Requirements 4.2**

  - [x] 6.3 接入分批合并循环与上限/失败处理
    - 单次合并调用最多处理 `max_rounds=5` 批；达上限剩余消息保留为未合并（不推进边界）
    - 某批失败则停止后续、保留失败批及之后为未合并、记录失败批序号与原因
    - 每次合并记录处理消息数与批次数
    - _Requirements: 4.3, 4.4, 4.5, 4.6_

  - [x] 6.4 编写批次上限属性测试
    - **Property 15: 单次合并调用至多处理 5 个分片批次**
    - **Validates: Requirements 4.3**

  - [x] 6.5 编写达上限保留未合并属性测试
    - **Property 16: 达批次上限后剩余消息保留为未合并**
    - **Validates: Requirements 4.5**

  - [x] 6.6 编写分片失败停止属性测试
    - **Property 17: 分片失败停止后续并保留未合并**
    - **Validates: Requirements 4.6**

- [x] 7. 实现工具输出截断优化（Tool Output Limiter）
  - [x] 7.1 实现共享 `truncate_head_tail` 函数
    - 在 `crates/tyclaw-tools/src/base.rs`：头尾双段保留，总字符 ≤ max，尾段 ≥ max*0.25，中间插入标明省略字符数的截断标记；≤max 时原样返回
    - _Requirements: 5.3, 5.4, 5.6_

  - [x] 7.2 编写头尾截断总量与尾段比例属性测试
    - **Property 19: 头尾双段截断满足总量与尾段比例约束**
    - **Validates: Requirements 5.3**

  - [x] 7.3 编写截断标记省略字符数属性测试
    - **Property 20: 截断标记标明正确的省略字符数**
    - **Validates: Requirements 5.4**

  - [x] 7.4 编写未超上限恒等属性测试
    - **Property 21: 未超上限时截断为恒等操作**
    - **Validates: Requirements 5.6**

  - [x] 7.5 在 exec 与 grep_search 输出路径替换截断调用
    - 在 `crates/tyclaw-tools/src/shell.rs` 用 `truncate_head_tail` 取代纯头部 `truncate_by_chars`，读取配置上限；保持 `read_file` 的 `MAX_READ_CHARS=128_000` 不变
    - _Requirements: 5.1, 5.2, 5.5_

  - [x] 7.6 编写 read_file 上限保持单元测试
    - 验证 `MAX_READ_CHARS = 128_000` 不变
    - _Requirements: 5.5_

- [x] 8. Checkpoint - 历史卫生与截断完成
  - Ensure all tests pass, ask the user if questions arise.

- [x] 9. 实现并发控制器（Concurrency Controller）
  - [x] 9.1 创建 `crates/tyclaw-provider/src/concurrency.rs` 扩展现有信号量
    - 全局沿用 `Semaphore`；per-user 用 `HashMap<String, Arc<Semaphore>>`；`acquire_permit(user_id)` 用 `tokio::time::timeout(queue_timeout, ..)` 实现排队超时，返回 `Permit` 或 `ConcurrencyError::QueueTimeout { limit_kind }`
    - 排队/拒绝时写审计含 user_id 与 limit_kind
    - _Requirements: 6.1, 6.2, 6.3, 6.4, 6.5, 6.6_

  - [x] 9.2 将 `chat_with_retry` 的信号量获取改为调用控制器
    - 在 `crates/tyclaw-provider/src/provider.rs` 接入；user_id 经 `cache_scope`/task_local 传入
    - _Requirements: 6.2, 6.5_

  - [x] 9.3 编写并发拒绝审计属性测试
    - **Property 22: 并发拒绝审计记录上限类型与用户**
    - **Validates: Requirements 6.6**

  - [x] 9.4 编写并发排队与超时集成测试
    - 覆盖全局/单用户排队、排队超时拒绝
    - _Requirements: 6.2, 6.3, 6.5_

- [x] 10. 实现子任务链超时控制（Subtask Chain Timeout）
  - [x] 10.1 实现 per-node 与 dispatch 整体超时
    - 在 `crates/tyclaw-orchestration/src/subtasks/scheduler.rs`：node 有效超时 `min(node.timeout_ms, node_max_duration)`；`DagScheduler::execute` 外层包 `dispatch_max_duration` 超时，超时终止未完成 node 记 `Failed + "chain_timeout"`，返回已完成部分结果并标注超时节点
    - _Requirements: 7.2, 7.4, 7.5_

  - [x] 10.2 编写超时结果分类属性测试
    - **Property 23: 子任务超时结果正确分类每个节点**
    - **Validates: Requirements 7.5**

  - [x] 10.3 编写 node 与 dispatch 超时时序集成测试
    - 覆盖 per-node 超时与整体超时部分结果返回
    - _Requirements: 7.2, 7.4_

- [x] 11. 实现沙盒环境约束预检（Sandbox Precheck）
  - [x] 11.1 实现可写性与配置可达性预检层
    - 在 `crates/tyclaw-tools`（写/编辑工具路径）实现：写前可写性探测，只读返回 `readonly_path:<path>`；依赖配置脚本启动前校验约定路径，缺失返回 `config_missing:<path>`
    - 用 per-turn `HashSet<PathBuf>` 记忆已知只读/缺失路径，二次探查直接短路返回缓存错误
    - _Requirements: 8.1, 8.2, 8.3, 8.4_

  - [x] 11.2 编写已知路径短路属性测试
    - **Property 24: 已知只读/缺失路径短路重复探查**
    - **Validates: Requirements 8.2, 8.4**

  - [x] 11.3 实现 Skill 路径稳定解析
    - 在 `crates/tyclaw-orchestration/src/skill_manager.rs` 用 glob/别名模式解析 Skill 路径，路径变动仍命中
    - _Requirements: 8.5_

  - [x] 11.4 编写 Skill 路径解析属性测试
    - **Property 25: Skill 路径稳定解析命中变体**
    - **Validates: Requirements 8.5**

  - [x] 11.5 编写沙盒可写性与配置可达集成测试
    - 覆盖只读路径与配置缺失首次探测行为
    - _Requirements: 8.1, 8.3_

- [x] 12. 实现空结果任务快速返回（Empty Result Fast Return）
  - [x] 12.1 实现空结果识别与快速返回
    - 在查询类 skill 协议中约定首次 0 行输出结构化标记；编排层 `is_empty_result(n)` 识别后在 `fast_return_secs` 内返回「无匹配数据」并跳过后续生成/发送；非空则正常执行
    - _Requirements: 9.1, 9.2, 9.4, 9.5_

  - [x] 12.2 编写空结果判定属性测试
    - **Property 26: 空结果判定等价于零行**
    - **Validates: Requirements 9.1**

  - [x] 12.3 编写跳过后续生成属性测试
    - **Property 27: 空结果任务返回提示后跳过后续生成**
    - **Validates: Requirements 9.4**

  - [x] 12.4 编写空结果时序集成测试
    - 覆盖时限内返回与到期补发
    - _Requirements: 9.2, 9.3_

- [x] 13. Checkpoint - 资源治理与快速失败完成
  - Ensure all tests pass, ask the user if questions arise.

- [x] 14. 实现 SSE 动态超时（SSE Dynamic Timeout）
  - [x] 14.1 实现 `effective_chunk_timeout` 并接入 openai_compat
    - 在 `crates/tyclaw-provider/src/openai_compat.rs` 将 `CHUNK_TIMEOUT_SECS` 常量改为运行期可配置；in-flight > 阈值时用 `high_concurrency_timeout_secs`；已开始接收数据后超时走指数退避重试；SSE 等待期补发心跳
    - _Requirements: 10.1, 10.2, 10.3, 10.4_

  - [x] 14.2 编写动态超时选择属性测试
    - **Property 28: chunk 超时阈值随并发单调选择**
    - **Validates: Requirements 10.2**

  - [x] 14.3 实现重试耗尽返回「请稍后重试」提示
    - 重试达上限返回可识别的「请稍后重试」类提示，不含失败兜底文案
    - _Requirements: 10.5_

  - [x] 14.4 编写重试耗尽提示属性测试
    - **Property 29: 重试耗尽返回稍后重试提示而非兜底文案**
    - **Validates: Requirements 10.5**

  - [x] 14.5 编写 SSE 重试与心跳集成测试
    - 覆盖已开始接收数据后超时重试与心跳维持
    - _Requirements: 10.3, 10.4_

- [x] 15. 实现 Prompt Token 压缩（Prompt Token Compression）
  - [x] 15.1 实现工具 schema 精简表示
    - 在 prompt/`compression.rs` 层提供 `compact` 模式工具定义，去除冗长 description/示例，使发送 token < 精简前
    - _Requirements: 11.1_

  - [x] 15.2 编写精简 schema token 量属性测试
    - **Property 30: 精简工具 schema 的 token 量低于原始**
    - **Validates: Requirements 11.1**

  - [x] 15.3 接入历史 token 预算裁剪
    - 复用 `crates/tyclaw-agent/src/compression.rs` 的 `trim_history_by_token_budget`，在历史超预算时裁剪
    - _Requirements: 11.2_

  - [x] 15.4 编写历史裁剪预算属性测试
    - **Property 31: 历史裁剪结果不超 token 预算**
    - **Validates: Requirements 11.2**

  - [x] 15.5 实现稳定前缀缓存指纹复用
    - 基于 (identity, bootstrap files, MEMORY.md) 指纹三元组，复用现有 `cache_scope`/`cache_breakpoint_idx`，三者全等时复用前缀
    - _Requirements: 11.4_

  - [x] 15.6 编写前缀缓存命中属性测试
    - **Property 32: 稳定前缀缓存命中当且仅当指纹全等**
    - **Validates: Requirements 11.4**

  - [x] 15.7 实现 token 占比观测指标
    - 每次请求记录 history/cases/skills/tool 定义各部分 token，写入 progress/审计指标
    - _Requirements: 11.3_

  - [x] 15.8 编写 token 占比指标集成测试
    - 验证各部分 token 占比被记录
    - _Requirements: 11.3_

- [x] 16. 实现 Skill 依赖冷启动预装（Skill Dependency Preinstall）
  - [x] 16.1 在沙盒镜像预装 matplotlib
    - 在 `docker/sandbox/requirements.txt` 增加 `matplotlib`（openpyxl 已存在）
    - _Requirements: 12.1_

  - [x] 16.2 编写镜像依赖预装烟雾测试
    - 构建后 import 验证 matplotlib/openpyxl，首次运行不触发 pip install
    - _Requirements: 12.1, 12.2_

- [x] 17. 实现长链路 Skill 分段返回与缓存（Segmented Return & Caching）
  - [x] 17.1 实现 `StepCache` TTL 缓存
    - 在 `crates/tyclaw-tools` 实现 `StepCache.get/put`：未过期返回缓存值，过期返回 None
    - _Requirements: 13.2, 13.3_

  - [x] 17.2 编写 TTL 缓存语义属性测试
    - **Property 34: 步骤缓存遵循 TTL 语义**
    - **Validates: Requirements 13.3**

  - [x] 17.3 实现文本先于附件的分段返回
    - 在 skill 协议/`tyclaw-tools` 返回路径：含文本+附件的任务先发文本再发附件；子步骤超步骤超时则终止返回已完成部分
    - _Requirements: 13.1, 13.4_

  - [x] 17.4 编写文本先于附件属性测试
    - **Property 33: 文本结果先于附件返回**
    - **Validates: Requirements 13.1**

  - [x] 17.5 编写子步骤超时集成测试
    - 覆盖子步骤超时终止并返回已完成部分
    - _Requirements: 13.4_

- [x] 18. 实现失败原因码与可观测性（Failure Codes & Observability）
  - [x] 18.1 实现 `FailureCode` 枚举与 `resolve_priority`
    - 在 `crates/tyclaw-control/src/audit.rs` 定义 `FailureCode` 与 `FailureAuditEntry`；`resolve_priority` 在同时含 SseTimeout 与 HitMaxIterations 时返回 SseTimeout
    - _Requirements: 14.1, 14.3_

  - [x] 18.2 编写失败审计原因码属性测试
    - **Property 35: 失败审计始终携带合法原因码**
    - **Validates: Requirements 14.1**

  - [x] 18.3 编写 SSE 优先级属性测试
    - **Property 36: SSE 超时优先于撞顶**
    - **Validates: Requirements 14.3**

  - [x] 18.4 接入失败审计与慢请求统计
    - 失败结果附原因码写审计；`max_iterations` 重置以 WARN 记录含 workspace；记录每日慢请求（超 `slow_request_threshold_secs`）原因码分布
    - _Requirements: 14.1, 14.2, 14.4_

  - [x] 18.5 编写迭代重置 WARN 与慢请求统计集成测试
    - 覆盖 max_iterations 重置 WARN 打点与慢请求原因码分布记录
    - _Requirements: 14.2, 14.4_

- [x] 19. 全链路 wiring 与配置注入
  - [x] 19.1 在 main.rs 注入 PerformanceConfig 到各组件
    - 在 `crates/tyclaw-app/src/main.rs` 启动时将 `PerformanceConfig` 注入 Orchestrator、provider（并发/SSE）、工具层（截断）
    - _Requirements: 1.3, 5.1, 5.2, 6.1, 10.1_

  - [x] 19.2 在 orchestrator 请求生命周期接入治理关卡
    - 在 `crates/tyclaw-orchestration/src/orchestrator.rs` fresh user turn 处接入 `filter_pollution` → `enforce_tool_call_pairing` → `enforce_size_limits`；接入 size-limit 与污染剔除审计
    - _Requirements: 1.1, 1.2, 1.5, 3.2, 3.6, 3.8_

  - [x] 19.3 编写编排关卡端到端集成测试
    - 验证污染剔除 + 配对修复 + 规模限制在单回合内协同工作
    - _Requirements: 1.1, 1.2, 3.6_

- [x] 20. 最终 Checkpoint - 全量测试通过
  - Ensure all tests pass, ask the user if questions arise.

## Notes

- 标记 `*` 的子任务为可选（单元/属性/集成/烟雾测试），可为快速 MVP 跳过；核心实现任务不带 `*`。
- 每个任务引用具体需求子条款编号以保证可追溯性。
- 36 条 Correctness Properties 各由单个 proptest 属性测试实现（最少 100 次迭代），紧邻其实现任务放置以尽早发现错误。
- 并发/时序/沙盒/镜像类验收标准采用集成测试与烟雾测试（design.md「Testing Strategy」），不做属性测试。
- Checkpoint 任务确保增量验证。

## Task Dependency Graph

```json
{
  "waves": [
    { "id": 0, "tasks": ["1.1", "1.2", "2.1", "7.1", "9.1", "16.1", "17.1", "18.1"] },
    { "id": 1, "tasks": ["2.2", "2.3", "3.1", "5.1", "6.1", "7.2", "7.3", "7.4", "9.3", "11.1", "11.3", "14.1", "15.1", "15.3", "16.2", "17.2", "17.3", "18.2", "18.3"] },
    { "id": 2, "tasks": ["2.4", "2.5", "2.6", "3.2", "3.4", "3.6", "5.2", "5.3", "6.2", "6.3", "7.5", "7.6", "9.2", "9.4", "10.1", "11.2", "11.4", "11.5", "12.1", "14.2", "14.3", "15.2", "15.4", "15.5", "15.7", "17.4", "17.5", "18.4"] },
    { "id": 3, "tasks": ["2.7", "3.3", "3.5", "3.7", "5.4", "5.5", "5.6", "5.8", "6.4", "6.5", "6.6", "10.2", "10.3", "12.2", "12.3", "12.4", "14.4", "14.5", "15.6", "15.8", "18.5", "19.1"] },
    { "id": 4, "tasks": ["5.7", "5.9", "19.2"] },
    { "id": 5, "tasks": ["19.3"] }
  ]
}
```
