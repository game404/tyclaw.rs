#!/usr/bin/env python3
"""通过钉钉企业内部机器人批量发送 Markdown 测试消息。

该脚本仅用于显式指定收件人的人工验证，不包含默认收件人，也不会在输出中
展示完整 userId、robotCode、processQueryKey 或钉钉原始响应。
"""

from __future__ import annotations

import argparse
import json
import sys
from pathlib import Path
from typing import Any, Sequence

import requests
import yaml

DEFAULT_CONFIG = Path(__file__).resolve().parent.parent / "workspace" / "config" / "config.yaml"
TOKEN_URL = "https://api.dingtalk.com/v1.0/oauth2/accessToken"
BATCH_SEND_URL = "https://api.dingtalk.com/v1.0/robot/oToMessages/batchSend"
DEFAULT_TITLE = "多多机器人测试消息"
DEFAULT_TEXT = (
    "### 你好，这是一条来自机器人的 Markdown 消息\n"
    "- 发送方式：`oToMessages/batchSend`\n"
    "- 消息类型：**sampleMarkdown**"
)
PARTIAL_RESULT_KEYS = {
    "invalid": "invalidStaffIdList",
    "flow_controlled": "flowControlledStaffIdList",
    "filtered": "filteredStaffIdList",
}
SAFE_ERROR_CODES = {
    "staffId.notExisted",
    "InvalidAuthentication",
    "Forbidden.AccessDenied",
    "InvalidParameter",
    "Throttling.User",
    "TooManyRequests",
}


class DingTalkTestError(Exception):
    """可安全展示给终端用户的测试错误。"""


def expand_escaped_newlines(text: str) -> str:
    return text.replace("\\r\\n", "\n").replace("\\n", "\n")


def load_dingtalk_config(config_path: Path) -> dict[str, Any]:
    if not config_path.is_file():
        raise DingTalkTestError("找不到配置文件")
    try:
        with config_path.open("r", encoding="utf-8") as config_file:
            config = yaml.safe_load(config_file) or {}
    except (OSError, UnicodeError, yaml.YAMLError) as error:
        raise DingTalkTestError("配置文件无法读取或格式无效") from error

    dingtalk = config.get("dingtalk")
    if not isinstance(dingtalk, dict):
        raise DingTalkTestError("配置中缺少 dingtalk 配置块")
    if not dingtalk.get("client_id") or not dingtalk.get("client_secret"):
        raise DingTalkTestError("dingtalk.client_id / client_secret 未配置")
    return dingtalk


def _safe_field(data: object, names: Sequence[str]) -> str | None:
    if not isinstance(data, dict):
        return None
    value = next((data.get(name) for name in names if isinstance(data.get(name), str)), None)
    if not value or len(value) > 128:
        return None
    if not all(character.isascii() and (character.isalnum() or character in ".-_:") for character in value):
        return None
    return value


def _sanitized_api_error(context: str, status: int, data: object = None) -> str:
    code = _safe_field(data, ("code", "errorCode", "errcode"))
    request_id = _safe_field(data, ("requestId", "request_id", "requestid"))
    details = [f"HTTP {status}"]
    if code in SAFE_ERROR_CODES:
        details.append(f"code={code}")
        if code == "staffId.notExisted":
            details.append("配置中的钉钉收件人不存在")
    if request_id:
        details.append(f"requestId={request_id}")
    return f"{context}失败：{'；'.join(details)}"


def _json_response(response: requests.Response, context: str) -> dict[str, Any]:
    try:
        data = response.json()
    except ValueError as error:
        if response.status_code == 200:
            raise DingTalkTestError(f"{context}返回了无效 JSON 响应") from error
        raise DingTalkTestError(_sanitized_api_error(context, response.status_code)) from error
    if response.status_code != 200:
        raise DingTalkTestError(_sanitized_api_error(context, response.status_code, data))
    if not isinstance(data, dict):
        raise DingTalkTestError(f"{context}返回了无效响应")
    return data


def get_access_token(client_id: str, client_secret: str) -> str:
    try:
        response = requests.post(
            TOKEN_URL,
            json={"appKey": client_id, "appSecret": client_secret},
            headers={"Content-Type": "application/json"},
            timeout=15,
        )
    except requests.RequestException as error:
        raise DingTalkTestError("获取 accessToken 的网络请求失败") from error
    data = _json_response(response, "获取 accessToken")
    token = data.get("accessToken")
    if not isinstance(token, str) or not token:
        raise DingTalkTestError("accessToken 接口返回了无效响应")
    return token


def send_markdown(
    access_token: str,
    robot_code: str,
    user_ids: list[str],
    title: str,
    text: str,
) -> dict[str, Any]:
    payload = {
        "robotCode": robot_code,
        "userIds": user_ids,
        "msgKey": "sampleMarkdown",
        "msgParam": json.dumps({"title": title, "text": text}, ensure_ascii=False),
    }
    try:
        response = requests.post(
            BATCH_SEND_URL,
            json=payload,
            headers={
                "Content-Type": "application/json",
                "x-acs-dingtalk-access-token": access_token,
            },
            timeout=15,
        )
    except requests.RequestException as error:
        raise DingTalkTestError("发送钉钉消息的网络请求失败") from error
    return _json_response(response, "发送钉钉消息")


def _partial_counts(result: dict[str, Any]) -> dict[str, int]:
    counts: dict[str, int] = {}
    for label, key in PARTIAL_RESULT_KEYS.items():
        value = result.get(key) or []
        if not isinstance(value, list):
            raise DingTalkTestError("发送接口返回了无效的部分发送结果")
        counts[label] = len(value)
    return counts


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        description="通过钉钉企业内部机器人给显式指定的用户发送 Markdown 测试消息"
    )
    parser.add_argument(
        "--config", type=Path, default=DEFAULT_CONFIG, help="config.yaml 路径"
    )
    parser.add_argument(
        "--user-id",
        action="append",
        dest="user_ids",
        required=True,
        metavar="USERID",
        help="接收者 userId，可重复指定",
    )
    parser.add_argument("--title", default=DEFAULT_TITLE, help="Markdown 消息标题")
    parser.add_argument("--text", default=None, help="Markdown 消息正文（用 \\n 换行）")
    parser.add_argument("--text-file", type=Path, default=None, help="从文件读取 Markdown 正文")
    parser.add_argument("--robot-code", default=None, help="机器人 robotCode，默认使用 client_id")
    parser.add_argument("--dry-run", action="store_true", help="仅校验并显示脱敏后的消息概要")
    return parser


def main(argv: Sequence[str] | None = None) -> int:
    args = build_parser().parse_args(argv)
    try:
        if any(
            not user_id
            or len(user_id) > 128
            or any(character.isspace() or ord(character) < 32 or ord(character) == 127 for character in user_id)
            for user_id in args.user_ids
        ):
            raise DingTalkTestError("--user-id 包含无效值")
        dingtalk = load_dingtalk_config(args.config)
        client_id = dingtalk["client_id"]
        client_secret = dingtalk["client_secret"]
        robot_code = args.robot_code or client_id

        if args.text_file:
            try:
                text = args.text_file.read_text(encoding="utf-8")
            except (OSError, UnicodeError) as error:
                raise DingTalkTestError("Markdown 文件无法读取或不是有效 UTF-8") from error
        elif args.text is not None:
            text = expand_escaped_newlines(args.text)
        else:
            text = DEFAULT_TEXT

        if args.dry_run:
            preview = {
                "dry_run": True,
                "recipient_count": len(args.user_ids),
                "message": {
                    "type": "sampleMarkdown",
                    "title_characters": len(args.title),
                    "body_bytes": len(text.encode("utf-8")),
                },
            }
            print(json.dumps(preview, ensure_ascii=False, indent=2))
            return 0

        token = get_access_token(client_id, client_secret)
        result = send_markdown(token, robot_code, args.user_ids, args.title, text)
        counts = _partial_counts(result)
        if any(counts.values()):
            print(
                "[部分成功] "
                f"请求收件人数={len(args.user_ids)}，"
                f"无效={counts['invalid']}，"
                f"限流={counts['flow_controlled']}，"
                f"过滤={counts['filtered']}"
            )
            return 2
        print(f"[成功] 消息已提交，收件人数={len(args.user_ids)}")
        return 0
    except DingTalkTestError as error:
        print(f"[错误] {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main())
