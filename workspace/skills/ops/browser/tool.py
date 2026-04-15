#!/usr/bin/env python3
"""
TyClaw Browser Tool — persistent browser session via CDP.

浏览器进程常驻在容器内，tool.py 每次通过 CDP 连接操作，
登录状态、Cookie、页面上下文跨调用保持。

Usage:
    python3 tool.py <action> [options]

Actions:
    navigate  --url <url>                 Open a URL (auto screenshot)
    screenshot [--save <path>]            Take screenshot
    click     --selector <css> | --text <visible_text>  (auto screenshot)
    type      --selector <css> --text <value> [--human] (auto screenshot)
    scroll    --direction up|down [--pixels N]           (auto screenshot)
    wait      --selector <css> [--timeout N]
    text      --selector <css>            Get element text content
    info                                  Get current URL + title
    eval      --js <expression>           Execute JavaScript
    close                                 Close browser (kill daemon)
"""

import argparse
import base64
import json
import os
import random
import signal
import subprocess
import sys
import time

CDP_PORT = 9222
CDP_URL = f"http://127.0.0.1:{CDP_PORT}"
PID_FILE = "/tmp/.browser_daemon_pid"
XVFB_PID_FILE = "/tmp/.xvfb_daemon_pid"
STORAGE_FILE = "/tmp/.browser_storage.json"


# ---------------------------------------------------------------------------
# Xvfb + Chromium daemon management
# ---------------------------------------------------------------------------

def ensure_xvfb():
    """Ensure Xvfb is running as a background daemon."""
    if _is_pid_alive(XVFB_PID_FILE):
        return

    # Clean stale X files
    for f in ["/tmp/.X99-lock", "/tmp/.X11-unix/X99"]:
        try:
            os.remove(f)
        except OSError:
            pass

    proc = subprocess.Popen(
        ["Xvfb", ":99", "-screen", "0", "1920x1080x24", "-ac"],
        stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
    )
    _write_pid(XVFB_PID_FILE, proc.pid)

    # Wait for socket
    for _ in range(20):
        if os.path.exists("/tmp/.X11-unix/X99"):
            return
        time.sleep(0.2)


def ensure_browser():
    """Ensure Chromium is running with CDP on port 9222."""
    if _is_pid_alive(PID_FILE) and _cdp_is_reachable():
        return

    # Kill stale browser if any
    _kill_pid(PID_FILE)

    ensure_xvfb()
    os.environ["DISPLAY"] = ":99"

    # Find Chromium binary (Playwright installs it here)
    chrome = _find_chromium()

    args = [
        chrome,
        f"--remote-debugging-port={CDP_PORT}",
        "--no-first-run",
        "--no-sandbox",
        "--disable-dev-shm-usage",
        "--disable-gpu",
        "--disable-blink-features=AutomationControlled",
        "--disable-background-timer-throttling",
        "--disable-backgrounding-occluded-windows",
        "--disable-renderer-backgrounding",
        "--window-size=1920,1080",
        "--user-data-dir=/tmp/.chromium_profile",
        "about:blank",
    ]

    proc = subprocess.Popen(
        args, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
    )
    _write_pid(PID_FILE, proc.pid)

    # Wait for CDP to be reachable
    for _ in range(30):
        if _cdp_is_reachable():
            return
        time.sleep(0.3)

    raise RuntimeError("Chromium failed to start (CDP not reachable after 9s)")


def _find_chromium():
    """Find the Playwright-installed Chromium binary."""
    import glob
    patterns = [
        "/root/.cache/ms-playwright/chromium-*/chrome-linux/chrome",
        "/root/.cache/ms-playwright/chromium-*/chrome-linux64/chrome",
    ]
    for pat in patterns:
        matches = sorted(glob.glob(pat))
        if matches:
            return matches[-1]
    # Fallback
    for name in ["chromium", "chromium-browser", "google-chrome"]:
        path = subprocess.run(["which", name], capture_output=True, text=True)
        if path.returncode == 0:
            return path.stdout.strip()
    raise RuntimeError("Chromium not found")


def _cdp_is_reachable():
    """Check if CDP endpoint is responding."""
    try:
        import urllib.request
        req = urllib.request.urlopen(f"{CDP_URL}/json/version", timeout=2)
        req.close()
        return True
    except Exception:
        return False


def _is_pid_alive(pid_file):
    try:
        with open(pid_file) as f:
            pid = int(f.read().strip())
        os.kill(pid, 0)
        return True
    except (FileNotFoundError, ValueError, ProcessLookupError, PermissionError):
        return False


def _kill_pid(pid_file):
    try:
        with open(pid_file) as f:
            pid = int(f.read().strip())
        os.kill(pid, signal.SIGTERM)
        time.sleep(0.5)
        try:
            os.kill(pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
    except (FileNotFoundError, ValueError, ProcessLookupError):
        pass
    try:
        os.remove(pid_file)
    except OSError:
        pass


def _write_pid(pid_file, pid):
    with open(pid_file, "w") as f:
        f.write(str(pid))


# ---------------------------------------------------------------------------
# Connect to browser via CDP
# ---------------------------------------------------------------------------

def get_page():
    """Connect to the persistent browser via CDP and return (pw, browser, page)."""
    ensure_browser()

    from playwright.sync_api import sync_playwright
    pw = sync_playwright().start()

    browser = pw.chromium.connect_over_cdp(CDP_URL)

    # Reuse existing context/page if available, otherwise create new
    contexts = browser.contexts
    if contexts and contexts[0].pages:
        page = contexts[0].pages[0]
    else:
        context = browser.new_context(
            viewport={"width": 1920, "height": 1080},
            user_agent=random_user_agent(),
            locale="zh-CN",
            timezone_id="Asia/Shanghai",
        )
        page = context.new_page()
        # Anti-detection
        page.add_init_script("""
            Object.defineProperty(navigator, 'webdriver', { get: () => undefined });
            Object.defineProperty(navigator, 'plugins', { get: () => [1, 2, 3, 4, 5] });
            Object.defineProperty(navigator, 'languages', { get: () => ['zh-CN', 'zh', 'en'] });
            window.chrome = { runtime: {} };
        """)

    return pw, browser, page


def random_user_agent():
    agents = [
        "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36",
        "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36",
        "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36",
    ]
    return random.choice(agents)


# ---------------------------------------------------------------------------
# Auto-screenshot helper
# ---------------------------------------------------------------------------

def auto_screenshot(page):
    """Take a screenshot and return the [[IMAGE:...]] marker."""
    time.sleep(0.5)
    screenshot_bytes = page.screenshot(full_page=False)
    b64 = base64.b64encode(screenshot_bytes).decode()
    return f"\n[[IMAGE:data:image/png;base64,{b64}]]"


# ---------------------------------------------------------------------------
# Actions
# ---------------------------------------------------------------------------

def human_type(page, selector, text):
    page.click(selector)
    page.fill(selector, "")
    for char in text:
        page.keyboard.type(char)
        time.sleep(random.uniform(0.03, 0.15))


def action_navigate(page, args):
    url = args.url
    if not url.startswith(("http://", "https://")):
        url = "https://" + url
    page.goto(url, wait_until="domcontentloaded", timeout=30000)
    try:
        page.wait_for_load_state("networkidle", timeout=10000)
    except Exception:
        pass  # Some pages never reach networkidle
    info = json.dumps({
        "status": "ok", "url": page.url, "title": page.title(),
    }, ensure_ascii=False)
    return info + auto_screenshot(page)


def action_screenshot(page, args):
    if args.save:
        screenshot_bytes = page.screenshot(full_page=False)
        os.makedirs(os.path.dirname(args.save) or ".", exist_ok=True)
        with open(args.save, "wb") as f:
            f.write(screenshot_bytes)
        return json.dumps({"status": "ok", "saved": args.save, "size": len(screenshot_bytes)})
    return auto_screenshot(page)


def action_click(page, args):
    if args.text:
        page.get_by_text(args.text, exact=False).first.click()
        info = json.dumps({"status": "ok", "clicked": f"text={args.text}"})
    elif args.selector:
        page.click(args.selector, timeout=5000)
        info = json.dumps({"status": "ok", "clicked": args.selector})
    else:
        return json.dumps({"status": "error", "message": "Need --selector or --text"})
    return info + auto_screenshot(page)


def action_type(page, args):
    if not args.selector or not args.text:
        return json.dumps({"status": "error", "message": "Need --selector and --text"})
    if args.human:
        human_type(page, args.selector, args.text)
    else:
        page.fill(args.selector, args.text)
    info = json.dumps({"status": "ok", "typed": f"{len(args.text)} chars into {args.selector}"})
    return info + auto_screenshot(page)


def action_scroll(page, args):
    pixels = args.pixels or 500
    if args.direction == "up":
        pixels = -pixels
    page.mouse.wheel(0, pixels)
    info = json.dumps({"status": "ok", "scrolled": pixels})
    return info + auto_screenshot(page)


def action_wait(page, args):
    if not args.selector:
        return json.dumps({"status": "error", "message": "Need --selector"})
    timeout = (args.timeout or 10) * 1000
    try:
        page.wait_for_selector(args.selector, timeout=timeout)
        return json.dumps({"status": "ok", "found": args.selector})
    except Exception as e:
        return json.dumps({"status": "timeout", "selector": args.selector, "error": str(e)})


def action_text(page, args):
    if not args.selector:
        return json.dumps({"status": "error", "message": "Need --selector"})
    try:
        el = page.query_selector(args.selector)
        if el:
            content = el.text_content() or ""
            if len(content) > 5000:
                content = content[:5000] + f"\n... (truncated, {len(content)} chars total)"
            return json.dumps({"status": "ok", "text": content}, ensure_ascii=False)
        return json.dumps({"status": "error", "message": f"Element not found: {args.selector}"})
    except Exception as e:
        return json.dumps({"status": "error", "message": str(e)})


def action_info(page, _args):
    return json.dumps({
        "status": "ok", "url": page.url, "title": page.title(),
    }, ensure_ascii=False)


def action_eval(page, args):
    if not args.js:
        return json.dumps({"status": "error", "message": "Need --js"})
    try:
        result = page.evaluate(args.js)
        return json.dumps({"status": "ok", "result": result}, ensure_ascii=False, default=str)
    except Exception as e:
        return json.dumps({"status": "error", "message": str(e)})


def action_close(_page, _args):
    _kill_pid(PID_FILE)
    _kill_pid(XVFB_PID_FILE)
    return json.dumps({"status": "ok", "message": "Browser and Xvfb stopped"})


# ---------------------------------------------------------------------------
# Main
# ---------------------------------------------------------------------------

def main():
    parser = argparse.ArgumentParser(description="TyClaw Browser Tool")
    parser.add_argument("action", choices=[
        "navigate", "screenshot", "click", "type", "scroll",
        "wait", "text", "info", "eval", "close",
    ])
    parser.add_argument("--url", help="URL to navigate to")
    parser.add_argument("--selector", help="CSS selector")
    parser.add_argument("--text", help="Text to type or visible text to click")
    parser.add_argument("--human", action="store_true", help="Human-like typing")
    parser.add_argument("--direction", choices=["up", "down"], default="down")
    parser.add_argument("--pixels", type=int, help="Scroll pixels")
    parser.add_argument("--timeout", type=int, help="Wait timeout in seconds")
    parser.add_argument("--save", help="Save screenshot to file path")
    parser.add_argument("--js", help="JavaScript expression to evaluate")

    args = parser.parse_args()

    try:
        if args.action == "close":
            print(action_close(None, args))
            return

        pw, browser, page = get_page()

        actions = {
            "navigate": action_navigate,
            "screenshot": action_screenshot,
            "click": action_click,
            "type": action_type,
            "scroll": action_scroll,
            "wait": action_wait,
            "text": action_text,
            "info": action_info,
            "eval": action_eval,
        }
        result = actions[args.action](page, args)
        print(result)

        # Disconnect from CDP (browser stays alive)
        browser.close()
        pw.stop()

    except Exception as e:
        print(json.dumps({"status": "error", "message": str(e)}))
        sys.exit(1)


if __name__ == "__main__":
    main()
