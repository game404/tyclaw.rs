#!/bin/bash
# TyClaw sandbox entrypoint
# 启动 Xvfb 虚拟屏幕，然后 sleep infinity 等待 docker exec

# 启动虚拟屏幕（后台，静默）
Xvfb :99 -screen 0 1920x1080x24 -ac +extension GLX +render -noreset > /dev/null 2>&1 &

# 等待 Xvfb 就绪
sleep 0.5

exec sleep infinity
