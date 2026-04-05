# Agent Loop 优化方案（基于 `agent_loop_cases/ltvskill/tyclaw.log`）

## 目标
- 减少无效上下文和重复探索，降低 `prompt_tokens`
- 缩短平均响应时长和首字延迟
- 在不改业务能力的前提下，提升任务完成效率

## 观测结论（本次日志）
- 总调用次数：41 次
- `prompt_tokens`：从 50750 增长到 101850
- 平均 prompt 规模约 7.8w tokens
- 最慢单次调用约 64s
- 探索阶段 16 轮后才进入产出阶段（`explore -> produce`）
- 工具输出多次超过 10k 字符，虽然有截断，但回灌成本仍高

## 主要问题
1. **历史预算过宽**
   - 当前默认 context window 为 200k，历史按 45% 预算，容易保留过多旧轮次。
2. **探索阶段过长**
   - 探索上限比例为 50%，导致模型长时间验证而不产出。
3. **工具结果注入过重**
   - `exec/read_file` 的大输出进入消息链，持续推高每轮 prompt。
4. **系统提示偏重**
   - 技能与能力注入在复杂会话中累积 token 压力。
5. **预算估算与模型侧不完全一致**
   - 本地 token 估算和实际计费模型编码存在偏差，容易低估真实开销。

## 优化优先级（先做高 ROI）

### P0（立即）
1. **历史硬上限**
   - 在比例预算外，增加绝对上限（建议 8k~16k tokens）。
   - 即使 context window 很大，也不允许历史无限膨胀。

2. **缩短探索预算**
   - `EXPLORE_MAX_RATIO_PERCENT` 从 50 降至 25~30。
   - 对“重复验证型工具调用”更早触发强制产出。

3. **工具结果摘要注入**
   - 对超长工具输出仅注入摘要（关键结论 + 统计 + 截断说明），原文仅保留日志/文件。

### P1（短期）
4. **系统提示分层注入**
   - 默认仅注入技能摘要（name/trigger/risk/tool_path），命中后再加载技能正文。
   - 能力列表按 query 命中度取 top-N。

5. **会话切换策略**
   - 任务类型明显变化时提示或自动新会话，减少跨任务历史污染。

### P2（中期）
6. **基于真实 usage 的预算校准**
   - 用每轮 `usage.prompt_tokens` 反向调整下一轮 history/cases/skills 预算。

7. **`max_tokens` 动态化**
   - 工具调用回合给较小 completion 预算，最终答案回合再放大。

8. **提示缓存（如果网关支持）**
   - 启用/适配 provider 的 prompt cache，复用稳定前缀。

## 建议改动点（代码位置）
- `crates/tyclaw-orchestration/src/orchestrator.rs`
  - 历史硬上限
  - 技能/能力注入分层
  - 动态预算策略
- `crates/tyclaw-agent/src/agent_loop.rs`
  - 探索比例下调
  - 重复验证熔断
  - 工具结果摘要化注入
- `crates/tyclaw-provider/src/openai_compat.rs`
  -（可选）调试日志采样/开关，避免大 payload 全量打印
- `crates/tyclaw-types/src/tokens.rs`
  -（可选）估算策略校准与观测指标统一

## 预期收益（保守）
- 平均 `prompt_tokens` 下降 30%~60%
- 复杂任务总耗时下降 20%~40%
- 进入产出阶段更早，减少“验证循环”

## 落地顺序
1. 历史硬上限 + 探索比例下调（最小改动、立竿见影）
2. 工具结果摘要注入（减少每轮回灌）
3. 系统提示分层（控制基础 prompt 成本）
4. 动态预算闭环（进一步稳定）
