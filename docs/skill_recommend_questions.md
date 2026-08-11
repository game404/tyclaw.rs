# Skill 推荐问题（追问建议 / 猜你想问）机制

> 面向 skill 作者与渠道维护者。说明 tyclaw.rs 里「推荐问题」是怎样从 skill 产出、
> 被渠道解析、最终渲染成钉钉卡片按钮（或可点击 dtmd 链接）的，以及 skill 应当如何书写。

## 1. 概述

「推荐问题」指回答末尾展示的一组用户可以一键继续追问的问句，在产品里也叫
**追问建议**、**猜你想问**、**您可能还想了解**。用户点击后，钉钉客户端会以本人身份把
该问句原样发回会话，从而触发新一轮问答。

tyclaw.rs 里存在**两套**实现，请优先用第一套：

| 机制 | 状态 | 产出方 | 传递路径 |
|---|---|---|---|
| A. 正文末尾「追问建议」块 | ✅ 当前推荐 | skill 把追问块写进回复正文末尾 | 渠道 egress 解析并剥离 |
| B. `suggest_recommends` 工具 | ⚠️ 已停用 | Agent 调用工具写入 | `AgentResponse.recommends` |

机制 B 的工具已在编排层停用（见 `builder.rs`），但 `PendingRecommendStore` 与
`AgentResponse.recommends` 字段保留以减少改动面，并作为机制 A 为空时的回退来源。
个别老 skill（如 `dingtalk-calendar`）仍在文档里提到调用 `suggest_recommends`，
新 skill **不要**再依赖它，统一用机制 A。

## 2. 机制 A：正文末尾写「追问建议」块（推荐）

### 2.1 设计思路

正文的丰富度与领域感知全部由 skill 掌控：模型把带领域知识的追问建议**写进回复正文
末尾**，渠道出口再把这块文本解析出来、渲染成卡片按钮，并从展示正文里剥离，避免重复。

好处：不占用一次工具调用、追问内容与正文同源、对未适配的渠道无副作用（解析不到就原样
下发）。

### 2.2 skill 侧书写规范

在最终回复正文的**末尾**追加一个追问块，标题行 + 列表项：

```
---

💡 **您可能还想了解：**
1. {具体化的追问问题，含真实产品名 + 指标名/事项名}
2. {另一维度的追问问题}
3. {第三条}
```

书写要求（来自 `weekly-report-faq` / `weekly-report-faq-lite` 的 `answer_guide.md`）：

- **标题行**必须包含下列关键字之一，才能被渠道识别（去掉 emoji/加粗/`#` 后按子串匹配）：
  `您可能还想了解`、`你可能还想问`、`您可能还想问`、`你可能还想了解`、`追问建议`、`猜你想问`。
- **列表项**支持有序（`1.` / `1、` / `1)` / `1）` / `1．` / `1:` / `1：`）与无序（`-` / `*` / `•`）；
  项内的 `**加粗**` 会被去掉。
- 建议 **3-5 条**；渠道侧最多保留 **5 条**，超出截断。
- 内容要**具体、可直接复制发送**：含真实产品名 / 竞品名 / 字段名，名称必须来自真实数据源，
  不得编造；多条之间覆盖不同维度，避免同质化。
- 追问块**之后**可以再跟「数据来源标注」等非列表行（如 `📎 数据来源：...`），解析遇到
  非列表行即停止，不会误吞。
- 允许列表项之间夹空行。

### 2.3 与正文完整性的关系（强制）

`prompts.yaml` 的「回答呈现规范」明确：**先写完整正文，追问建议再附在正文末尾**。
严禁因为要输出追问块而把正文写短、或用「详见上方」之类占位替代正文。若某 skill
（如 weekly-report-faq）规定了输出结构（意图明细 + 综合简评 + 追问建议 + 数据来源标注），
必须完整遵循，不得删减。

### 2.4 渠道 egress 解析

入口：`crates/tyclaw-channel/src/dingtalk/sanitize.rs` 的 `extract_recommends(text) -> (剥离后正文, 问题列表)`。

解析规则：

1. 从后往前找最后一个匹配 `RECOMMEND_HEADINGS` 的**标题行**（取最后一个，避免正文中偶然
   提及导致误判）。找不到则原样返回 `(text, [])`。
2. 从标题行往下用 `parse_list_item` 逐行收集问题，遇到非列表行或文本结束即停止；跨空行探测。
3. 去重、最多保留 5 条。
4. 向前吞掉紧邻标题的空行与单独的分隔线（`---` / `***` / `___`），避免剥离后留下悬空分隔线。
5. 返回剥离追问块后的正文 + 问题列表。

### 2.5 渲染成钉钉卡片 / 纯文本

入口：`crates/tyclaw-channel/src/dingtalk/bot.rs`，两种呈现形态：

- **AI 卡片（首选）**：`build_recommends_json(&[String])` 生成按钮数据 JSON，形如
  `[{"text":"问题","url":"dtmd://dingtalkclient/sendMessage?content=<urlencoded>"}]`，
  写入卡片模板变量 `recommends`，由「按钮」组件循环渲染。卡片正文的 markdown 组件**不支持
  dtmd**，所以卡片场景必须走按钮组件。
- **纯文本 fallback（未启用卡片 / 卡片 finalize 失败）**：`render_recommend_questions(&[String])`
  把问题内联成 dtmd 链接列表，拼到正文末尾（普通 markdown 消息支持 dtmd 链接）：

```
---

**🤔 你可能还想问：**

- [问题](dtmd://dingtalkclient/sendMessage?content=<urlencoded>)
```

卡片最终态由 `ai_card.rs` 的 `finalize(reply, recommends_json, recommends_md)` 提交，
分别写入模板变量 `recommends`（按钮）与 `recommends_md`（markdown 组件，用于验证卡片内
markdown 是否支持 dtmd）。

### 2.6 数据源优先级

`bot.rs` 组装回复时：

```
优先使用 从正文提取的 extracted_recommends（机制 A）
若为空 → 回退到 response.recommends（机制 B，通常为空）
```

## 3. 机制 B：`suggest_recommends` 工具（已停用）

- 工具定义：`crates/tyclaw-tools/src/interaction.rs` 的 `SuggestRecommendsTool`，
  名称 `suggest_recommends`，入参为字符串数组 `questions`（建议 2-3 条完整问句）。
- 存储：写入按请求隔离的 `PendingRecommendStore`，编排层结束时 `drain` 到
  `AgentResponse.recommends`。
- 停用点：`crates/tyclaw-orchestration/src/builder.rs` 里工具**未注册**，注释说明推荐问题
  改由机制 A 的正文追问块实现；`PendingRecommendStore` 与 `AgentResponse.recommends` 字段
  仅作保留与回退。

新 skill 不要再写「回答结束时调用 `suggest_recommends`」。

## 4. 端到端流程

```
skill（正文末尾写「追问建议」块）
   ↓  Agent 产出最终回复正文
渠道 egress: extract_recommends()
   ├─ 剥离追问块后的正文 → 用于展示
   └─ 问题列表
        ├─ 卡片: build_recommends_json() → 模板变量 recommends → 按钮组件
        └─ 纯文本 fallback: render_recommend_questions() → 内联 dtmd 链接
             ↓ 用户点击
        钉钉以本人身份把问题发回会话 → 新一轮问答
```

## 5. skill 作者 checklist

- [ ] 正文完整详尽在前，追问块附在**最末尾**（数据来源标注可再跟其后）。
- [ ] 标题行含被识别的关键字之一（如 `💡 **您可能还想了解：**`）。
- [ ] 3-5 条，有序或无序列表；内容具体、含真实名称、可直接复制发送、覆盖不同维度。
- [ ] 不编造名称，全部来自真实数据源。
- [ ] 不依赖 `suggest_recommends` 工具。
- [ ] 结构化数据用 bullet list，不用 markdown 管道表格（钉钉渲染不稳定）。

## 6. 相关代码 / 参考

- `crates/tyclaw-channel/src/dingtalk/sanitize.rs` — `extract_recommends`、`RECOMMEND_HEADINGS`、`parse_list_item`
- `crates/tyclaw-channel/src/dingtalk/bot.rs` — `build_recommends_json`、`render_recommend_questions`、egress 组装
- `crates/tyclaw-channel/src/dingtalk/ai_card.rs` — `finalize` / 模板变量 `recommends` / `recommends_md`
- `crates/tyclaw-tools/src/interaction.rs` — `SuggestRecommendsTool`、`PendingRecommendStore`（机制 B，已停用）
- `crates/tyclaw-orchestration/src/builder.rs` — 工具注册处（说明机制 B 停用）
- `workspace/config/prompts.yaml` — 回答呈现规范（正文完整性、追问建议附末尾）
- `workspace/skills/finance/weekly-report-faq/references/answer_guide.md` — 追问建议书写模板
- `workspace/skills/finance/weekly-report-faq-lite/references/answer_guide.md` — 精简版书写模板
