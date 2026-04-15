# Docker 中搭建浏览器自动化环境方案

> 在 Docker 容器内运行完整 GUI 浏览器 + 虚拟屏幕，实现 AI 驱动的浏览器自动化操作。
> 适用于 Boss 直聘等反爬严格的网站。

---

## 一、整体架构

```
┌──────────────────────────────────────────────┐
│  Docker Container                            │
│                                              │
│  Xvfb (:99) ──── 虚拟屏幕 1920x1080         │
│    ├── Chromium (非headless，完整GUI渲染)      │
│    │     └── Playwright / Selenium 驱动       │
│    └── x11vnc ──── VNC Server (:5900)        │
│                      └── noVNC (:6080)       │
│                           ↓                  │
│              浏览器打开 localhost:6080         │
│              就能实时看到虚拟屏幕              │
└──────────────────────────────────────────────┘
```

### 核心组件说明

| 组件 | 作用 | 备注 |
|------|------|------|
| **Xvfb** | X Virtual Framebuffer，模拟一块虚拟显示器 | 浏览器以为自己在真实屏幕上渲染 |
| **Chromium（非 headless）** | 完整的 GUI 浏览器 | 指纹、行为与真人一致，规避反爬检测 |
| **Playwright** | 浏览器自动化驱动 | 比 Selenium 更现代，API 更好用 |
| **x11vnc** | 把 Xvfb 虚拟屏幕暴露为 VNC 流 | 可远程观看浏览器实时画面 |
| **noVNC** | 基于 Web 的 VNC 客户端 | 浏览器打开即可看，无需安装 VNC 客户端 |

---

## 二、为什么不用纯 Headless

| 对比项 | Headless 模式 | Xvfb + 非 Headless |
|--------|--------------|---------------------|
| 浏览器指纹 | 可被检测到 headless 标记 | 与真实浏览器完全一致 |
| `navigator.webdriver` | 默认 `true`，需要 patch | 更容易伪装 |
| Canvas / WebGL 指纹 | 部分缺失或异常 | 完整渲染，指纹正常 |
| 反爬对抗 | Boss 直聘等网站可识别 | 极难区分是否自动化 |
| 调试能力 | 只能看截图 | VNC 实时观看完整操作过程 |
| 资源占用 | 较低 | 略高（多了 Xvfb，约 50-100MB） |

---

## 三、完整 Dockerfile

```dockerfile
FROM ubuntu:22.04

ENV DEBIAN_FRONTEND=noninteractive

# 1. 基础依赖 + Xvfb + VNC
RUN apt-get update && apt-get install -y \
    xvfb \
    x11vnc \
    novnc \
    websockify \
    fluxbox \
    fonts-wqy-zenhei \
    fonts-noto-cjk \
    python3 \
    python3-pip \
    curl \
    && rm -rf /var/lib/apt/lists/*

# 2. 安装 Playwright + Chromium
RUN pip3 install playwright && \
    playwright install --with-deps chromium

# 3. 环境变量
ENV DISPLAY=:99
ENV SCREEN_WIDTH=1920
ENV SCREEN_HEIGHT=1080
ENV SCREEN_DEPTH=24

# 4. 启动脚本
COPY entrypoint.sh /entrypoint.sh
RUN chmod +x /entrypoint.sh

EXPOSE 5900 6080

ENTRYPOINT ["/entrypoint.sh"]
```

---

## 四、启动脚本 entrypoint.sh

```bash
#!/bin/bash
set -e

# 启动虚拟屏幕
Xvfb :99 -screen 0 ${SCREEN_WIDTH}x${SCREEN_HEIGHT}x${SCREEN_DEPTH} -ac &
sleep 1

# 启动轻量窗口管理器（部分网站需要）
fluxbox -display :99 &

# 启动 VNC 服务（可选，用于实时观测）
x11vnc -display :99 -forever -nopw -shared -rfbport 5900 &

# 启动 noVNC Web 客户端（可选，浏览器访问 :6080）
websockify --web /usr/share/novnc/ 6080 localhost:5900 &

echo "========================================="
echo "  虚拟屏幕已就绪: DISPLAY=:99"
echo "  VNC 端口: 5900"
echo "  noVNC Web: http://localhost:6080/vnc.html"
echo "========================================="

# 执行传入的命令（如 python3 your_script.py）
exec "$@"
```

---

## 五、构建 & 运行

```bash
# 构建镜像
docker build -t browser-bot .

# 运行（映射 VNC 端口 + 挂载脚本目录）
docker run -it --rm \
  -p 5900:5900 \
  -p 6080:6080 \
  -v $(pwd)/scripts:/app \
  browser-bot \
  python3 /app/your_script.py
```

运行后：
- 打开浏览器访问 `http://localhost:6080/vnc.html` → 实时看到虚拟屏幕上的浏览器操作
- 或用 VNC 客户端连接 `localhost:5900`

---

## 六、Python 自动化示例代码

### 6.1 基础模板

```python
from playwright.sync_api import sync_playwright
import time

with sync_playwright() as p:
    browser = p.chromium.launch(
        headless=False,           # 关键：非 headless，走 Xvfb 渲染
        args=[
            '--disable-blink-features=AutomationControlled',  # 隐藏自动化标记
            '--no-sandbox',
            '--disable-dev-shm-usage',
            '--window-size=1920,1080',
        ]
    )

    context = browser.new_context(
        viewport={'width': 1920, 'height': 1080},
        user_agent='Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 '
                   '(KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36',
        locale='zh-CN',
        timezone_id='Asia/Shanghai',
    )

    page = context.new_page()

    # 注入反检测脚本
    page.add_init_script("""
        Object.defineProperty(navigator, 'webdriver', { get: () => undefined });
    """)

    page.goto('https://www.zhipin.com')
    time.sleep(3)

    # 截图确认
    page.screenshot(path='/app/screenshot.png')

    browser.close()
```

### 6.2 反检测增强（重要）

对 Boss 直聘等严格反爬网站，建议额外处理：

```python
# 1. 模拟真人行为：随机延迟
import random

def human_type(page, selector, text):
    """模拟真人打字，每个字符间随机延迟"""
    page.click(selector)
    for char in text:
        page.keyboard.type(char)
        time.sleep(random.uniform(0.05, 0.2))

def human_click(page, selector):
    """模拟真人点击，先移动到元素附近再点"""
    element = page.locator(selector)
    box = element.bounding_box()
    if box:
        # 加入随机偏移，不要精确点中心
        x = box['x'] + box['width'] * random.uniform(0.3, 0.7)
        y = box['y'] + box['height'] * random.uniform(0.3, 0.7)
        page.mouse.move(x, y)
        time.sleep(random.uniform(0.1, 0.3))
        page.mouse.click(x, y)

# 2. 使用 stealth 插件（推荐）
# pip install playwright-stealth
from playwright_stealth import stealth_sync
stealth_sync(page)  # 一行代码注入所有反检测补丁
```

---

## 七、进阶：AI 操控浏览器的集成思路

```
┌──────────┐    截图/DOM     ┌──────────┐    指令     ┌──────────┐
│ Chromium │ ─────────────→ │  AI 大模型 │ ────────→ │ Playwright│
│ (Xvfb)  │ ←───────────── │ (视觉理解) │           │  Driver   │
└──────────┘    操作指令     └──────────┘            └──────────┘
```

核心循环：
1. **截图** → 发送给多模态大模型（GPT-4o / Claude）
2. **AI 分析**当前页面状态，决定下一步操作（点击、输入、滚动...）
3. **Playwright 执行**操作
4. **等待页面变化** → 重新截图 → 回到步骤 1

```python
# 伪代码示意
while not task_completed:
    screenshot = page.screenshot()               # 截图
    action = ai_model.decide(screenshot, goal)    # AI 决策
    execute_action(page, action)                  # 执行
    page.wait_for_load_state('networkidle')       # 等待稳定
```

---

## 八、常见问题

### Q1: 中文显示乱码？
Dockerfile 中已安装 `fonts-wqy-zenhei` 和 `fonts-noto-cjk`，覆盖中文显示。如果仍有问题：
```bash
fc-cache -fv  # 刷新字体缓存
```

### Q2: 容器内 Chromium 崩溃？
通常是共享内存不足，运行时加参数：
```bash
docker run --shm-size=2g ...
```
或在 Chromium 启动参数里加 `--disable-dev-shm-usage`（上面的示例已包含）。

### Q3: 需要持久化登录状态（Cookie）？
```python
# 保存登录状态
context.storage_state(path='/app/auth_state.json')

# 下次启动时恢复
context = browser.new_context(storage_state='/app/auth_state.json')
```

### Q4: 如何在服务器上长期运行？
建议配合 `docker-compose` + `restart: always`：
```yaml
version: '3'
services:
  browser-bot:
    build: .
    shm_size: '2g'
    ports:
      - "6080:6080"
    volumes:
      - ./scripts:/app
      - ./data:/data
    restart: always
    command: python3 /app/main.py
```

---

## 九、方案总结

| 特性 | 说明 |
|------|------|
| **浏览器模式** | 非 Headless，完整 GUI 渲染到 Xvfb 虚拟屏幕 |
| **反爬能力** | 浏览器指纹与真人一致，配合 stealth 插件增强 |
| **可观测性** | 通过 VNC / noVNC 实时观看操作过程 |
| **AI 可集成** | 截图 → 多模态 AI 决策 → Playwright 执行 |
| **资源占用** | 镜像约 1.5-2GB，运行时约 500MB-1GB 内存 |
| **适用场景** | Boss 直聘投递、社交平台操作、表单填写等需要拟人操作的任务 |
