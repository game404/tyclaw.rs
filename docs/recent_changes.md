# 近期改动记录

## 1. `remember` 工具 —— LLM 主动知识管理

### 背景

Agent 在长会话中会丢失早期学到的结构性知识（行布局、公式逻辑、数据格式等），导致验证阶段反复重新探索。

曾尝试自动从 assistant 消息中提取 facts（`extract_conclusions`），但效果极差：
- 提取过于激进，把中间假设甚至错误结论也存入
- 矛盾信息同时呈现给 LLM，干扰决策
- `iter113` 跑到 max 都没完成，宣告失败

### 方案

改为让 LLM **自己决定** 存什么：新增 `remember` 工具。

### 涉及文件

| 文件 | 改动 |
|------|------|
| `crates/tyclaw-tools/src/remember.rs` | 新建工具定义 |
| `crates/tyclaw-tools/src/lib.rs` | 注册 `pub mod remember` |
| `crates/tyclaw-orchestration/src/orchestrator.rs` | `register_default_tools` 中注册 `RememberTool` |
| `crates/tyclaw-agent/src/agent_loop.rs` | 拦截 `remember` 调用，执行 `ctx_state.replace_all_facts` |
| `crates/tyclaw-agent/src/context_state.rs` | 新增 `replace_all_facts` 方法；移除自动提取逻辑 |
| `GUIDELINES.md` | 新增 `#5.1 remember — 知识记忆` 使用指南 |

### 工作机制

1. LLM 调用 `remember({ facts: ["...", "..."] })`
2. Agent loop 拦截，调用 `ctx_state.replace_all_facts(facts, iteration)`
3. 旧 facts 全部清除，替换为新列表
4. 下一轮 STATE SNAPSHOT 中展示这些 facts，LLM 可持续看到

### 设计要点

- **全量替换**：每次调用传入完整列表，避免增量追加导致过期信息累积
- **LLM 自主决策**：只存已验证结论，不存猜测；由 LLM 判断什么值得记住
- 每条 fact 截断上限 500 字符，总容量 32 条，渲染展示 16 条

---

## 2. `startup_snapshot` —— 快速调试跳过探索

### 背景

每次调试都要重跑完整探索阶段（15+ 轮），耗时且浪费 token。需要一种机制从上次产出点直接恢复。

### 方案

如果 `config/startup_snapshot.json` 存在，程序自动加载并从该点继续。

### 涉及文件

| 文件 | 改动 |
|------|------|
| `crates/tyclaw-agent/src/agent_loop.rs` | 启动时检测并加载 snapshot，恢复 phase/轮次 |
| `crates/tyclaw-channel/src/cli.rs` | 检测到 snapshot 后自动发送触发消息，无需手动输入 |

### Snapshot 文件格式

与 `sessions/context_snapshot_iterN.json` 完全一致：

```json
{
  "iteration": 15,
  "phase": "produce",
  "phase_iter": 2,
  "model": "...",
  "messages": [ ... ],
  "tools": [ ... ]
}
```

### 工作流

1. CLI 启动 → 检测 `config/startup_snapshot.json` 存在
2. 自动注入触发消息 `[从 snapshot 继续]`，无需用户手动输入
3. `AgentLoop::run()` 加载 snapshot：
   - 解析 `messages` 字段作为消息历史
   - 从元数据直接读取 `iteration`、`phase`、`phase_iter`，设置正确的阶段和轮次
   - 跳过 `take_reset_marker`（snapshot 自带完整状态）
   - `has_plan = true`（已有计划，不重新规划）
4. 直接进入产出/验证阶段

### 使用方法

```bash
# 把某次运行的 snapshot 复制过来
cp sessions/context_snapshot_iter15.json config/startup_snapshot.json

# 直接启动，自动从 iter15 继续
cargo run

# 调试完毕后删除，恢复正常模式
rm config/startup_snapshot.json
```

---

## 3. Assistant 消息两级压缩

### 机制

为控制上下文膨胀，对历史 assistant 消息的纯文本内容（不含 tool_calls）进行压缩：

| 范围 | 策略 |
|------|------|
| 最近 5 条 | 超过 700 字符 → 保留前 700 字符 |
| 更早的 | 保留前 180 字符 + 后 100 字符 |

`tool_calls` 内容不压缩，保证工具调用链完整。

### 涉及文件

- `crates/tyclaw-agent/src/agent_loop.rs` — `maybe_compact_assistant_history`

---

## 4. Facts 基础设施

虽然自动提取已废弃，但 facts 基础结构保留给 `remember` 工具使用：

- `Fact` 结构体：`{ id, text, source, iteration }`
- `FactSource` 枚举：`Init` / `AssistantConclusion` / `FinalAnswer`
- 容量：最多 32 条存储，渲染展示 16 条（按最新优先）
- 每条带 `@iterN` 标记，便于 LLM 判断时效性

---

## 5. 已回撤的实验

以下方案经测试效果不佳，已全部回撤：

| 实验 | 问题 |
|------|------|
| 自动 facts 提取（`extract_conclusions`）| 提取质量低，存入矛盾信息，iter113 跑满未完成 |
| 增大 `MAX_TOOL_OUTPUT_CHARS` (4000→5000) | 效果更差 |
| 增大 `MAX_TOOL_OUTPUT_CHARS_EXPLORE` (3500→6000) | 效果更差 |
| 验证阶段特定指导文字（GUIDELINES 中写死验证脚本策略）| 反而限制了 LLM 灵活性 |
| 产出阶段 nudge 中加入具体对比脚本建议 | 效果更差 |
| `TOOL_RESULT_FRESH_COUNT` 18（从 6 调高）| 上下文膨胀加剧 |
| 全局 system nudge 去重 + assistant tool_calls 内容压缩 | 探索阶段延长，信息丢失加重 |
remember 工具本身不是问题根因——它只用了 2 轮（iter6, iter14），facts 也正确存了。但它也没帮上忙，因为：

remember 的 tool result 被压缩成了 [tool: remember, 61 chars, ok]，LLM 没收到确认反馈
生产阶段 LLM 再也没调用过 remember，facts 停留在 iter14 的状态
最关键：生产阶段进入了 3 次完全重写的死循环
第 1 次 write_file (msg#34) → 验证发现问题 → 31 次 exec 逐个排查
第 2 次 write_file (msg#139) → 又发现问题 → 继续验证
第 3 次 write_file (msg#155) → 还有问题 → 16 次 edit_file + exec 直到 max
根因是一样的：LLM 产出的代码质量不够，每次验证发现差异，逐个 cell 修修补补，越改越复杂。remember 工具对这个问题帮不上忙——知识记住了，代码还是写不对。

建议：回撤 remember 工具（因为它没带来正面效果，反而增加了 STATE SNAPSHOT 体积 2600 chars），把精力集中到其他方向。要不要回撤？

