# TyClaw 分群与部署说明（支持 1 进程多二级 WS）

## 目标

用最简单、可运维的方式同时支持：

- 中心服务器多团队场景
- 单人多角色场景

核心原则：

- 逻辑两级：`team` + `domain`
- 物理单级：`workspace_id`
- 运行模式：支持严格隔离，也支持进程复用

---

## 一、核心概念关系

- **team**：组织边界（团队/群组）
- **domain**：能力域（code/data/ops 等）
- **workspace**：运行隔离单元（会话/记忆/审计/skills）
- **bot**：钉钉入口实例（对应一个 workspace）
- **route**：群路由映射（`conversation_id -> workspace_id`）

建议统一命名：

`workspace_id = <team>.<domain>`

示例：

- `game.code`
- `game.data`
- `game.ops`

---

## 一点五、钉钉应用 / 机器人 / 群关系（新增）

- 一个钉钉 **bot** 绑定到一个钉钉应用，应用有唯一 `AppKey/AppSecret`
- 一个 bot 可被加入多个群（受组织权限与可见范围约束）
- 机器人加入哪些群，就会接收这些群的消息事件
- 在本项目中可通过 `dingtalk.routes` 将不同群映射到不同 `workspace_id`

可理解为：

- **一个应用身份（bot） + 多群接入 + 按群路由到 workspace**

---

## 二、两种模式（同一套架构）

## 0) 进程模式选择（新增）

### 模式 A：严格隔离（传统）

- `1 workspace = 1 bot = 1 process`
- 优点：隔离最强，问题定位最直观
- 代价：进程数量线性增长

### 模式 B：团队级复用（推荐大规模）

- `1 team = 1 process`，进程内承载多个二级 `workspace`
- 钉钉入口通过 `dingtalk.routes` 按 `conversation_id` 路由到目标 workspace
- 优点：显著减少进程数量；保留 team 内二级能力隔离

## 1) 中心服务器模式

适用：多个真实团队接入、统一运维和审计。

- 每个团队按能力域拆多个 workspace
- 可选：
  - 模式 A：每个 workspace 独立 bot 和独立进程
  - 模式 B：团队一个 bot/进程，按群路由到不同 workspace

示例：

- Team Game 群：
  - `@CodeBot` -> `game.code`
  - `@DataBot` -> `game.data`
  - `@OpsBot` -> `game.ops`
- Team Platform 群：
  - `@PlatformCodeBot` -> `platform.code`

## 2) 单人多角色模式

适用：个人使用、多个角色 agent。

- 把“单人”视作一个 `team`（如 `xinzhou`）
- 多角色视作多个 `domain`（如 `code/data/ops`）
- 仍然走 `1ws=1bot=1process`

示例：

- `xinzhou.code`（开发角色）
- `xinzhou.data`（数据角色）
- `xinzhou.ops`（运维角色）

---

## 三、两级划分规则（推荐）

## Level-1：team（团队/租户边界）

负责：

- 成员和角色策略
- 资源隔离和审计归属

## Level-2：domain（能力边界）

负责：

- 能力集（builtin capabilities）范围
- 提示词和 skill 注入范围
- 机器人角色定位

---

## 四、配置模板（workspaces + dingtalk routes）

```yaml
workspaces:
  game.code:
    name: "Game - Code"
    builtin_capabilities: [code-analysis, gitlab-mr, branch-diff]
    default_role: "member"
    members:
      alice: "admin"
      bob: "developer"

  game.data:
    name: "Game - Data"
    builtin_capabilities: [config-reader, excel-diff, changelog]
    default_role: "member"

  game.ops:
    name: "Game - Ops"
    builtin_capabilities: [sls-query, server-error, tb-task]
    default_role: "member"

dingtalk:
  workspace_id: "game.main"   # 默认 workspace（未命中 routes 时使用）
  routes:
    "cid_game_code_group": "game.code"
    "cid_game_data_group": "game.data"
    "cid_game_ops_group": "game.ops"
```

说明：

- `builtin_capabilities` 用于控制该 workspace 可用的 builtin skill 子集
- personal skill 仍按 `_personal/<workspace>/<staff_id>/...` 隔离
- `routes` 用于在一个进程内把不同群映射到不同二级 workspace

---

## 五、并发模型说明（当前实现）

## 1) ReAct 内部

- 同一请求的 LLM 轮次是串行（上一轮结束后才下一轮）
- 同一轮多个工具调用可并发执行（已支持）

## 2) 多用户/多 workspace

- 当前单实例内仍有编排层串行区（大锁路径）
- 要完全释放多核并发，推荐采用多进程（即本方案 `1ws=1bot=1process`）

结论：

- 单进程：可跑，但多用户并发能力受限
- 多进程：天然并发、隔离清晰、故障域更小

---

## 六、进程部署模式（推荐）

## 标准模式（小规模）：1ws=1bot=1process

每个进程绑定：

- 一个 `workspace_id`
- 一个钉钉机器人身份
- 一套独立日志目录

建议目录：

- `workspace/<workspace_id>/...`（sessions/memory/cases/audit）
- `logs/<workspace_id>/tyclaw.log`

## 启动参数建议

- 独立 `--workspace` 路径
- 独立 `--config` 文件（或共享配置 + 实例覆盖）

## 运维建议

- 进程托管：`systemd` / `supervisor` / 容器编排均可
- 健康检查：按进程维度监控
- 扩容：新增 workspace 时直接新增一个 bot 进程

## 扩展模式（大规模）：1 team = 1 process

每个进程绑定：

- 一个 team 的多组 `workspace_id`
- 一个钉钉机器人身份
- 一份 `dingtalk.routes` 映射

效果：

- 第一级（team）由进程隔离
- 第二级（domain）由 workspace 隔离
- 通过群 ID 路由到对应二级 workspace

---

## 七、扩展一个新团队的最小步骤

1. 新增 workspace 命名：`newteam.code/data/ops`
2. 在配置中增加对应 `workspaces` 项
3. 在 `dingtalk.routes` 增加新群到 workspace 的映射
4. 重启该团队进程（模式 B）或新增进程（模式 A）
5. 在群内约定路由规则（按群或 @机器人）

---

## 八、实践建议

- 小规模先用模式 A；群数/域数增大后切换模式 B
- 命名严格规范化（小写英文、`.` 分层）
- 同一群中禁止同时 @ 多个 bot，避免重复响应
- 共享同一 LLM key 可行，但需关注总配额和 429
