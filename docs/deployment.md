# TyClaw.rs 部署指南

## 部署架构

**1 团队 = 1 DingTalk Bot = 1 rootdir = 1 进程**

每个团队拥有独立的运行目录和配置。skills 和 tools 从 shared 复制到团队目录，允许团队按需定制。

```
/opt/tyclaw/
├── shared/                    # 共享资源（发布基准）
│   ├── bin/tyclaw             # Rust 编译后的二进制
│   ├── skills/                # 技能定义（YAML/MD）
│   ├── tools/                 # Python 工具脚本
│   └── config.example.yaml    # 配置模板
│
├── team-alpha/                # 团队 A 的 rootdir
│   ├── skills/
│   │   └── common/            # cp -r shared/skills 而来，可团队定制
│   ├── tools/                 # cp -r shared/tools 而来，可团队定制
│   ├── config/
│   │   └── config.yaml        # 团队独立配置（API Key、钉钉凭据、路由等）
│   ├── sessions/              # 会话历史（自动生成）
│   ├── audit/                 # 审计日志（自动生成）
│   ├── logs/                  # 运行日志（自动生成）
│   └── memory/                # Agent 记忆（自动生成）
│
├── team-beta/                 # 团队 B 的 rootdir（结构同上）
│   └── ...（同 team-alpha）
```

二进制不需要复制或 symlink 到团队目录，通过绝对路径调用即可。

## 路径解析机制

### Rust 侧（核心进程）

所有路径基于 **cwd**（进程工作目录），即 team rootdir：

| 资源 | 路径 | 来源 |
|------|------|------|
| config | `config/config.yaml` | cwd 相对 |
| sessions | `sessions/` | cwd 相对 |
| audit | `audit/` | cwd 相对 |
| logs | `logs/tyclaw.log`（可配置） | cwd 相对 |
| memory | `memory/` | cwd 相对 |
| skills | `skills/` | cwd 相对 |
| tools | `tools/` | cwd 相对 |

### Python 工具侧

`tools/utils.py` 的 `load_config()` 按以下优先级查找 `config/config.yaml`：

1. **cwd**（Rust 进程的工作目录 = team rootdir）— 优先
2. **`__file__` 相对路径**（tools 脚本自身位置的上级）— fallback

## 初始化新团队

```bash
TEAM_DIR=/opt/tyclaw/team-gamma

# 1. 创建目录
mkdir -p $TEAM_DIR/config

# 2. 复制 skills 和 tools（团队可按需修改）
cp -r /opt/tyclaw/shared/skills $TEAM_DIR/skills/common/
cp -r /opt/tyclaw/shared/tools $TEAM_DIR/tools

# 3. 复制配置模板并填入团队信息
cp /opt/tyclaw/shared/config.example.yaml $TEAM_DIR/config/config.yaml
# 编辑 config.yaml：填入 LLM API Key、钉钉 AppKey/Secret、路由等

# 4. 启动（cd 到 rootdir，绝对路径调用二进制）
cd $TEAM_DIR && /opt/tyclaw/shared/bin/tyclaw --dingtalk
```

运行时目录（sessions、audit、logs、memory）会自动创建。

## 进程管理

### 推荐：通过 `./tyc deploy`（自动分流）

日常部署统一用 `./tyc deploy`，脚本按操作系统自动选择进程管理方式：

- **线上 Linux**：委托 [`deploy/systemd.sh`](../deploy/systemd.sh)，注册为**非 root 的 systemd user 服务**（`systemctl --user`），自动生成 `~/.config/systemd/user/tyclaw.service`（`Restart=always` 崩溃自动拉起，stdout/stderr 进 journal），无需 root 即可日常管理。
- **本地 macOS**：保留 `nohup` + `.tyclaw.pid` 方式（macOS 无 systemd），行为不变。

```bash
./tyc deploy --works-dir /home/tuyoo/xxx/works   # 部署（Linux 自动走 systemd）
./tyc stop                                       # 停止
./tyc status                                     # 状态
```

首次在 Linux 上部署，需一次性 root 开启 linger（否则登出/重启后服务不自启）：

```bash
sudo loginctl enable-linger <user>     # 如 sudo loginctl enable-linger tuyoo
```

排查命令：

```bash
systemctl --user status tyclaw               # 运行状态
journalctl --user -u tyclaw -f               # stderr / panic backtrace（RUST_BACKTRACE=full）
tail -f workspace/logs/tyclaw.log            # 应用日志（含 panic hook 记录）
```

> 崩溃日志说明：进程内已安装 panic hook，Rust panic 会同时写入 `logs/tyclaw.log`（与业务日志同源）与 journal。被 OOM Killer(SIGKILL)/段错误等直接杀死的情况不触发 panic hook，需查 `dmesg -T | grep -i oom` 与 journal。

### 备选：root 环境下的 systemd system 模板 service

有 root 且按 `/opt/tyclaw` 多团队结构部署时，可用 systemd **模板 service**，一个文件管理所有团队：

```ini
# /etc/systemd/system/tyclaw@.service
[Unit]
Description=TyClaw.rs - %i
After=network.target

[Service]
Type=simple
User=tyclaw
Group=tyclaw
WorkingDirectory=/opt/tyclaw/%i
ExecStart=/opt/tyclaw/shared/bin/tyclaw --dingtalk
Restart=always
RestartSec=5
Environment=RUST_LOG=info

[Install]
WantedBy=multi-user.target
```

`%i` 自动替换为 `@` 后的团队名：

```bash
# 启用 & 启动
systemctl enable tyclaw@team-alpha
systemctl start tyclaw@team-alpha
systemctl start tyclaw@team-beta

# 查看状态
systemctl status tyclaw@team-alpha
journalctl -u tyclaw@team-alpha -f   # 查看 stderr 输出
tail -f /opt/tyclaw/team-alpha/logs/tyclaw.log  # 查看应用日志

# 查看所有实例
systemctl list-units 'tyclaw@*'
```

## 升级流程

1. 编译新二进制，替换 `shared/bin/tyclaw`
2. 更新 `shared/skills/` 和 `shared/tools/`（如有变更）
3. 将更新同步到各团队目录：
   ```bash
   for team in team-alpha team-beta; do
       cp -r /opt/tyclaw/shared/skills /opt/tyclaw/$team/skills/common/
       cp -r /opt/tyclaw/shared/tools /opt/tyclaw/$team/tools
   done
   ```
   > 注意：如果团队对 skills/tools 有本地定制，需手动合并而非直接覆盖
4. 重启所有团队进程：`systemctl restart 'tyclaw@*'`

## 配置隔离要点

每个团队的 `config.yaml` 独立配置：

- **LLM**：可使用不同的 API Key、模型、迭代次数
- **DingTalk**：不同的 AppKey/Secret（对应不同的钉钉机器人）
- **路由**：`dingtalk.routes` 将群 ID 映射到 workspace，实现群级别的技能隔离
- **SLS**：可按需配置不同的 AccessKey（不同团队可能查询不同的日志项目）
- **Skills/Tools**：各团队独立副本，可按需增删或修改脚本逻辑
