"""
钉钉机器人 markdown 消息发送测试脚本。

从 config.yaml 读取 dingtalk.client_id / client_secret（robot_code = client_id），
通过主动消息 API（sampleMarkdown）向指定员工单聊发送内置的 markdown 测试用例。

用法：
  # 发送全部内置用例
  python3 tools/test_dingtalk_markdown.py --user-id <staffId>
  # 只列出用例标题，不发送
  python3 tools/test_dingtalk_markdown.py --list
  # 只发第 3 个用例
  python3 tools/test_dingtalk_markdown.py --user-id <staffId> --case 3
"""

import argparse
import json
import sys
from pathlib import Path

import requests
import yaml

BASE_URL = "https://api.dingtalk.com"
DEFAULT_CONFIG = Path(__file__).resolve().parent.parent / "config" / "config.yaml"


# 内置 markdown 测试用例，覆盖不同语法以直观检验钉钉端渲染效果。
TEST_CASES = [
    {
        "title": "基础语法",
        "text": (
            "# 一级标题\n"
            "## 二级标题\n\n"
            "普通段落，**加粗文本**，*斜体文本*，~~删除线~~。\n\n"
            "---\n\n"
            "上面是一条分割线。"
        ),
    },
    {
        "title": "列表（无序/有序/嵌套）",
        "text": (
            "**无序列表：**\n\n"
            "- 第一项\n"
            "- 第二项\n"
            "  - 嵌套子项 a\n"
            "  - 嵌套子项 b\n"
            "- 第三项\n\n"
            "**有序列表：**\n\n"
            "1. 步骤一\n"
            "2. 步骤二\n"
            "3. 步骤三"
        ),
    },
    {
        "title": "链接与 dtmd 跳转",
        "text": (
            "普通链接：[钉钉开放平台](https://open.dingtalk.com)\n\n"
            "dtmd 跳转（点击会以本人身份把内容发回会话）：\n\n"
            "- [点我发送“你好”](dtmd://dingtalkclient/sendMessage?content=%E4%BD%A0%E5%A5%BD)"
        ),
    },
    {
        "title": "行内代码与代码块",
        "text": (
            "行内代码：`cargo run --release`\n\n"
            "代码块：\n\n"
            "```python\n"
            "def hello(name: str) -> str:\n"
            "    return f\"hello, {name}\"\n"
            "```"
        ),
    },
    {
        "title": "引用块与 emoji",
        "text": (
            "> 这是一段引用文本。\n"
            "> 引用第二行。\n\n"
            "带 emoji：🦀 ✅ ⚠️ 🚀 📊"
        ),
    },
    {
        "title": "GFM 管道表格（已知渲染不稳，对比用例）",
        "text": (
            "| 名称 | 状态 | 备注 |\n"
            "| --- | --- | --- |\n"
            "| 任务A | ✅ 完成 | 无 |\n"
            "| 任务B | ⏳ 进行中 | 预计明天 |"
        ),
    },
    {
        # 畸形表格：6 列表头被挤成单行，且分隔行只有 4 个 |---|，
        # 列数与表头不匹配，钉钉无法识别为表格，整段压成一行显示。（数据为虚构）
        "title": "畸形表格（单行拼接 + 4 列分隔）",
        "text": (
            "「示例项目」在「上线发行渠道详情」表里共 3 条渠道明细，"
            "均为预研阶段（数据来源：示例周报.xlsx）：\n\n"
            "| 游戏名称 | 主体分类 | 上线主体 | 渠道细分 | 发行范围 | 上线时间/状态 | "
            "|---|---|---|---| | 示例游戏A | 境内主体 | 甲公司 | 微信小游戏 | 境内 | "
            "暂定，未上线，无时间规划 | | 示例游戏A | 境外主体 | 乙工作室 | Google | 境外 | "
            "2025-06 | | Пример（俄语版） | 境外主体 | 丙主体（三方支付接 示例支付） | "
            "俄罗斯包-Google | 境外 | 预计 2026年7月 |\n\n"
            "拆分要点\n\n"
            "- 境内：计划挂在甲公司主体走微信小游戏，但主体和时间都还是暂定，尚未上线。\n"
            "- 境外主流：乙工作室主体承接 Google 渠道（2025-06 已上）。\n"
            "- 境外俄区：单独用丙主体（俄罗斯本地化包 Пример），"
            "俄罗斯地区接示例支付三方结算，预计 2026年7月上线。"
        ),
    },
    {
        # 对照用例：同样内容改成钉钉稳定渲染的 bullet list（对齐 sanitize_markdown 兜底策略）。数据为虚构。
        "title": "对照修复：渠道详情（bullet list）",
        "text": (
            "**「示例项目」上线发行渠道详情**（共 3 条，均为预研阶段，"
            "数据来源：示例周报.xlsx）\n\n"
            "- **境内主体**：甲公司 ｜ 渠道 微信小游戏 ｜ 发行范围 境内 ｜ "
            "暂定，未上线，无时间规划\n"
            "- **境外主体**：乙工作室 ｜ 渠道 Google ｜ 发行范围 境外 ｜ 2025-06 已上线\n"
            "- **境外主体（俄语版 Пример）**：丙主体（三方支付接 示例支付）｜ "
            "渠道 俄罗斯包-Google ｜ 发行范围 境外 ｜ 预计 2026年7月"
        ),
    },
]


def load_credentials(config_path: Path) -> tuple[str, str]:
    """从 config.yaml 读取 client_id / client_secret。"""
    if not config_path.exists():
        print(f"[ERROR] 配置文件不存在: {config_path}", file=sys.stderr)
        sys.exit(1)
    with open(config_path, "r", encoding="utf-8") as f:
        config = yaml.safe_load(f) or {}
    dt = config.get("dingtalk", {}) or {}
    client_id = dt.get("client_id", "")
    client_secret = dt.get("client_secret", "")
    if not client_id or not client_secret:
        print(
            "[ERROR] config.yaml 中缺少 dingtalk.client_id / client_secret",
            file=sys.stderr,
        )
        sys.exit(1)
    return client_id, client_secret


def get_access_token(client_id: str, client_secret: str) -> str:
    """获取钉钉应用 access token。"""
    resp = requests.post(
        f"{BASE_URL}/v1.0/oauth2/accessToken",
        json={"appKey": client_id, "appSecret": client_secret},
        timeout=10,
    )
    resp.raise_for_status()
    data = resp.json()
    token = data.get("accessToken", "")
    if not token:
        print(f"[ERROR] 获取 token 失败: {json.dumps(data, ensure_ascii=False)}", file=sys.stderr)
        sys.exit(1)
    return token


def send_markdown(token: str, robot_code: str, user_id: str, title: str, text: str) -> bool:
    """向单个员工发送 markdown 单聊消息，返回是否成功。"""
    payload = {
        "robotCode": robot_code,
        "userIds": [user_id],
        "msgKey": "sampleMarkdown",
        "msgParam": json.dumps({"title": title, "text": text}, ensure_ascii=False),
    }
    resp = requests.post(
        f"{BASE_URL}/v1.0/robot/oToMessages/batchSend",
        headers={
            "x-acs-dingtalk-access-token": token,
            "Content-Type": "application/json",
        },
        json=payload,
        timeout=10,
    )
    body = resp.text
    ok = resp.status_code == 200
    print(f"    HTTP {resp.status_code} | {body}")
    return ok


def main() -> None:
    parser = argparse.ArgumentParser(description="钉钉机器人 markdown 消息发送测试")
    parser.add_argument("--user-id", help="接收消息的员工 staffId（发送时必填）")
    parser.add_argument("--case", type=int, help="只发送第 N 个用例（从 1 开始）")
    parser.add_argument("--list", action="store_true", help="仅列出所有内置用例标题，不发送")
    parser.add_argument("--stdin", action="store_true", help="从 stdin 读取 markdown 作为一条自定义消息发送")
    parser.add_argument("--text", help="直接传入 markdown 文本作为一条自定义消息发送")
    parser.add_argument("--title", default="表格修复测试", help="自定义消息（--stdin/--text）的标题")
    parser.add_argument("--config", help="config.yaml 路径", default=str(DEFAULT_CONFIG))
    args = parser.parse_args()

    if args.list:
        print("内置 markdown 测试用例：")
        for i, case in enumerate(TEST_CASES, 1):
            print(f"  {i}. {case['title']}")
        return

    if not args.user_id:
        print("[ERROR] 发送时必须提供 --user-id（或用 --list 查看用例）", file=sys.stderr)
        sys.exit(1)

    if args.stdin or args.text is not None:
        text = args.text if args.text is not None else sys.stdin.read()
        if not text.strip():
            print("[ERROR] 自定义消息内容为空", file=sys.stderr)
            sys.exit(1)
        cases = [{"title": args.title, "text": text}]
    elif args.case is not None:
        if not 1 <= args.case <= len(TEST_CASES):
            print(f"[ERROR] --case 超出范围，应为 1..{len(TEST_CASES)}", file=sys.stderr)
            sys.exit(1)
        cases = [TEST_CASES[args.case - 1]]
    else:
        cases = TEST_CASES

    client_id, client_secret = load_credentials(Path(args.config))
    robot_code = client_id  # robot_code 直接等于 client_id（与 tyclaw 一致）

    print("获取 access token ...")
    token = get_access_token(client_id, client_secret)
    print("token 获取成功。\n")

    failures = 0
    for case in cases:
        print(f"发送用例：{case['title']} -> {args.user_id}")
        if not send_markdown(token, robot_code, args.user_id, case["title"], case["text"]):
            failures += 1
        print()

    total = len(cases)
    print(f"完成：成功 {total - failures}/{total}，失败 {failures}。")
    if failures:
        sys.exit(1)


if __name__ == "__main__":
    main()
