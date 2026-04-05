# TyClaw.rs Demo Cases

本目录包含可直接运行的演示案例，展示 TyClaw.rs 的核心 Skill 和工具协作能力。

## 测试数据

| 文件 | 说明 |
|------|------|
| `input.xlsx` | 原始数据 Excel（多 sheet，约 9000 行游戏数据） |
| `result.xlsx` | 期望输出样例（LTV 汇总 + 分维度明细，10 个 sheet） |
| `ad.mp4` | 短视频素材（约 110 秒，竖屏 540x960） |

## 案例列表

### 案例 1：视频素材分析（video-analyzer）

> 单 Skill 演示，验证视频处理 + 图片视觉分析全链路

**发送消息：**
```
分析一下 attachments/ad.mp4
```

**预期流程：**
1. 触发「视频处理」Skill → 读取 SKILL.md → 执行 tool.py
2. 输出 scenes.json + 关键帧图片 + 场景切片
3. 用 `read_file` 读取关键帧 → 图片自动作为视觉输入发送给 LLM
4. LLM 分析画面内容，输出场景结构 + 画面风格描述

**验证点：**
- 关键帧输出到 `$TMPDIR/tyclaw_*_video-analyzer/frames/`（容器内持久化目录）
- LLM 能描述出画面的具体内容（人物、场景、色调），而不只是"无法识别"
- 总耗时约 30-70 秒

---

### 案例 2：创建数据处理 Skill（skill-creator + Excel 工具）

> 综合演示：Skill 自动创建 + Excel 数据处理

**准备：** 将 `input.xlsx` 和 `result.xlsx` 放入用户的 `attachments/` 目录

**发送消息：**
```
帮我创建一个 Skill，输入 Excel（input.xlsx），生成 LTV 数据，
输出格式跟 result.xlsx 一样
```

**预期流程：**
1. 触发「创建 Skill」→ 读取两个 Excel 的结构（sheet 名、列名、行数）
2. 对比 input 和 result 的数据关系，推导转换逻辑
3. 生成 `tool.py`（数据处理脚本）+ `SKILL.md`（使用文档）
4. 执行验证：运行生成的 tool.py，对比输出与 result.xlsx

**验证点：**
- Skill 创建在 `_personal/{workspace_id}/{staff_id}/` 目录下
- 生成的 tool.py 可独立运行
- 输出 Excel 的 sheet 名和结构与 result.xlsx 一致

---

### 案例 3：代码理解 + 知识沉淀（code-analysis + business-qa + project-knowledge）

> 多 Skill 联动：代码分析 → 业务问答 → 知识库写入

**前置条件：** 将一个代码仓库路径配置到 `config.yaml` 的 `code.repos` 中

**第一步 — 代码分析：**
```
帮我看看这个项目的入口在哪？主要模块有哪些？
```
→ 触发 code-analysis，扫描目录结构 + 入口文件，输出模块概览

**第二步 — 业务问答：**
```
用户登录的完整流程是什么？从请求到返回 token
```
→ 触发 business-qa，结合代码和配表，输出流程说明

**第三步 — 知识沉淀：**
```
把刚才分析的登录流程整理到项目知识文档里
```
→ 触发 project-knowledge，结构化写入知识库，后续可复用

**验证点：**
- code-analysis 能定位到正确的入口文件和调用链
- business-qa 能结合代码 + 配表（如有）给出完整回答
- project-knowledge 将结果持久化到 `memory/` 目录

---

### 案例 4：版本发布对比（branch-diff）

> 单 Skill 演示，适合 QA 和团队 leader 了解版本变更

**前置条件：** 项目代码仓库有多个分支（dev / release / feature 等）

**发送消息：**
```
对比一下 dev 和上个 release 分支的差异
```

**预期流程：**
1. 触发「分支对比」Skill
2. 自动识别最新的 release 分支
3. 执行 `git diff --stat` 获取文件级概览
4. 按模块分组深入分析关键变更
5. 输出结构化技术摘要

**验证点：**
- 能自动找到最新 release 分支
- 输出按模块分组，不是逐文件罗列
- 描述基于 diff 内容，不是照搬 commit message

---

### 案例 5：端到端综合流程（video-analyzer → skill-creator）

> 最完整的综合演示：分析 → 发现重复需求 → 自动化

**第一步 — 分析视频：**
```
分析一下 attachments/ad.mp4，重点看画面构成和节奏
```
→ video-analyzer 完成场景切分 + 视觉分析

**第二步 — 创建自定义分析 Skill：**
```
我经常要分析这类推广视频，帮我做一个 Skill，
每次自动输出：场景结构、画面风格、BGM 节奏评估、投放建议
```
→ skill-creator 基于 video-analyzer 的输出格式，创建个人 Skill：
  - tool.py 调用 video-analyzer 处理视频
  - 追加结构化评分模板
  - 输出标准化的素材评估报告

**第三步 — 使用新 Skill：**
```
用刚才创建的 Skill 再分析一个视频
```
→ 新 Skill 被自动识别并触发，一键输出完整评估报告

**验证点：**
- 第一步和第二步的 Skill 调用链路正确
- 创建的个人 Skill 能被后续消息触发
- 输出格式在多个视频之间保持一致

---

## 运行方式

1. 启动 TyClaw.rs：
   ```bash
   cargo run -p tyclaw-app -- --run-dir workspace
   ```

2. 将测试数据复制到用户工作目录：
   ```bash
   # reset.sh 会自动将 demo/ 下的文件复制到 cli_user 的 attachments/
   ./script/reset.sh
   ```

3. 在 CLI 中输入上述案例的消息，观察执行过程

## Skill 能力矩阵

| Skill | 类型 | 需要 Docker | 需要代码仓库 | 需要附件 |
|-------|------|:-----------:|:----------:|:-------:|
| video-analyzer | 工具型（tool.py） | ✓ | | ✓ mp4 |
| skill-creator | 元能力 | ✓ | | 视需求 |
| code-analysis | 提示型 | | ✓ | |
| business-qa | 提示型 | | ✓ | |
| branch-diff | 提示型 | | ✓ | |
| project-knowledge | 模板型 | | | |
