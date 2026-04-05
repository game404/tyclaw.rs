---
name: 项目接入
description: 引导用户完成项目定制，将空模板填充为可用的排查环境
category: onboarding
tags: [接入, 初始化, 配置, 引导]
triggers: [项目接入, 初始化项目, 配置 BugHunter, 接入]
tool: null
risk_level: write
---

# 项目接入引导

引导用户完成 BugHunter 的项目定制，将空模板填充为可用的排查环境。

## 安全约束

1. 只修改本仓库内的文件，不修改外部仓库
2. 不写入实际密钥到 `defaults.py` 或其他可提交文件（密钥只能写入 `config/config.yaml`）
3. 每个阶段完成后让用户确认，不自动跳到下一阶段

## 启动

1. 读取 `openspec/tasks.md`，了解完整的定制任务清单
2. 读取 `config/defaults.py`，了解需要填充的数据结构和格式
3. 读取 `knowledge/SKILL.md`，了解模板结构
4. 检查当前进度：`defaults.py` 中的 dict 是否已填充、`knowledge/SKILL.md` 是否已编写、`config/config.yaml` 是否已创建
5. 向用户报告当前状态，从未完成的第一个阶段开始引导

## 引导阶段

### 阶段 1: SLS 日志配置（必须）

向用户收集：
- SLS 项目名、endpoint（如 `cn-beijing.log.aliyuncs.com`）
- SLS AccessKey ID / Secret（写入 `config/config.yaml`）

**自动探查流程**（推荐）：
1. 先在 `config/defaults.py` 填入最小 `SLS_PROJECTS`（只需 endpoint + project，logstores 留空 `{}`）
2. 确保 `config/config.yaml` 已填好 SLS AK/SK
3. 运行 `PYTHONPATH=. venv/bin/python3 tools/discover_logstores.py` 自动探查
4. 脚本会自动发现所有 logstore，获取每个的索引字段和样例数据
5. 根据探查结果，自动为每个 logstore 生成配置（name、role_id_field、description），回填到 `SLS_PROJECTS`
6. 让用户确认

**role_id_field 推断规则**：从索引字段中查找包含 `roleId`、`role_id`、`rid`、`ext.rid` 等关键词的字段。如果样例数据中某个字段值看起来像玩家 ID（纯数字、10-20 位），优先选择。无法确定时设为 `None`。

收集完毕后：
1. 确认 `config/defaults.py` 的 `SLS_PROJECTS` 已正确填充
2. 根据 logstore 信息更新 `skills/log-guide/SKILL.md`，填写各 logstore 的字段速查和查询示例

### 阶段 2: GM 接口配置（必须）

向用户收集：
- GM 系统有哪些接口，各接口的路径、HTTP 方法、参数
- 常用的查询场景（如查玩家基本信息、背包、邮件等）

收集完毕后：
1. 填充 `config/defaults.py` 的 `GM_ENDPOINTS` dict
2. 让用户确认
3. 根据接口信息更新 `skills/gm-api/SKILL.md`，补充项目特有的使用示例

### 阶段 3: 代码仓库配置

向用户收集：
- 有哪些代码仓库（服务端、客户端、配表、GM 后台等）
- 各仓库在本机的路径
- 哪些仓库需要 worktree 管理（多分支并行开发）
- 服务端代码中哪些目录支持热更、哪些不支持（影响 MR 目标分支选择）

收集完毕后：
1. 帮用户生成 `config/config.yaml` 的 `code` 段配置
2. 根据热更目录信息更新 `skills/gitlab-mr/SKILL.md` 的目标分支策略

### 阶段 4: 项目业务知识（必须）

向用户收集：
- 服务器语言和框架（如 C#/Orleans、Java/Spring、Go 等）
- 客户端引擎（如 Cocos Creator、Unity 等）
- 配表工具（如 Luban、Excel 直读等）
- 服务端和客户端的核心目录结构
- 关键业务模块及其入口文件
- 常用枚举和配置 ID
- 已知的排查经验和常见问题模式
- 用户口语化说法与实际模块的映射

⚠️ 不要求一次提供全部，信息可以分批补充。先收集用户能立即提供的，其余标注 TODO。

收集完毕后：
1. 编写 `knowledge/SKILL.md`，将信息填入对应章节
2. 让用户确认

### 阶段 5: 配表工具适配（按需）

根据阶段 4 收集到的配表工具信息：
- 如果使用 Luban：更新 `skills/config-reader/SKILL.md` 中的查表流程和索引机制
- 如果使用其他工具：调整配表读取相关 skill 的示例和说明
- 更新 `.cursor/rules/config-review.mdc` 中的配表导出步骤

### 阶段 6: 战斗回放适配（可选）

如果项目有战斗回放解码需求：
- 收集战斗 protobuf 的字段定义和结构
- 调整 `tools/decode_battle_replay.py` 中的字段映射
- 更新 `skills/battle-replay/SKILL.md` 中的分析框架

## 完成

所有阶段完成后：
1. 运行各工具的基本连通性测试（如有 config.yaml 凭据）：
   - `venv/bin/python3 tools/sls_query.py --logstore <第一个logstore> --query "*" --limit 1`
   - `venv/bin/python3 tools/gm_api.py --endpoint <第一个endpoint> --help`
2. 更新 `openspec/tasks.md`，勾选已完成的定制任务
3. 输出接入完成摘要：已配置的能力、待补充的内容、可以开始使用的功能

## 核心原则

- **每次只聚焦一个阶段**，不在一个消息里问太多问题
- **给出具体示例**：提问时附带格式示例，降低用户理解成本
- **容错**：用户说"不知道"或"跳过"时，标注 TODO 并继续下一阶段
- **渐进式完善**：先让核心功能跑起来（SLS + GM + 代码路径），其余后续补充
