# 系统提示词与身份配置指南

TyClaw 的系统提示词由模块化的 Section 组成，大部分通过 workspace 根目录下的 Markdown 文件配置，**无需修改代码、无需重新编译**。

## 系统提示词结构

系统提示词按以下顺序拼接，每个 Section 之间用 `---` 分隔：

```
┌──────────────────────────────────────────────┐
│ 1. Identity        IDENTITY.md + 运行时信息   │
│ 2. Bootstrap       workspace/*.md 自动扫描    │
│ 3. Memory          memory/MEMORY.md 长期记忆  │
│ 4. Date & Time     代码动态生成，当前时间/时区  │
│ 5. Capabilities    运行时注入，可用能力列表     │
│ 6. Skills          运行时注入，技能完整内容     │
│ 7. Cases           运行时注入，相似历史案例     │
└──────────────────────────────────────────────┘
```

其中 Section 4 由代码动态生成；Section 5-7 由编排器在运行时注入；**Section 1-3 完全由文件驱动**。

## Bootstrap 文件配置

### 工作原理

ContextBuilder 启动时会**扫描 workspace 根目录下所有 `*.md` 文件**，按文件名字母顺序排序后依次加载到系统提示的 Bootstrap 段。

- 新增一个 `.md` 文件即可自动生效（下次请求时加载）
- 删除文件即可移除对应内容
- 修改文件内容后自动刷新（基于文件 mtime + size 的缓存指纹机制）

### 推荐文件

| 文件名 | 用途 | 说明 |
|--------|------|------|
| `IDENTITY.md` | Agent 身份描述 | "你是谁"——名称和一句话定位（由 Identity section 单独加载，不进入 Bootstrap 段） |
| `GUIDELINES.md` | 行为准则与 ReAct 协议 | 控制 Agent 工具调用行为、循环终止协议等 |
| `SOUL.md` | Agent 人格与语气 | 定义 Agent 的沟通风格、语言偏好、行为边界 |
| `AGENTS.md` | 多 Agent 协作规则 | 定义 Agent 之间的分工与交互方式 |
| `USER.md` | 用户交互规范 | 用户偏好、回复格式要求等 |
| `TOOLS.md` | 工具使用指南 | 各工具的使用场景、注意事项、示例 |

文件名不限于以上列表——任何 `*.md` 文件都会被加载。例如你可以创建 `SAFETY.md`（安全策略）、`ROUTING.md`（消息路由规则）等。

### 示例：SOUL.md

```markdown
## 身份

你是 TyClaw，途游游戏的企业 AI 助手。

## 语气

- 使用简洁专业的中文交流
- 回答要直接，不要冗余的开场白
- 涉及线上问题时保持严谨，给出明确的操作步骤

## 边界

- 不要编造不确定的信息，如实告知不知道
- 涉及危险操作（删除文件、执行 rm 等）时必须确认
```

### 示例：GUIDELINES.md

```markdown
## Guidelines

- State intent before tool calls, but NEVER predict results.
- Before modifying a file, read it first.
- If a tool call fails, analyze the error before retrying.
- Ask for clarification when the request is ambiguous.

## ReAct Loop Control Protocol

- If task is complete and no more tools are needed, return JSON content with:
  `{"loop_control":{"status":"done","reason":"..."},"final_answer":"..."}`
- If task is not complete, return normal content or tool calls as usual.
```

## Identity 配置（IDENTITY.md）

Identity 是系统提示词的第一个 Section，由 `IDENTITY.md` 文件 + 代码追加的运行时信息组成。

### 文件格式

在 workspace 根目录创建 `IDENTITY.md`：

```markdown
# TyClaw Agent

You are TyClaw, an AI assistant for enterprise automation.
```

代码会自动在其后追加运行时信息：

```
## Runtime
{os} {arch}, Rust

## Workspace
Your workspace is at: {workspace_path}
- Long-term memory: {workspace_path}/memory/MEMORY.md
- History log: {workspace_path}/memory/HISTORY.md
```

### 自定义示例

如果你想把 Agent 改名为 "GameHelper"：

```markdown
# GameHelper

You are GameHelper, a game development assistant for TuYou Games.
You communicate in Chinese and specialize in game server troubleshooting.
```

### 默认值

如果 `IDENTITY.md` 不存在或为空，则使用内置默认值：

```
# TyClaw Agent

You are TyClaw, an AI assistant for enterprise automation.
```

> **注意**：`IDENTITY.md` 不会被 Bootstrap 段重复加载——它由 Identity section 单独处理。人格、语气等细节建议放在 `SOUL.md` 中，Identity 只需要一句话说明"你是谁"。

## Memory（自动维护）

`memory/MEMORY.md` 由记忆合并器（MemoryConsolidator）自动维护，无需手动编辑。当对话 token 超过上下文窗口 50% 时，合并器会调用 LLM 将旧对话浓缩为事实摘要写入此文件。

手动编辑 `MEMORY.md` 也是安全的——内容会在下次请求时加载到系统提示中。

## PromptMode 分级

系统提示词支持三种模式，用于不同场景：

| 模式 | 包含的 Section | 适用场景 |
|------|---------------|---------|
| `Full` | 全部 7 个 Section | 主 Agent，正常对话 |
| `Minimal` | Identity + TOOLS.md + GUIDELINES.md + DateTime | 子 Agent、记忆合并等辅助调用 |
| `None` | 仅一行身份说明 | 极简场景（如嵌入式调用） |

Minimal 模式下只加载 `TOOLS.md` 和 `GUIDELINES.md` 两个引导文件，大幅节省 token。

### 代码中使用 PromptMode

```rust
use tyclaw_agent::{ContextBuilder, PromptMode, PromptParams};

let ctx = ContextBuilder::new("/path/to/workspace");

// Full 模式（默认，向后兼容）
let prompt = ctx.build_system_prompt(caps, skills, cases);

// 指定模式
let prompt = ctx.build_system_prompt_with_params(&PromptParams {
    mode: PromptMode::Minimal,
    ..Default::default()
});

// 使用自定义模式构建消息列表
let messages = ctx.build_messages_with_mode(
    PromptMode::Minimal,
    &history, "user input",
    None, None, None,   // caps, skills, cases
    None, None, None, None,  // channel, chat_id, user_id, workspace_id
);
```

## 缓存机制

Bootstrap 文件和 MEMORY.md 被归入「稳定前缀」，有内存缓存：

- **缓存键**：workspace 路径 + OS + ARCH + 所有 `*.md` 文件的 `(mtime, size)` 指纹 + MEMORY.md 指纹
- **失效条件**：任何 `.md` 文件的内容变化（修改/新增/删除）都会导致缓存键变化，触发重建
- **作用范围**：仅 Full 模式使用缓存；Minimal 模式每次独立构建

这意味着修改文件后**无需重启服务**，下次请求会自动加载最新内容。

## 部署架构与多团队隔离

TyClaw 采用**部署隔离**策略，而非代码级多租户：

```
1 Team = 1 Rust 进程 = 1 根目录 = 1 钉钉机器人 = N 个 workspace_id
```

### 隔离模型

每个团队独立部署一个 TyClaw 进程，拥有独立的根目录，从而天然隔离：

```
/opt/tyclaw/game-server/          ← 游戏服务端团队
├── IDENTITY.md                    # "你是游戏服务端助手"
├── SOUL.md
├── GUIDELINES.md
├── skills/ → /opt/tyclaw/shared-skills/  # 符号链接，共享技能
├── _personal/                     # 本团队个人技能
├── memory/
├── sessions/
└── config/config.yaml             # 本团队的钉钉 bot + LLM 配置

/opt/tyclaw/game-client/          ← 游戏客户端团队
├── IDENTITY.md                    # "你是游戏客户端助手"
├── SOUL.md
├── skills/ → /opt/tyclaw/shared-skills/  # 同一个共享技能目录
├── _personal/
├── memory/
├── sessions/
└── config/config.yaml

/opt/tyclaw/shared-skills/        ← 公共技能库（所有团队共享）
├── troubleshoot/
├── code/
├── data/
└── ops/
```

### 共享 Skills

不同团队的进程可以通过以下方式共享 Skills：

- **符号链接**：`ln -s /opt/tyclaw/shared-skills /opt/tyclaw/game-server/skills`
- **配置指定**：在 SkillManager 初始化时传入公共 skills 目录路径

个人技能（`_personal/`）始终在各自根目录下，不共享。

### 同一进程内的 workspace_id

一个进程内的多个 workspace_id（如 `game.main`、`game.code`）共享同一套 md 文件和系统提示词，区别仅在于：

- **会话隔离**：session key = `workspace_id:channel:chat_id`
- **RBAC**：不同 workspace_id 可配置不同的成员角色
- **技能路由**：不同 workspace_id 可配置不同的 `builtin_capabilities` 白名单
- **钉钉群路由**：通过 `config.yaml` 的 `dingtalk.routes` 映射群 → workspace_id

### 部署示例

为游戏服务端团队部署一个实例：

```bash
# 1. 创建根目录
mkdir -p /opt/tyclaw/game-server/{memory,sessions,_personal,config}

# 2. 链接公共技能
ln -s /opt/tyclaw/shared-skills /opt/tyclaw/game-server/skills

# 3. 配置身份和行为
cat > /opt/tyclaw/game-server/IDENTITY.md << 'EOF'
# GameServer Assistant

You are the AI assistant for the game server team at TuYou Games.
EOF

cat > /opt/tyclaw/game-server/SOUL.md << 'EOF'
## 语气
- 使用中文交流，技术术语可用英文
- 回答简洁直接
EOF

cp /opt/tyclaw/templates/GUIDELINES.md /opt/tyclaw/game-server/

# 4. 配置 LLM 和钉钉
cat > /opt/tyclaw/game-server/config/config.yaml << 'EOF'
llm:
  api_key: "sk-..."
  api_base: "https://relay.example.com/v1/"
  model: "openai/claude-sonnet-4-20250514"
dingtalk:
  client_id: "game-server-bot-id"
  client_secret: "game-server-bot-secret"
  workspace_id: "game.server"
  routes:
    "cid_server_dev": "game.server.dev"
    "cid_server_ops": "game.server.ops"
EOF

# 5. 启动
cd /opt/tyclaw/game-server && tyclaw serve
```

## 配置文件总览

所有配置文件均位于 workspace 根目录，修改后无需重启，下次请求自动生效：

```
workspace/
├── IDENTITY.md          # Agent 身份（"你是谁"）
├── GUIDELINES.md        # 行为准则、ReAct 协议
├── SOUL.md              # 人格、语气、边界
├── TOOLS.md             # 工具使用指南
├── USER.md              # 用户交互规范
├── AGENTS.md            # 多 Agent 协作规则
├── *.md                 # 任意自定义 Section
├── memory/
│   └── MEMORY.md        # 长期记忆（自动维护）
└── config/
    └── config.yaml      # LLM / 钉钉 / 日志配置
```

## 快速上手

1. 进入 workspace 根目录：

```bash
cd /path/to/tyclaw2/rust_edition
```

2. 定义 Agent 身份：

```bash
cat > IDENTITY.md << 'EOF'
# TyClaw Agent

You are TyClaw, an AI assistant for enterprise automation.
EOF
```

3. 创建 Agent 人格文件：

```bash
cat > SOUL.md << 'EOF'
## 语气
- 使用简洁专业的中文
- 回答直接，不要冗余开场白

## 边界
- 不编造不确定的信息
- 危险操作必须确认
EOF
```

4. 确认 GUIDELINES.md 已存在（控制工具调用行为）

5. 按需创建其他文件（TOOLS.md、USER.md 等）

6. 启动服务，所有 `*.md` 文件会自动加载到系统提示中
