# 多模型编排系统：提示词拼装全流程

> 本文档描述 TyClaw.rs 多模型编排系统中，从主 LLM 到 Sub-Agent 的完整提示词拼装流程。
> 所有提示词模板已外置到 `config/prompts/`，修改无需重新编译。

---

## 全局架构图

```
┌──────────────────────────────────────────────────────────────────┐
│                        用户输入                                   │
└───────────────────────────┬──────────────────────────────────────┘
                            ▼
┌──────────────────────────────────────────────────────────────────┐
│                     主 LLM (ReAct 循环)                          │
│                                                                  │
│  System Prompt 拼装：                                            │
│  ┌─────────────────────────────────────────────────────────────┐ │
│  │ ① IDENTITY.md          ← workspace 根目录                   │ │
│  │ ② AGENTS.md            ← workspace/*.md (按字母序)           │ │
│  │ ③ GUIDELINES.md        ←                                    │ │
│  │ ④ TOOLS.md             ←                                    │ │
│  │ ⑤ memory/MEMORY.md     ← 长期记忆                           │ │
│  │ ⑥ 当前时间              ← 运行时注入                         │ │
│  │ ⑦ 可用能力列表          ← 工具注册表生成                      │ │
│  │ ⑧ 匹配的 Skill 内容    ← Skill 触发匹配                     │ │
│  │ ⑨ 相似历史案例          ← 案例库检索                         │ │
│  └─────────────────────────────────────────────────────────────┘ │
│                                                                  │
│  工具集（取决于 main_llm_full_tools 配置）：                      │
│  ┌──────────────────────────────┐  ┌──────────────────────────┐  │
│  │ full_tools = true            │  │ full_tools = false       │  │
│  │ read_file, write_file,       │  │ read_file, exec_main,    │  │
│  │ edit_file, list_dir,         │  │ ask_user, send_file,     │  │
│  │ exec, ask_user, send_file,   │  │ dispatch_subtasks        │  │
│  │ dispatch_subtasks            │  │                          │  │
│  └──────────────────────────────┘  └──────────────────────────┘  │
│                                                                  │
│  当主 LLM 调用 dispatch_subtasks 时 ──────────────────────────▶  │
└──────────────────────────────────────────────────────────────────┘
                            │
                            ▼
┌──────────────────────────────────────────────────────────────────┐
│               dispatch_subtasks 工具执行                          │
│                                                                  │
│  输入参数：                                                       │
│  {                                                               │
│    subtasks: [{id, node_type, prompt, dependencies}, ...],       │
│    context: "主 LLM 的关键发现和约束...",                          │
│    failure_policy: "best_effort"                                 │
│  }                                                               │
│                                                                  │
│  处理步骤：                                                       │
│  ① 构建 TaskPlan (DAG)                                           │
│  ② 注入 main_context → executor                                  │
│  ③ 生成 .dispatch/main_llm.md                                    │
│  ④ DAG 调度 → 并行执行 sub-agent                                 │
│  ⑤ 结果归并 → 返回主 LLM                                         │
└──────────────────────────────────────────────────────────────────┘
                            │
                ┌───────────┼───────────┐
                ▼           ▼           ▼
┌────────────────┐ ┌────────────────┐ ┌────────────────┐
│  Sub-Agent 1   │ │  Sub-Agent 2   │ │  Sub-Agent N   │
│  (coding)      │ │  (reasoning)   │ │  (summarize)   │
└────────────────┘ └────────────────┘ └────────────────┘
```

---

## 第一部分：主 LLM 的提示词拼装

### 1.1 System Prompt 构成

主 LLM 的 system prompt 由 `ContextBuilder::build_system_prompt_with_params()` 拼装。

**代码位置**：`crates/tyclaw-agent/src/context.rs:505-570`

```
┌─── system prompt ──────────────────────────────────────────────┐
│                                                                │
│  [稳定前缀 - 编译时缓存]                                        │
│  ├── IDENTITY.md 内容                                          │
│  │   来源: workspace/IDENTITY.md                                │
│  │   包含: 角色定义、核心能力、语言偏好、工作风格                   │
│  │                                                             │
│  ├── Bootstrap 文件（所有 workspace/*.md，按字母序）              │
│  │   来源: workspace/AGENTS.md, GUIDELINES.md, TOOLS.md 等      │
│  │   Full 模式: 加载所有 .md                                    │
│  │   Minimal 模式: 只加载 TOOLS.md + GUIDELINES.md              │
│  │                                                             │
│  └── Memory                                                    │
│      来源: workspace/memory/MEMORY.md                           │
│                                                                │
│  ---                                                           │
│                                                                │
│  [动态部分 - 每轮刷新]                                           │
│  ├── 当前时间: "2026-03-23 15:30:00 +08:00"                    │
│  ├── 能力列表: 工具分类描述                                      │
│  ├── 匹配 Skill: 触发词命中的 Skill 完整内容                     │
│  └── 相似案例: 从案例库检索的历史记录                              │
│                                                                │
└────────────────────────────────────────────────────────────────┘
```

### 1.2 工具注册

**代码位置**：`crates/tyclaw-orchestration/src/builder.rs:151-287`

```yaml
# config.yaml 中的关键配置
multi_model:
  enabled: true                    # 是否启用多模型编排
  main_llm_full_tools: true        # 主 LLM 是否拥有完整工具集
```

| 配置 | 工具集 | 说明 |
|------|--------|------|
| `full_tools: true` | read_file, write_file, edit_file, list_dir, exec, ask_user, send_file, **dispatch_subtasks** | 主 LLM 可以自己干活也可以 dispatch |
| `full_tools: false` | read_file, exec_main(受限), ask_user, send_file, **dispatch_subtasks** | 主 LLM 只能规划和 dispatch，不能直接写文件 |

### 1.3 dispatch_subtasks 工具描述

主 LLM 通过工具描述了解如何使用 dispatch_subtasks。

**模板文件**：`config/prompts/dispatch_tool_description.md`

**变量替换**：
- `{routing_table}` → 从 config.yaml routing_rules 动态生成
- `{max_concurrency}` → 从 config.yaml 读取

**代码位置**：`crates/tyclaw-orchestration/src/multi_model/tool.rs:107-120`

---

## 第二部分：dispatch_subtasks 调用处理

### 2.1 参数解析与 TaskPlan 构建

**代码位置**：`crates/tyclaw-orchestration/src/multi_model/tool.rs:213-302`

```
主 LLM 调用: dispatch_subtasks({
  subtasks: [
    {id: "write_code", node_type: "coding", prompt: "...", dependencies: []},
    {id: "write_doc",  node_type: "general", prompt: "...", dependencies: ["write_code"]}
  ],
  context: "关键发现：1. 使用账面数据... 2. 排除未结束月份...",
  failure_policy: "best_effort"
})

        ┌──── 处理流程 ────┐
        │                  │
        ▼                  │
  ① 解析 subtasks         │
        │                  │
        ▼                  │
  ② 构建 edges            │  安全兜底：如果 LLM 没设依赖
     (from dependencies)  │  且有 2+ 个子任务，自动串联
        │                  │
        ▼                  │
  ③ 创建 TaskPlan         │
     (nodes + edges + policy)
        │                  │
        ▼                  │
  ④ 校验 DAG              │  检查：ID 唯一、引用合法、无环
     (plan.validate())    │
        │                  │
        ▼                  │
  ⑤ 注入 context          │  executor.set_main_context(context)
     到 executor          │  → 写入 main_llm.md
        │                  │
        ▼                  │
  ⑥ 执行                  │
     (scheduler.execute)  │
        └──────────────────┘
```

### 2.2 main_llm.md 生成

每个 sub-agent 启动前，executor 会生成/更新 `.dispatch/main_llm.md`。

**代码位置**：`crates/tyclaw-orchestration/src/multi_model/executor.rs:282-428`

```
┌─── .dispatch/main_llm.md ─────────────────────────────────────┐
│                                                                │
│  # Main LLM Context                                           │
│                                                                │
│  ## Key Context from Main LLM          ← dispatch 的 context   │
│  **重要发现、约束、洞察...**              参数写入这里             │
│  （主 LLM 通过 context 字段传递的所有                             │
│    关键信息，sub-agent 最先看到这部分）                            │
│                                                                │
│  ## Current Task                       ← 当前 node.prompt      │
│  编写 tool.py 文件...                                           │
│                                                                │
│  ## Working Directory                  ← 绝对路径               │
│  `/Users/.../rust_edition`                                     │
│                                                                │
│  ## Project Structure                  ← 自动扫描目录树          │
│  ```                                     跳过 target/crates 等  │
│    dir   _personal                                             │
│    dir   agent_loop_cases                                      │
│    file  AGENTS.md                                             │
│  ```                                                           │
│                                                                │
│  ## User Skills                        ← _personal/ 下的 Skill  │
│  ### _personal/default/cli_user/ltv-calculator                 │
│  ├── tool.py                                                   │
│  └── SKILL.md                                                  │
│                                                                │
│  ## agent_loop_cases/                  ← 数据文件目录            │
│  ```                                                           │
│    1.xlsx (2.1MB)                                              │
│    2.xlsx (48.2KB)                                             │
│  ```                                                           │
│                                                                │
└────────────────────────────────────────────────────────────────┘
```

### 2.3 DAG 调度

**代码位置**：`crates/tyclaw-orchestration/src/multi_model/scheduler.rs:39-292`

```
TaskPlan: write_code → write_doc

调度过程：
  Batch 1: [write_code]  ← 无依赖，立即执行
     │
     ▼ 完成后
  Batch 2: [write_doc]   ← 依赖 write_code，拿到其输出

并发控制：
  - max_concurrency (默认 4) 个节点同时执行
  - Tokio Semaphore 限流
  - 每节点独立超时 (default_timeout_ms)

失败策略：
  - fail_fast: 任一失败 → 跳过剩余
  - best_effort: 继续执行不受影响的节点
```

---

## 第三部分：Sub-Agent 的提示词拼装

### 3.1 System Prompt 结构

每个 sub-agent 的 system prompt 由三部分拼接：

**代码位置**：`executor.rs:440-446` + `executor.rs:449-465`

```
┌─── sub-agent system prompt ────────────────────────────────────┐
│                                                                │
│  [Part 1: 角色提示词]      ← config/prompts/node_types/{type}.md│
│  You are an expert software engineer...                        │
│  **Workflow (MUST follow this order)**:                         │
│  1. Read main_llm.md                                           │
│  2. Plan in your response text                                 │
│  3. Write the COMPLETE script in ONE write_file call            │
│  4. Run with real data                                         │
│  5. If errors, use edit_file...                                │
│                                                                │
│  ─────────────────────────────────────────                     │
│                                                                │
│  [Part 2: 共享工作规范]    ← config/prompts/sub_agent_guidelines.md│
│  ## Working Principles                                         │
│  - Efficiency first: max {max_iterations} iterations           │
│  - Read before write                                           │
│  ## Verification (CRITICAL)                                    │
│  - py_compile is NOT enough                                    │
│  - Must run with real data                                     │
│  ## When to STOP investigating                                 │
│  - Logic bug → fix; Data-source diff → report and stop         │
│  - Max 2 investigation scripts                                 │
│                                                                │
│  ─────────────────────────────────────────                     │
│                                                                │
│  [Part 3: 工作区提示]     ← config/prompts/workspace_hint.md    │
│  ## Workspace                                                  │
│  Working directory: `/Users/.../rust_edition`                  │
│  Read `.dispatch/main_llm.md` FIRST                            │
│  Use full paths when writing files                             │
│                                                                │
└────────────────────────────────────────────────────────────────┘
```

### 3.2 User Prompt 结构

**代码位置**：`executor.rs:463-490`

```
┌─── sub-agent user prompt ──────────────────────────────────────┐
│                                                                │
│  [上游输出注入 - 仅当有依赖时]                                    │
│  === Context from upstream tasks ===                           │
│  [write_code] (full output also at `.dispatch/write_code.md`): │
│  {上游输出前 2000 字符预览}                                      │
│  ... (truncated, use read_file for full content)               │
│  === End of context ===                                        │
│                                                                │
│  [当前任务指令]                                                  │
│  {node.prompt 的完整内容}                                       │
│                                                                │
│  [验收标准 - 可选]                                               │
│  Acceptance criteria: {criteria}                               │
│                                                                │
└────────────────────────────────────────────────────────────────┘
```

### 3.3 Sub-Agent 工具集

**代码位置**：`executor.rs:239-253`

| 工具 | 说明 |
|------|------|
| `read_file` | 读取文件 |
| `write_file` | 写入文件（创建新文件）|
| `edit_file` | 编辑文件（局部修改）|
| `list_dir` | 列出目录 |
| `exec` | 执行命令 |

**不包含的工具**：
- `ask_user` — sub-agent 不能暂停等用户输入
- `send_file` — 文件发送由主 LLM 统一管理
- `dispatch_subtasks` — 不支持嵌套 dispatch

### 3.4 Sub-Agent 执行参数

| 参数 | 值 | 来源 |
|------|---|------|
| max_iterations | 40 | `SUB_AGENT_MAX_ITERATIONS` |
| max_output_chars | 60,000 | `SUB_AGENT_MAX_OUTPUT_CHARS` |
| model | 按 routing 路由 | config.yaml routing_rules |
| RBAC gate | 无 | sub-agent 不做权限检查 |
| write_snapshot | false | sub-agent 不写快照 |

### 3.5 完整的 Sub-Agent 消息序列示例

以 `node_type="coding"` 为例：

```json
[
  {
    "role": "system",
    "content": "You are an expert software engineer...\n\n**Workflow**:\n1. Read main_llm.md...\n\n## Working Principles\n- Efficiency first...\n\n## Workspace\nWorking directory: `/Users/.../rust_edition`\nRead `.dispatch/main_llm.md` FIRST..."
  },
  {
    "role": "user",
    "content": "=== Context from upstream tasks ===\n[analyze_data] (full output also at `.dispatch/analyze_data.md`):\n分析结果：数据包含9219行...\n=== End of context ===\n\n请编写 tool.py 文件，实现 LTV 计算...\n\nAcceptance criteria: 输出文件与 2.xlsx 格式一致"
  }
]
```

---

## 第四部分：结果回流

### 4.1 Sub-Agent 输出

每个 sub-agent 完成后：
1. 输出文本写入 `.dispatch/{node_id}.md`
2. `ExecutionRecord` 返回给 scheduler

**代码位置**：`executor.rs:189-196`

### 4.2 结果归并

**代码位置**：`crates/tyclaw-orchestration/src/multi_model/reducer.rs:52-129`

```
多个 sub-agent 输出
        │
        ▼
  ① 按拓扑序拼接
  ② 去重（hash 判断）
  ③ 检测冲突（关键词对：yes/no, true/false 等）
        │
    ┌───┴───┐
    │       │
  无冲突  有冲突
    │       │
    ▼       ▼
  直接     调用 reducer_model
  拼接     做语义归并
            │
            ▼
      归并提示词：config/prompts/reducer_prompt.md
```

### 4.3 返回主 LLM 的格式

**代码位置**：`tool.rs:388-500`

```
✅ **write_code** (89s, 9 tools): 代码已写完，验证通过...
   Detail: `./.dispatch/write_code.md`

✅ **write_doc** (23s, 2 tools): SKILL.md 已创建...
   Detail: `./.dispatch/write_doc.md`

---
Stats: 2 succeeded / 0 failed / 0 skipped | wall time 112.0s
Use `read_file` on the detail paths above if you need the full output.
```

主 LLM 可以：
- 直接使用摘要继续推理
- `read_file .dispatch/{id}.md` 获取完整输出
- 如果有失败，决定重试或自己修复

---

## 第五部分：配置模板一览

### 模板文件结构

```
config/prompts/
├── README.md                       # 使用说明
├── sub_agent_guidelines.md         # 所有 sub-agent 共享规范
│   变量: {max_iterations}
│
├── workspace_hint.md               # 工作区路径提示
│   变量: {workspace}, {context_file}
│
├── dispatch_tool_description.md    # dispatch 工具描述（给主 LLM）
│   变量: {routing_table}, {max_concurrency}
│
├── reducer_prompt.md               # 结果归并提示词
│   变量: 无
│
├── node_types/                     # 按任务类型的角色提示词
│   ├── coding.md                   # coding / coding_deep
│   ├── reasoning.md                # reasoning / analysis
│   ├── search.md                   # search / research
│   ├── summarize.md                # summarize / synthesis
│   ├── review.md                   # review / critique
│   ├── planning.md                 # planning / design
│   └── default.md                  # 兜底
│
└── nudges/                         # ReAct 循环中的催促文本
    ├── plan_required.md            # 前 2 轮没规划就调工具
    ├── repeated_tool_batch.md      # 重复工具调用
    ├── explore_over_limit.md       # 探索超限
    ├── explore_halfway.md          # 探索半程提醒
    ├── explore_hard_block.md       # 探索硬封（拦截 exec）
    ├── consecutive_exec_in_produce.md  # 产出阶段连续 exec
    └── no_write_tool.md            # 主 LLM 想写文件但没工具
```

### 加载优先级

```
文件存在 → 读文件内容
文件不存在 → 使用 include_str!() 编译时嵌入的默认值
```

即使 `config/prompts/` 目录被删除，系统也能正常运行（使用内置默认值）。

### 如何添加新的 node_type

```bash
# 1. 创建提示词文件
echo "You are a data analyst..." > config/prompts/node_types/data_analysis.md

# 2. 在 config.yaml 添加路由规则
routing_rules:
  - node_type_pattern: "data_analysis"
    target_model: "claude-opus"

# 3. 主 LLM dispatch 时使用新类型
dispatch_subtasks({
  subtasks: [{id: "analyze", node_type: "data_analysis", prompt: "..."}]
})
```

无需修改任何 Rust 代码，无需重新编译。

---

## 第六部分：Nudge 催促系统

### 6.1 概述

Nudge 是 agent loop 在特定条件下自动注入到消息列表中的 `system` 催促文本，用于纠正 LLM 的行为偏差（如过度探索、忘记写文件、重复调用相同工具等）。

所有 nudge 文本已外置到 `config/prompts/nudges/` 目录，修改无需重新编译。

**代码位置**：
- 加载器：`crates/tyclaw-agent/src/nudge_loader.rs`
- 触发逻辑：`crates/tyclaw-agent/src/agent_loop.rs`、`agent/phase.rs`、`agent/tool_runner.rs`
- 初始化：`crates/tyclaw-orchestration/src/builder.rs:build()` 中调用 `nudge_loader::init()`

### 6.2 Nudge 清单

```
config/prompts/nudges/
├── plan_required.md            # ① 规划催促
├── repeated_tool_batch.md      # ② 重复工具催促
├── explore_over_limit.md       # ③ 探索超限催促
├── explore_halfway.md          # ④ 探索半程催促
├── explore_hard_block.md       # ⑤ 探索硬封（拦截 exec）
├── consecutive_exec_in_produce.md  # ⑥ 产出阶段连续 exec 催促
└── no_write_tool.md            # ⑦ 无 write_file 工具催促
```

### 6.3 各 Nudge 的触发条件与流程位置

```
ReAct 循环每轮迭代
│
├─ LLM 返回 tool_calls？
│  │
│  ├─ YES（有工具调用）
│  │  │
│  │  ├─① 前 2 轮没输出计划就调工具？
│  │  │  → 注入 plan_required.md
│  │  │  → 不执行本轮工具，让 LLM 重新回答
│  │  │
│  │  ├── 执行工具
│  │  │
│  │  ├─② 连续相同 tool batch + 没写过文件？
│  │  │  → 注入 repeated_tool_batch.md
│  │  │
│  │  ├─ 探索阶段？
│  │  │  ├─④ 到了 explore_max/2？
│  │  │  │  → 注入 explore_halfway.md
│  │  │  ├─③ 超过 explore_max？（连续 3 轮）
│  │  │  │  → 注入 explore_over_limit.md
│  │  │  └─⑤ 超过 explore_max+3？
│  │  │     → exec 被拦截，返回 explore_hard_block.md
│  │  │
│  │  └─ 产出阶段？
│  │     └─⑥ 连续 N 次 exec 没写文件？
│  │        → 注入 consecutive_exec_in_produce.md
│  │
│  └─ NO（最终回复，无工具调用）
│     │
│     └─⑦ 主 LLM 想写文件但没有 write_file 工具？
│        → 注入 no_write_tool.md
│        → 继续循环（不退出）
│
└─ 循环结束
```

### 6.4 各 Nudge 详细说明

| # | 文件名 | 触发条件 | 模板变量 | 触发位置 |
|---|--------|---------|---------|---------|
| ① | `plan_required.md` | 前 2 轮 LLM 只调工具不输出计划文本 | 无 | `agent_loop.rs` |
| ② | `repeated_tool_batch.md` | 连续 2 次完全相同的工具调用组合，且从未使用 write_file | 无 | `agent_loop.rs` |
| ③ | `explore_over_limit.md` | 探索轮次超过 explore_max（连续最多触发 3 次） | `{exploration_iterations}`, `{explore_max}` | `agent/phase.rs` |
| ④ | `explore_halfway.md` | 探索轮次到达 explore_max 的一半 | `{exploration_iterations}`, `{explore_max}` | `agent/phase.rs` |
| ⑤ | `explore_hard_block.md` | 超过 explore_max+3 轮后仍在探索，exec 被直接拦截 | 无 | `agent/tool_runner.rs` |
| ⑥ | `consecutive_exec_in_produce.md` | 产出阶段连续 N 次 exec 没有 write_file/edit_file | `{consecutive_exec_in_produce}` | `agent/phase.rs` |
| ⑦ | `no_write_tool.md` | 主 LLM 的最终回复包含"写文件"意图但没有 write_file 工具 | 无 | `agent_loop.rs` |

### 6.5 Nudge 与主 LLM / Sub-Agent 的关系

所有 nudge 都在 `AgentLoop` 的 ReAct 循环内触发，**对主 LLM 和 sub-agent 都生效**（因为两者共享同一个 AgentLoop 引擎）。

区别：
- **主 LLM**：⑦（no_write_tool）在 `full_tools=false` 时有意义
- **Sub-Agent**：⑤（explore_hard_block）对 coding 类 sub-agent 最常触发，因为它们倾向于过度探索

---

## 第七部分：主 LLM vs Sub-Agent 对比

| 维度 | 主 LLM | Sub-Agent |
|------|--------|-----------|
| **System Prompt 来源** | workspace/*.md (IDENTITY, AGENTS, GUIDELINES...) | config/prompts/node_types/{type}.md + sub_agent_guidelines.md |
| **提示词大小** | ~29K chars | ~3-5K chars |
| **工具集** | 7-8 个（含 dispatch_subtasks） | 5 个（无 dispatch/ask_user/send_file）|
| **最大轮次** | config.yaml max_iterations (默认 99) | SUB_AGENT_MAX_ITERATIONS (40) |
| **输出限制** | 无 | 60K chars |
| **上下文来源** | 用户消息 + 历史 + Skill + 案例 | main_llm.md + 上游输出 + node.prompt |
| **工作区感知** | 完整（GUIDELINES.md 等） | 仅 main_llm.md（不加载 workspace/*.md）|
| **RBAC 权限** | 有（ExecutionGate） | 无 |
| **可嵌套 dispatch** | 是 | 否 |
