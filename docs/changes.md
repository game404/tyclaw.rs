# TyClaw.rs Changelog

## v1.0 (2026-03-20)

### Multi-Agent 并行编排

新增多模型并行执行框架，主控 LLM 作为 Planner，通过 `dispatch_subtasks` 工具将子任务分发给不同专业模型并行执行。

**核心模块** (`crates/tyclaw-orchestration/src/multi_model/`)：
- **routing** — node_type → 目标模型路由，支持通配符匹配
- **scheduler** — DAG 拓扑调度 + 信号量并发控制 + 超时处理
- **executor** — 为每个子任务启动独立 mini AgentLoop（带完整工具集）
- **reducer** — 结果归并：去重、矛盾检测、可选 LLM 综合
- **tool** — `dispatch_subtasks` 工具，注册到主控 ReAct 循环
- **protocol** — TaskPlan / TaskNode / ExecutionRecord 数据结构
- **config** — 多 Provider 注册、路由规则、故障策略配置

**关键设计**：
- 主控通过 `node_type`（coding/analysis/search/review 等）自动路由到最擅长的模型
- DAG 依赖链：上游输出自动注入下游 context
- 单任务短路优化：只有 1 个子任务时跳过 DAG/Reducer 开销
- 多模型模式下主控使用受限工具集（read_file + exec_main），编码任务强制走 subagent
- subagent 不继承主控的 system prompt / skills / memory，context 精简独立

### 工具层重构

- 新增 `ExecMainTool` — 受限 shell，禁止代码执行和内容写入（sed/awk/python/node 等），多模型模式下主控使用此工具
- 新增 `fileops.rs` — 文件操作工具重构
- `filesystem.rs` — read_file/write_file 增强，write_file 自动创建父目录

### Provider 层增强

- Extended Thinking 支持两种模式：
  - **adaptive**（`reasoning.effort`）— 模型自行决定是否 think
  - **forced**（`reasoning.max_tokens`）— 强制每轮 think
- reasoning 多轮回传：assistant message 携带 `reasoning` 字段，保持 thinking 上下文连续
- `sanitize_messages` 白名单加入 `reasoning`，避免被 context 清洗过滤
- 提取超时常量（SEND_TIMEOUT / CHUNK_TIMEOUT / NON_STREAM_TIMEOUT）
- 新增 `set_temperature()` / `set_max_tokens()` per-provider 参数覆盖
- LLMProvider trait 新增 `api_base()` / `api_key()` 方法
- subagent 使用 provider 的实际模型名（而非路由别名）发送请求

### Agent Loop 优化

- read_file 工具输出不截断（保护用户需求文档完整性）
- context 压缩时保护前 N 条 read_file 结果（需求/规格文档永不压缩）
- CLI 前端输出 `[Thinking]` 内容
- reasoning 日志增强（has_reasoning / reasoning_len / SSE chunk 调试）
- subagent 工作规范注入（效率优先、验证策略、禁止翻页、临时文件写 tmp/）

### GUIDELINES 更新

- 多模型调度规范：使用场景、依赖关系规则
- 依赖关系强制规则：有信息依赖的子任务必须用 dependencies 串联，禁止盲目并行
- 子任务 prompt 编写规范：必须包含 workspace 路径、业务规则、文件结构、期望输出

### 构建优化

- tokio features 精简（`full` → 按需声明）
- 依赖升级：tiktoken-rs 0.6→0.9、tokio-tungstenite 0.24→0.29
- 消除 base64 / thiserror 重复版本
- Release profile：fat LTO + opt-level z（6.4MB → 5.2MB，-19%）

### 配置与运维

- 新增 `config.china.yaml` — 国内模型配置模板（deepseek + glm + kimi）
- 新增 `config.multi_haiwai.yaml` — 海外模型配置模板（claude + gpt + gemini）
- `config.example.yaml` — 新增 multi_model 配置块
- `reset.sh` — 增强清理：subagent 产出、skills/ 残留、workspace 临时文件、tmp/

### 测试验证

| 方案 | 耗时 | 结果 |
|------|------|------|
| 单模型 claude-opus | 14.6min | 通过 |
| 多模型 claude 主控 + claude/gemini subagent | 18.6min | 通过 |
| 多模型 deepseek 主控 + glm/kimi subagent | 13.1min | 通过，核心验证值全匹配 |
