#!/bin/bash
# systemd user 服务帮助脚本 —— 被 tyc 在 Linux 分支 source 引入。
#
# 依赖 tyc 已定义的变量：SCRIPT_DIR、RUN_DIR、BUILD_BINARY、BINARY、LOG_FILE
#
# 设计：把全部 systemd user 逻辑隔离在本文件，tyc 主体只做 OS 分流 + 委托调用。
# 线上（Linux）用非 root 的 systemd user 服务实现自动守护（Restart=always），
# 输出走 journal 自动兜住 stderr；mac 本地仍走 tyc 内的 nohup 逻辑，不引入本文件。

# 非登录 shell / Jenkins 下 `systemctl --user` 需要 XDG_RUNTIME_DIR，
# 否则报 "Failed to connect to bus"。缺省时按当前 uid 自愈。
export XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:-/run/user/$(id -u)}"

SERVICE_NAME="tyclaw"
UNIT_DIR="$HOME/.config/systemd/user"
UNIT_FILE="$UNIT_DIR/${SERVICE_NAME}.service"

# 生成 user unit 文件。$1 = 传给二进制的额外参数
# （如 "--works-dir /path --dingtalk"，由调用方拼好；可为空）。
systemd_write_unit() {
    local extra_args="$1"
    mkdir -p "$UNIT_DIR"

    local exec_args="--run-dir $RUN_DIR"
    [ -n "$extra_args" ] && exec_args="$exec_args $extra_args"

    cat > "$UNIT_FILE" <<EOF
[Unit]
Description=TyClaw.rs AI Agent (user service)
After=network.target

[Service]
Type=simple
WorkingDirectory=$SCRIPT_DIR
ExecStart=$BINARY $exec_args
Restart=always
RestartSec=5
Environment=RUST_BACKTRACE=full
StandardOutput=journal
StandardError=journal

[Install]
WantedBy=default.target
EOF
    echo "[ok] Wrote unit: $UNIT_FILE"
}

# 部署：停旧服务 -> 拷贝二进制 -> 写 unit -> reload -> enable --now。
# $1 = 传给二进制的额外参数（如 "--works-dir /path --dingtalk"；可为空）。
systemd_deploy() {
    local extra_args="$1"

    if [ ! -x "$BUILD_BINARY" ]; then
        echo "Error: Release binary not found at $BUILD_BINARY"
        echo "Run './tyc build' first."
        exit 1
    fi

    # 先停旧服务，避免 cp 覆盖运行中的二进制报 "text file busy"。
    systemctl --user stop "$SERVICE_NAME" 2>/dev/null || true

    cp "$BUILD_BINARY" "$BINARY"
    echo "Copied binary to $BINARY"

    mkdir -p "$RUN_DIR/logs"

    systemd_write_unit "$extra_args"

    systemctl --user daemon-reload
    systemctl --user enable --now "$SERVICE_NAME"

    echo "TyClaw deployed as systemd user service ($SERVICE_NAME)"
    systemctl --user status "$SERVICE_NAME" --no-pager || true
    echo ""
    echo "  App log:  $LOG_FILE"
    echo "  Journal:  journalctl --user -u $SERVICE_NAME -f"
    echo "  Monitor:  http://127.0.0.1:9394"
    echo ""
    echo "  提示：如需登出后仍运行 / 开机自启，需一次性执行(需 root)："
    echo "        sudo loginctl enable-linger $(whoami)"
}

# 停止服务。
systemd_stop() {
    if systemctl --user stop "$SERVICE_NAME" 2>/dev/null; then
        echo "Stopped $SERVICE_NAME"
    else
        echo "Not running or stop failed ($SERVICE_NAME)"
    fi
}

# 查看状态 + 排查命令提示。
systemd_status() {
    systemctl --user status "$SERVICE_NAME" --no-pager || true
    echo ""
    echo "  journalctl --user -u $SERVICE_NAME -f"
    echo "  tail -f $LOG_FILE"
}

# clean 前置：停服务。不能裸 kill，否则被 Restart=always 立即拉起。
systemd_pre_clean() {
    if systemctl --user stop "$SERVICE_NAME" 2>/dev/null; then
        echo "[ok] 服务已停止 ($SERVICE_NAME)"
    fi
}
