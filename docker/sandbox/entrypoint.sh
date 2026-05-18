#!/bin/bash
# TyClaw sandbox entrypoint
# 1. 动态注入当前 UID 到 /etc/passwd（兼容任意 --user UID:GID）
# 2. 启动 Xvfb 虚拟屏幕
# 3. sleep infinity 等待 docker exec

if [ "$(id -u)" != "0" ] && ! getent passwd "$(id -u)" > /dev/null 2>&1; then
    echo "tyclaw:x:$(id -u):$(id -g):tyclaw:/workspace:/bin/bash" >> /etc/passwd
fi
if ! getent group "$(id -g)" > /dev/null 2>&1; then
    echo "tyclaw:x:$(id -g):" >> /etc/group
fi

# pip cache 落到 bind mount，跨容器重建可复用
mkdir -p /workspace/.cache/pip
export PIP_CACHE_DIR=/workspace/.cache/pip

# 启动虚拟屏幕（后台，静默）
Xvfb :99 -screen 0 1920x1080x24 -ac +extension GLX +render -noreset > /dev/null 2>&1 &

# 等待 Xvfb 就绪
sleep 0.5

exec sleep infinity
