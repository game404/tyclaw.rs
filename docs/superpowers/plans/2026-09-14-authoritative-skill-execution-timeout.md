# Skill 执行超时配置权威化 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 让已识别 Skill 的执行期限始终等于管理员 `skill_execution` 配置，LLM 提供的 `exec.timeout` 仅用于审计且不能改变最终期限。

**Architecture:** 保持现有 `BaseConfig → OrchestratorBuilder → ToolRegistry → ExecTool` 配置注入链路不变，在 `SkillExecutionConfig::resolve_in_workspace` 统一生成权威决策。`ExecTool` 的 host 和 sandbox 路径继续消费同一个 `ResolvedSkillExecution`，只扩充参数说明和结构化日志；普通非 Skill 命令、取消令牌及进程组清理保持现状。

**Tech Stack:** Rust 2021、Tokio、Serde、Tracing、Cargo test

**Spec:** `docs/superpowers/specs/2026-09-14-authoritative-skill-execution-timeout-design.md`

## Global Constraints

- 已识别 Skill 的最终期限只能来自 `skill_execution` 配置；不得使用 `min`、`max` 或其他方式让 LLM 请求值参与决策。
- `requested_timeout_secs` 只记录正整数；缺省和 `0` 均归一化为 `None`。
- 任意正整数请求均视为已忽略，即使它恰好等于配置值。
- 普通非 Skill `exec` 继续使用 `requested_timeout.unwrap_or(exec_default_timeout)`。
- 不修改正式 `workspace/config/config.yaml`、业务 Skill、Timer 协议、CancellationToken 或 sandbox 进程终止实现。
- 不调用真实生产外部服务进行验证。
- 保留工作区已有未提交内容，不格式化或修改本任务以外文件。

## 实施记录

2026-09-14 已在分支 `codex/authoritative-skill-timeout` 的隔离 worktree 中完成 Tasks 1–3 及 Task 4 的代码范围。核心策略测试经历预期红灯后转绿；`tyclaw-tools` 全量 187 项通过，示例配置解析通过，workspace 编译检查通过。

两项非本次代码原因的验证限制需在集成前知悉：

- `tyclaw-sandbox` 的 3 项纯 Rust 单元测试通过；11 项 Docker 集成测试在容器挂载初始化阶段失败，未进入被测逻辑。
- `tyclaw-orchestration` 148 项中 147 项通过；`test_scan_real_workspace_skills` 因隔离 worktree 不含运行时 `finance` Skill 目录而失败。
- `cargo clippy --workspace --all-targets -- -D warnings` 被既有 `tyclaw-prompt::PromptMode` 的 `derivable_impls` 告警拦截；定向 `tyclaw-tools` clippy 又被上游 `tyclaw-tool-abi` 的三个 `ptr_arg` 告警拦截，均与本次修改文件无关。
- 全仓 `cargo fmt --all` 会机械改写大量既有未格式化文件，`cargo fmt --all -- --check` 同样报告数十个基线文件差异；已恢复全部任务外格式化，只保留本计划列出的文件。

---

## File Map

- `crates/tyclaw-tools/src/skill_execution.rs`：Skill 身份解析、权威超时决策、决策元数据及策略单元测试。
- `crates/tyclaw-tools/src/shell.rs`：`exec.timeout` 接口说明、host/sandbox 共用的策略日志及工具边界回归测试。
- `workspace/config/config.example.yaml`：管理员配置的权威语义和主动取消边界。
- `docs/superpowers/specs/2026-09-14-authoritative-skill-execution-timeout-design.md`：已确认设计；实现时只在发现事实错误时修订。

无需修改 `tyclaw-tool-abi`、`tyclaw-sandbox`、`tyclaw-agent` 或 `tyclaw-orchestration`。这些模块已有配置注入、取消和进程清理实现，本计划只运行其相关回归测试。

---

### Task 1: 将 Skill 超时决策改为配置权威

**Files:**
- Modify: `crates/tyclaw-tools/src/skill_execution.rs:20-95`
- Test: `crates/tyclaw-tools/src/skill_execution.rs:532-551`

**Interfaces:**
- Consumes: `SkillExecutionConfig::timeout_for(&self, skill_name: &str) -> u64` 与 `source_for(&self, skill_name: &str) -> &'static str`。
- Produces: `ResolvedSkillExecution { skill_name, timeout_secs, configured_timeout_secs, requested_timeout_secs, source }`；`timeout_secs` 与 `configured_timeout_secs` 必须相等。

- [ ] **Step 1: 将现有 cap 测试改成权威配置失败测试**

把 `resolves_skill_timeout_as_a_cap` 重命名为 `configured_skill_timeout_is_authoritative`，并用下列断言替换原测试主体中的决策断言：

```rust
let cases = [
    (None, None),
    (Some(0), None),
    (Some(5), Some(5)),
    (Some(2700), Some(2700)),
    (Some(9999), Some(9999)),
];

for (requested, normalized_requested) in cases {
    let resolved = cfg.resolve(command, requested).unwrap();
    assert_eq!(resolved.skill_name, "example-skill");
    assert_eq!(resolved.timeout_secs, 2700);
    assert_eq!(resolved.configured_timeout_secs, 2700);
    assert_eq!(resolved.requested_timeout_secs, normalized_requested);
    assert_eq!(resolved.source, "override");
}
```

在同一测试中增加未单独配置 Skill 的断言：

```rust
let defaulted = cfg
    .resolve(
        "python3 /workspace/skills/finance/other/scripts/run.py",
        Some(5),
    )
    .unwrap();
assert_eq!(defaulted.timeout_secs, 1800);
assert_eq!(defaulted.configured_timeout_secs, 1800);
assert_eq!(defaulted.requested_timeout_secs, Some(5));
assert_eq!(defaulted.source, "default");
```

- [ ] **Step 2: 运行目标测试并确认按预期失败**

Run:

```bash
cargo test -p tyclaw-tools configured_skill_timeout_is_authoritative
```

Expected: 编译失败，指出 `ResolvedSkillExecution` 尚无 `configured_timeout_secs` 或 `requested_timeout_secs` 字段；不得因测试名过滤错误而显示 `0 tests`。

- [ ] **Step 3: 扩充决策结果并实现配置权威规则**

将结构体改为：

```rust
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedSkillExecution {
    pub skill_name: String,
    pub timeout_secs: u64,
    pub configured_timeout_secs: u64,
    pub requested_timeout_secs: Option<u64>,
    pub source: &'static str,
}
```

将 `resolve_in_workspace` 的超时计算改为：

```rust
let skill_name = identify_skill_in_workspace(command, workspace)?;
let configured_timeout_secs = self.timeout_for(&skill_name);
let requested_timeout_secs = requested_timeout.filter(|value| *value > 0);
Some(ResolvedSkillExecution {
    timeout_secs: configured_timeout_secs,
    configured_timeout_secs,
    requested_timeout_secs,
    source: self.source_for(&skill_name),
    skill_name,
})
```

删除原来的 `value.min(configured_timeout)` 逻辑，不增加 `max`、feature flag 或兼容分支。

- [ ] **Step 4: 运行策略模块测试**

Run:

```bash
cargo test -p tyclaw-tools configured_skill_timeout_is_authoritative
```

Expected: PASS，至少运行 1 个测试。

- [ ] **Step 5: 检查该任务的差异**

Run:

```bash
git diff -- crates/tyclaw-tools/src/skill_execution.rs
```

Expected: 仅包含结构体元数据、权威决策和对应测试；Skill 路径识别、前台约束及 Timer 安全规则无变化。

---

### Task 2: 更新 ExecTool 接口说明、日志和工具边界测试

**Files:**
- Modify: `crates/tyclaw-tools/src/shell.rs:26-40`
- Modify: `crates/tyclaw-tools/src/shell.rs:125-145`
- Modify: `crates/tyclaw-tools/src/shell.rs:185-242`
- Test: `crates/tyclaw-tools/src/shell.rs:553-615`

**Interfaces:**
- Consumes: Task 1 新增的 `ResolvedSkillExecution.configured_timeout_secs` 和 `requested_timeout_secs`。
- Produces: host/sandbox 共用的结构化策略日志；`resolve_command_policy` 对 Skill 返回配置值，对普通命令维持原值。

- [ ] **Step 1: 扩充工具边界测试，验证新语义贯通 ExecTool**

把 `exec_tool_resolves_skill_and_ordinary_timeouts_separately` 中 Skill 请求改为 `Some(5)`，并断言：

```rust
let (skill_timeout, skill) = tool
    .resolve_command_policy(
        "python3 /workspace/skills/category/example-skill/scripts/run.py",
        Some(5),
    )
    .unwrap();
let skill = skill.unwrap();
assert_eq!(skill_timeout, 2700);
assert_eq!(skill.skill_name, "example-skill");
assert_eq!(skill.configured_timeout_secs, 2700);
assert_eq!(skill.requested_timeout_secs, Some(5));
```

保留并补齐普通命令断言：

```rust
let (ordinary_default, skill) = tool.resolve_command_policy("echo ok", None).unwrap();
assert_eq!(ordinary_default, 120);
assert!(skill.is_none());

let (ordinary_requested, skill) = tool
    .resolve_command_policy("echo ok", Some(7))
    .unwrap();
assert_eq!(ordinary_requested, 7);
assert!(skill.is_none());
```

更新 `sandbox_timeout_uses_skill_aware_error_message` 的 fixture：

```rust
let resolved = ResolvedSkillExecution {
    skill_name: "example-skill".into(),
    timeout_secs: 2700,
    configured_timeout_secs: 2700,
    requested_timeout_secs: Some(5),
    source: "override",
};
```

- [ ] **Step 2: 运行工具边界测试并确认新语义已贯通**

Run:

```bash
cargo test -p tyclaw-tools exec_tool_resolves_skill_and_ordinary_timeouts_separately
```

Expected: PASS，且至少运行 1 个测试；证明短请求不能缩短 Skill 配置，同时普通命令仍可采用请求值。

- [ ] **Step 3: 更新 `exec.timeout` 参数说明**

将参数描述改为：

```rust
"timeout": {
    "type": "integer",
    "description": "Timeout in seconds for ordinary commands. Recognized Skill commands ignore this parameter and use the administrator-configured skill_execution timeout. Default for ordinary commands: 120."
}
```

不删除该参数，因为普通命令仍需要调用方控制。

- [ ] **Step 4: 在 host 和 sandbox 路径记录完整决策**

将两处现有 `Applying skill execution policy` 日志更新为相同字段：

```rust
tracing::info!(
    skill_name = %resolved.skill_name,
    policy_source = resolved.source,
    configured_timeout_secs = resolved.configured_timeout_secs,
    requested_timeout_secs = ?resolved.requested_timeout_secs,
    effective_timeout_secs = resolved.timeout_secs,
    requested_timeout_ignored = resolved.requested_timeout_secs.is_some(),
    "Applying skill execution policy"
);
```

不得记录完整命令或业务参数。两条执行路径必须使用完全相同的字段名。

- [ ] **Step 5: 保持超时和取消错误语义不变**

确认 `format_sandbox_result` 和 host 的 `HostFinish` 分支仍满足：

```text
timeout   -> Error: code=timeout Skill '<name>' 执行超时（<effective> 秒）
cancelled -> Error: code=cancelled Command cancelled
```

若代码已满足，不做无意义改写。

- [ ] **Step 6: 运行 `tyclaw-tools` 全量测试**

Run:

```bash
cargo test -p tyclaw-tools
```

Expected: PASS；普通命令 timeout、Skill 识别、前台执行限制、Timer exec 策略和 Skill 专属错误全部通过。

---

### Task 3: 明确示例配置和设计状态

**Files:**
- Modify: `workspace/config/config.example.yaml:130-143`
- Verify: `docs/superpowers/specs/2026-09-14-authoritative-skill-execution-timeout-design.md`

**Interfaces:**
- Consumes: Task 1 的配置权威语义。
- Produces: 无需迁移即可继续解析的 YAML；管理员能理解 `timeout_secs` 是权威期限而不是模型建议值。

- [ ] **Step 1: 更新示例配置注释**

保持所有键和值不变，只把注释更新为：

```yaml
skill_execution:
  # 已识别 Skill 的权威执行期限；定时任务和手动任务共用。
  # exec 工具调用中的 timeout 不会缩短或延长该值。
  # 用户或系统主动取消仍可立即终止；该值与 workspace.idle_timeout_secs 独立。
  default_timeout_secs: 1800

  # 可按 SKILL.md 所在目录名覆盖；未列出的 Skill 使用上方默认值。
```

- [ ] **Step 2: 运行配置解析测试**

Run:

```bash
cargo test -p tyclaw-orchestration example_config_contains_valid_skill_execution_policy
```

Expected: PASS，证明注释调整未破坏 YAML，且示例中的默认值和各 Skill 覆盖值仍能正确解析。

- [ ] **Step 3: 检查设计与实现无矛盾**

Run:

```bash
rg -n "min\(|max\(|配置权威|requested_timeout_ignored|普通非 Skill" \
  docs/superpowers/specs/2026-09-14-authoritative-skill-execution-timeout-design.md \
  crates/tyclaw-tools/src/skill_execution.rs \
  crates/tyclaw-tools/src/shell.rs \
  workspace/config/config.example.yaml
```

Expected: 当前设计明确拒绝 `min/max`；生产决策代码不再出现请求值与配置值的 `min/max`；普通命令分支仍保留请求参数。

---

### Task 4: 全面回归、风险验收与交付检查

**Files:**
- Verify only: `crates/tyclaw-tools/`
- Verify only: `crates/tyclaw-sandbox/`
- Verify only: `crates/tyclaw-orchestration/`
- Verify only: workspace-level Rust build

**Interfaces:**
- Consumes: Tasks 1–3 的全部改动。
- Produces: 可发布的验证证据；不修改真实配置或调用外部财务系统。

- [ ] **Step 1: 格式化本次 Rust 改动**

Run:

```bash
cargo fmt --all
```

Expected: 仅格式化本次触及的 Rust 文件。运行后检查 `git diff --stat`，若出现无关格式化，停止并只保留任务相关格式变化。

- [ ] **Step 2: 运行目标 crate 测试**

Run:

```bash
cargo test -p tyclaw-tools
cargo test -p tyclaw-sandbox
cargo test -p tyclaw-orchestration
```

Expected: 全部 PASS。`tyclaw-sandbox` 默认单测不得要求真实生产凭证；若 Docker 集成测试因 daemon 或镜像缺失未运行，交付说明记录环境限制。

- [ ] **Step 3: 运行 workspace 静态检查和严格 lint**

Run:

```bash
cargo check --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
```

Expected: 全部退出码为 0，无 warning。

- [ ] **Step 4: 检查差异和敏感信息**

Run:

```bash
git diff --check
git diff -- \
  crates/tyclaw-tools/src/skill_execution.rs \
  crates/tyclaw-tools/src/shell.rs \
  workspace/config/config.example.yaml \
  docs/superpowers/specs/2026-09-14-authoritative-skill-execution-timeout-design.md \
  docs/superpowers/plans/2026-09-14-authoritative-skill-execution-timeout.md
git status --short
```

Expected:

- 无空白错误；
- 没有真实 API key、token、Cookie、用户标识或财务数据；
- 没有修改 `workspace/config/config.yaml`、业务 Skill 或生成物；
- 现有无关未提交文件保持原状；
- 实现文件只包含权威策略、元数据、日志、参数说明和测试。

- [ ] **Step 5: 对照验收矩阵收口**

逐项确认：

```text
Skill: requested=None   -> configured
Skill: requested=0      -> configured
Skill: requested=short  -> configured
Skill: requested=equal  -> configured
Skill: requested=long   -> configured
Ordinary: requested=None -> Exec default
Ordinary: requested=N    -> N
Cancellation             -> cancelled，不等待 configured timeout
Timeout                  -> timeout，错误显示 effective timeout
```

交付说明必须列出修改位置、行为变化、实际执行的验证、未执行验证及原因。未经用户明确要求，不创建提交、不推送、不修改生产配置。
