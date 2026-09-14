# Skill 执行超时配置权威化设计

**日期：** 2026-09-14
**状态：** 已实施，待集成
**适用仓库：** `tyclaw.rs`

## 1. 摘要

已识别 Skill 的执行期限必须完全由管理员维护的 `skill_execution` 配置决定。LLM 在 `exec` 工具参数中提供的 `timeout` 只保留为审计信息，不得缩短或延长 Skill 的执行期限。未识别为 Skill 的普通 `exec` 继续采用现有调用参数和工具默认值。用户主动停止、Timer 取消和上层任务取消仍可随时提前终止执行，不受 Skill 超时配置约束。

本设计修正 2026-08-25 配置化方案中“配置是上限、调用方可缩短”的实际实现语义。历史设计文档保留不改，本文件是后续实现与验收的当前依据。

## 2. 背景与根因

当前 `SkillExecutionConfig::resolve_in_workspace` 使用以下规则：

```rust
effective_timeout = min(requested_timeout, configured_timeout)
```

两类输入的信任等级不同：

- `requested_timeout` 来自 LLM 生成的 `exec` 工具参数，是非确定性、不可信的运行请求；
- `configured_timeout` 来自管理员维护的 `config.yaml`，是可信的资源治理策略。

运行日志显示，某长时 Skill 的管理员配置大于 LLM 显式请求值。由于当前代码取两者较小值，实际期限被缩短；正常完成的历史任务已经接近该意外期限，形成稳定、可复现的临界超时风险。

业务处理规模和具体运行数据不属于本设计文档范围。可确认的技术根因是执行策略允许低信任的 LLM 参数缩短高信任的管理员配置。此前的专用执行路径采用固定期限；迁移到通用 Skill 策略后新增了“调用方可缩短”的行为，形成语义退化。

## 3. 目标与非目标

### 3.1 目标

1. 管理员配置稳定且唯一地决定已识别 Skill 的执行期限。
2. LLM 无法通过 `exec.timeout` 缩短或延长管理员配置。
3. 主 Agent、子 Agent、Timer、手动任务、CLI、Docker 和无 Docker 回退路径采用相同规则。
4. 主动取消继续即时生效，并与超时保持不同的结果语义。
5. 普通非 Skill 命令保持现有超时行为，控制兼容性影响。
6. 不修改正式配置结构，不要求数据或配置迁移。
7. 日志完整展示配置值、请求值和最终值，避免再次误判。
8. 超时和取消后继续可靠终止完整进程组，不留下脱离进程。

### 3.2 非目标

- 不优化具体 Skill 的业务算法、外部接口调用或文件生成性能；
- 不提高 `--workers` 或 `--voucher-workers`；
- 不调整 Timer 周期、会话空闲回收或 LLM provider 超时；
- 不新增动态配置热加载；
- 不允许 LLM 通过新的公共工具参数覆盖 Skill 策略；
- 不在本次改动中建设完整指标平台或 Timer 重入治理。

## 4. 方案比较

### 4.1 方案 A：取最大值

```rust
effective_timeout = max(requested_timeout, configured_timeout)
```

该方案能阻止 LLM 缩短执行期限，但会允许 LLM 将配置值延长为任意更大值。管理员无法限制挂起任务的最长资源占用，配置从上限变成下限，安全边界反转。

**结论：不采用。**

### 4.2 方案 B：配置权威

```rust
effective_timeout = configured_timeout
```

已识别 Skill 完全采用管理员配置。LLM 请求值只记录、不生效；普通命令维持原规则。该方案信任边界明确，不增加配置复杂度，并恢复旧版专用执行路径的确定性行为。

**结论：采用。**

### 4.3 方案 C：上下界夹取

为每个 Skill 增加默认、最小和最大超时，再对调用值执行 `clamp`。该方案可以支持可信程序调用方临时调整，但会增加配置校验、迁移和运维复杂度，且当前没有此类明确需求。LLM 仍能影响实际期限。

**结论：本次不采用；未来出现可信调用方需求时独立设计。**

## 5. 权威规则与行为矩阵

执行边界采用以下规则：

```rust
if command_is_recognized_skill {
    effective_timeout = configured_skill_timeout;
} else {
    effective_timeout = requested_timeout.unwrap_or(exec_default_timeout);
}
```

| 命令类型 | 配置值 | 请求值 | 最终值 |
|---|---:|---:|---:|
| 已识别 Skill | 1200 | 未传 | 1200 |
| 已识别 Skill | 1200 | 0 | 1200 |
| 已识别 Skill | 1200 | 600 | 1200 |
| 已识别 Skill | 1200 | 1200 | 1200 |
| 已识别 Skill | 1200 | 3600 | 1200 |
| 未单独配置的 Skill | 默认 900 | 600 | 900 |
| 普通命令 | 不适用 | 未传 | Exec 默认值 |
| 普通命令 | 不适用 | 600 | 600 |

信任与控制关系为：

```text
管理员配置 ──决定──> Skill 执行期限
LLM timeout ──仅审计──> 不改变 Skill 执行期限
用户或系统取消 ──立即终止──> 不受执行期限影响
```

## 6. 数据模型与接口

保留现有 `ResolvedSkillExecution.timeout_secs` 字段作为最终生效值，避免无必要的公共接口破坏；新增决策元数据：

```rust
pub struct ResolvedSkillExecution {
    pub skill_name: String,
    pub timeout_secs: u64,
    pub configured_timeout_secs: u64,
    pub requested_timeout_secs: Option<u64>,
    pub source: &'static str,
}
```

字段语义：

- `timeout_secs`：最终传入 host 或 sandbox 的执行期限；
- `configured_timeout_secs`：专属配置或默认配置解析出的权威值；
- `requested_timeout_secs`：LLM 工具参数，正整数原样记录，`0` 归一化为 `None`；
- `source`：继续表示配置来自 Skill 专属 `override` 还是全局 `default`，不表示请求参数来源。

解析逻辑为：

```rust
let configured_timeout_secs = self.timeout_for(&skill_name);
let requested_timeout_secs = requested_timeout.filter(|value| *value > 0);

Some(ResolvedSkillExecution {
    skill_name,
    timeout_secs: configured_timeout_secs,
    configured_timeout_secs,
    requested_timeout_secs,
    source: self.source_for(&skill_name),
})
```

`exec.timeout` 的工具描述调整为：

> 普通命令的执行超时，单位为秒。已识别 Skill 的执行期限由管理员 `skill_execution` 配置决定，此参数对 Skill 不生效。

提示词或参数描述只负责减少无意义参数，系统正确性必须由工具边界强制保证。

## 7. 配置语义

配置结构保持不变：

```yaml
skill_execution:
  default_timeout_secs: 900
  skills:
    example-long-task:
      timeout_secs: 1200
    example-short-task:
      timeout_secs: 300
```

新语义说明：

- `default_timeout_secs` 是未单独配置 Skill 的权威执行期限；
- `skills.<name>.timeout_secs` 是对应 Skill 的权威执行期限；
- `exec.timeout` 不得缩短或延长上述值；
- 用户或系统主动取消仍可立即终止；
- `0` 值继续按现有兼容逻辑回退到有效默认值，本次不改变配置解析方式；
- 配置在应用启动时加载，部署新代码或修改配置后需要重启。

示例配置注释应明确：

```yaml
# 已识别 Skill 的权威执行期限。exec 工具调用中的 timeout 不会
# 缩短或延长该值；用户或系统主动取消仍可立即终止执行。
```

## 8. 执行、取消与清理

本次只改变超时决策，不改变执行和取消链路。

### 8.1 Host 路径

`ExecTool::execute` 继续在命令输出、deadline 和 CancellationToken 之间 `tokio::select!`。超时或取消时继续先向进程组发送 `TERM`，等待现有宽限期后发送 `KILL`。

### 8.2 Sandbox 路径

`ExecTool::execute_in_sandbox` 把权威期限和 CancellationToken 传给 `SandboxExecContext`。`DockerSandbox` 与 `NoopSandbox` 继续区分 `Timeout` 和 `Cancelled`，执行 run-id、marker 和残留进程清理。

### 8.3 结果语义

超时与取消必须保持不同错误码：

```text
Error: code=timeout Skill 'example-long-task' 执行超时（1200 秒）
Error: code=cancelled Command cancelled
```

配置权威不意味着任务必须运行到期限；用户停止按钮、停止指令、Timer 取消和上层 CancellationToken 均可提前结束。

### 8.4 前台执行约束

继续拒绝已识别 Skill 使用 `setsid`、`nohup`、后台 `&`、`sleep`、`ps` 或 `tail` 轮询。该约束是取消和进程清理可靠性的必要条件，不因本次超时语义调整而放宽。

## 9. 主 Agent、子任务与 Timer 一致性

`SkillExecutionConfig` 已通过 `BaseConfig`、`RunConfig`、`OrchestratorBuilder` 和 `AppContext` 注入核心 ToolRegistry，主 Agent 和子任务使用相同配置。Timer 通过同一 `ExecTool` 执行并携带 `timer_job_id`，但 job ID 不参与超时计算。

本设计要求：

- 主 Agent 与子任务对同一 Skill 得到相同执行期限；
- Timer 与手动任务对同一 Skill 得到相同执行期限；
- host 与 sandbox 路径使用同一个 `ResolvedSkillExecution`；
- 不恢复按 Timer job ID 硬编码超时的旧实现。

## 10. 可观测性

识别到 Skill 时记录一条结构化 `INFO` 日志：

```text
Applying skill execution policy
skill_name=example-long-task
policy_source=override
configured_timeout_secs=1200
requested_timeout_secs=600
effective_timeout_secs=1200
requested_timeout_ignored=true
```

规则：

- 未传或传 `0` 时，`requested_timeout_secs` 记录为空；
- 传入任意正整数时，`requested_timeout_ignored` 均为 `true`；即使请求值恰好等于配置值，它仍未参与决策；
- 请求值与配置值不同不升级为 `WARN`，避免模型持续传参产生告警噪声；
- 真正超时或取消继续记录 `WARN`；
- 不记录完整命令、业务参数、环境变量或凭证。

建议后续独立建设以下指标，本次首版不以指标平台为交付前提：

- 按 Skill 的执行次数与耗时分布；
- 超时数与主动取消数；
- 被忽略请求超时的次数；
- `duration / configured_timeout` 比例。

比例建议分级：小于 0.70 正常，0.70 至 0.85 关注，0.85 至 0.95 预警，大于 0.95 高风险。

## 11. 兼容性与迁移

### 11.1 保持兼容

- `config.yaml` 结构和键名不变；
- `timeout_secs` 单位仍为秒；
- 未配置 `skill_execution` 时仍使用内建 Skill 默认值 `1800`；
- 普通非 Skill `exec` 行为不变；
- 超时错误继续包含 Skill 名称和最终期限；
- 现有取消、进程回收和 detached process 检测保持不变。

### 11.2 有意行为变化

现有调用方如果依赖“为 Skill 传入更短的 `timeout`”，升级后不再生效。这是本次安全边界修正，而不是需要保留的兼容行为。

开发和测试若需要快速终止：

- 单元测试构造较短的 `SkillExecutionConfig`；
- 人工运行使用现有取消机制；
- 不使用 LLM 的普通工具参数充当可信策略覆盖。

未来若确有可信程序调用方需要临时覆盖，应设计与 LLM 工具参数隔离、经过鉴权的内部接口，并单独评审。

## 12. 风险评估与控制

### 12.1 Skill 实际运行时间变长

**概率：高；影响：低至中。** 过去被 LLM 缩短的任务会恢复到管理员配置期限。

控制：上线前审查全部 Skill 配置；主动取消继续生效；保持进程组终止与清理。

### 12.2 管理员误配导致长期资源占用

**概率：低；影响：高。** 例如误配为 `86400` 秒会允许挂起任务长期运行。

控制：配置由管理员评审；部署前输出非敏感策略摘要；后续可独立增加全局合理性校验。本次不新增硬编码最大值，避免破坏已有合法长任务。

### 12.3 失去调用方短超时诊断能力

**概率：中；影响：低。** 开发人员不能再通过 `exec.timeout=5` 缩短真实 Skill。

控制：测试使用短配置；人工诊断使用取消；可信覆盖需求另建受控接口。

### 12.4 Skill 误识别

**概率：低；影响：中。** 普通脚本若被误识别，会套用 Skill 配置。

控制：保持受信任路径、词法解析、拒绝 `..`、不识别 `echo` 和 `python -c` 等现有约束；增加策略日志便于追踪。

### 12.5 取消路径回归

**概率：低；影响：高。** 如果实现时把 timeout 与 CancellationToken 耦合，用户可能无法停止长任务。

控制：不修改取消传递；为 host、NoopSandbox 和 DockerSandbox 增加或保留配置较长但立即取消的测试。

### 12.6 进程残留

**概率：低；影响：高。** 更长期限会让异常进程存活更久，终止失败可能留下子进程。

控制：继续使用进程组 `TERM/KILL`、Docker marker/run-id 清理、后台执行拦截和 detached process 检测。

### 12.7 Timer 重叠

**概率：因任务而异；影响：中至高。** 若配置期限大于调度周期且缺少单实例约束，前后两次可能重叠。

控制：上线前盘点“调度周期小于对应 Skill timeout”的 Timer；如存在则独立设计单实例或跳过重入策略，不把 Timer 调度治理混入本次变更。

### 12.8 行为兼容性变化

**概率：确定；影响：中。** 显式短 timeout 的 Skill 调用不再按请求值结束。

控制：全仓搜索调用方式；更新参数说明和发布说明；普通命令保持不变。

### 12.9 日志误判

**概率：高；影响：低。** 单一的 `timeout_secs` 字段容易被误读为管理员配置值。

控制：拆分 configured/requested/effective 字段，不再用单一字段表达整个决策过程。

### 12.10 配置不热加载

**概率：中；影响：低。** 修改配置但未重启会继续使用旧值。

控制：部署步骤明确重启；启动和首次执行日志验证权威值。

### 12.11 超时掩盖业务挂起

**概率：低至中；影响：中。** 期限恢复为较长的管理员配置后，业务挂起会更晚暴露。

控制：超时是最终保护，不应代替阶段级监控；后续为长 Skill 增加阶段耗时与无进展检测，但不得通过恢复不可信短 timeout 解决。

### 12.12 回滚期间运行任务不一致

**概率：低；影响：中。** 发布或回滚时强行重启可能中断正在执行的 Skill。

控制：发布前检查活跃任务；等待结束或显式取消；不在运行中直接替换进程。

## 13. 测试设计

### 13.1 策略单元测试

对专属配置 `2700` 秒验证：

```rust
assert_eq!(resolve(None).timeout_secs, 2700);
assert_eq!(resolve(Some(0)).timeout_secs, 2700);
assert_eq!(resolve(Some(5)).timeout_secs, 2700);
assert_eq!(resolve(Some(2700)).timeout_secs, 2700);
assert_eq!(resolve(Some(9999)).timeout_secs, 2700);
```

同时断言请求值和配置值元数据。对未单独配置 Skill 验证始终采用 `default_timeout_secs`。

### 13.2 普通命令回归

```rust
assert_eq!(resolve_command_policy("echo ok", None), exec_default);
assert_eq!(resolve_command_policy("echo ok", Some(7)), 7);
```

### 13.3 执行路径

覆盖：

- host 和 sandbox 使用相同权威值；
- Skill 超时返回 `code=timeout` 和最终期限；
- 用户取消返回 `code=cancelled` 而非 timeout；
- 超时与取消后进程组均被终止；
- 主 Agent、子任务、Timer 和手动任务获得相同配置；
- 更短和更长请求值都不会改变 Skill 的最终值。

### 13.4 配置解析

覆盖现有正式配置结构、缺省配置、专属覆盖和 `0` 回退，确保 `config.example.yaml` 可完整反序列化。

### 13.5 验证命令

```bash
cargo fmt --all -- --check
cargo test -p tyclaw-tools
cargo test -p tyclaw-sandbox
cargo test -p tyclaw-orchestration
cargo check --workspace
cargo clippy --workspace --all-targets -- -D warnings
git diff --check
```

Docker 测试依赖 Docker daemon 和 `tyclaw-sandbox:latest` 镜像。环境不具备时必须记录未执行原因。不得为验证本策略调用真实生产接口。

## 14. 实施范围

预计修改：

- `crates/tyclaw-tools/src/skill_execution.rs`：权威决策、元数据和单元测试；
- `crates/tyclaw-tools/src/shell.rs`：参数说明、结构化日志及 host/sandbox 回归测试；
- `workspace/config/config.example.yaml`：配置权威和取消边界说明；
- 必要的 README 或当前设计文档引用。

原则上无需修改：

- `tyclaw-tool-abi` 的取消接口；
- `tyclaw-sandbox` 的进程终止逻辑；
- Timer 调度协议；
- AgentLoop 取消流程；
- `financeskills` 业务脚本与真实配置。

若测试暴露取消或清理路径缺少可验证接口，应先评估是否属于本设计必需修复，不顺带重构无关模块。

## 15. 上线与观察

1. 合并代码、测试和文档，不修改业务 Skill。
2. 发布前审查正式 `skill_execution` 中全部期限。
3. 检查活跃任务，等待结束或显式取消后重启 TyClaw。
4. 使用离线测试 Skill 验证 `requested=5, configured=60, effective=60`。
5. 观察下一次目标 Skill：

   ```text
   configured_timeout_secs=<管理员配置值>
   requested_timeout_secs=<LLM 请求值或空>
   effective_timeout_secs=<管理员配置值>
   ```

6. 连续观察至少三次定时运行的耗时、超时、取消和残留进程情况。
7. 若 `duration/configured_timeout` 持续超过 0.85，另立性能治理任务。

## 16. 回滚

- 回滚应用版本即可恢复旧的 `min` 行为；
- 配置结构和正式配置无需回滚；
- 没有数据库、文件格式或持久化数据迁移；
- 发布或回滚前先处理运行中的任务，避免中途重启；
- 不通过临时缩短目标 Skill 配置模拟回滚，因为这会重新压缩所有调用的安全余量。

## 17. 验收标准

以下条件全部满足才算完成：

- 已识别 Skill 的最终期限始终等于管理员配置；
- 更短、更长、`0` 或缺省的 LLM timeout 均不能改变该期限；
- 普通非 Skill `exec` 的 timeout 行为不变；
- 主 Agent、子 Agent、Timer、手动调用、host 和 sandbox 行为一致；
- 用户取消仍能立即终止长 Skill；
- 超时和取消后没有残留进程；
- 日志同时包含 configured、requested 和 effective；
- 示例配置和工具说明明确权威语义；
- 目标测试、workspace check、clippy、格式和 diff 检查通过；
- 环境依赖测试若未执行，交付说明准确记录原因。

## 18. 后续独立事项

以下事项有价值，但不阻塞本设计实施：

1. Skill 执行耗时与接近上限比例指标；
2. 长 Skill 阶段级无进展检测；
3. Timer 单实例与跳过重入策略；
4. 管理员配置合理性上限或启动期严格校验；
5. 经过鉴权的可信临时覆盖接口。
