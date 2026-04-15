---
name: 浏览器操作
description: 操控常驻 Chromium 浏览器，支持导航、截图、点击、输入、滚动，跨调用保持登录状态
triggers:
  - 打开网页
  - 浏览器
  - 截图
  - 网页操作
  - browser
  - 访问网站
  - 爬取
  - 登录网站
tool: tool.py
risk_level: write
---

# 浏览器操作

在 Docker 沙箱内操控常驻的 Chromium 浏览器。浏览器进程在容器内后台运行，
**登录状态、Cookie、页面上下文跨调用保持**，支持连续多步操作（登录→导航→填表→提交）。

## 核心用法

```bash
# 打开网页（自动截图，LLM 可看到页面）
python3 tool.py navigate --url "https://www.baidu.com"

# 点击元素（自动截图）
python3 tool.py click --selector "#login-button"
python3 tool.py click --text "登录"

# 输入文本（自动截图）
python3 tool.py type --selector "#username" --text "user@example.com"

# 模拟真人输入（随机延迟，自动截图）
python3 tool.py type --selector "#password" --text "secret" --human

# 滚动页面（自动截图）
python3 tool.py scroll --direction down
python3 tool.py scroll --pixels 800

# 单独截图
python3 tool.py screenshot
python3 tool.py screenshot --save /workspace/work/tmp/page.png

# 获取页面文本
python3 tool.py text --selector ".main-content"

# 获取当前 URL 和标题
python3 tool.py info

# 等待元素出现
python3 tool.py wait --selector ".result-list" --timeout 10

# 执行 JavaScript
python3 tool.py eval --js "document.title"

# 关闭浏览器（杀掉后台进程）
python3 tool.py close
```

## 会话持久化

浏览器通过 CDP（Chrome DevTools Protocol）常驻运行：
- **首次调用**：自动启动 Xvfb + Chromium（约 2-3 秒）
- **后续调用**：直接连接已有浏览器（毫秒级）
- **跨调用保持**：登录状态、Cookie、页面历史全部保留
- **sandbox release 后**：下次调用自动重启

## 连续操作示例

```bash
# 第1步：打开登录页
python3 tool.py navigate --url "https://example.com/login"

# 第2步：输入账号密码（浏览器还是同一个实例，页面没变）
python3 tool.py type --selector "#username" --text "admin"
python3 tool.py type --selector "#password" --text "pass123" --human

# 第3步：点击登录
python3 tool.py click --text "登录"

# 第4步：登录后导航到目标页（Cookie 已保持）
python3 tool.py navigate --url "https://example.com/dashboard"
```

## 反检测

- 非 Headless 模式（Xvfb 虚拟屏幕渲染）
- `navigator.webdriver` 伪装
- 随机 User-Agent

## 注意事项

- navigate/click/type/scroll 都会**自动截图**返回，LLM 可以看到操作后的页面
- 首次调用较慢（启动浏览器），后续调用很快
- `close` 会杀掉后台浏览器进程，下次调用重新启动
