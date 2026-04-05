#!/usr/bin/env python3
"""
TyClaw Gateway Demo Client (Python)

连接 dingtalk-gateway，接收钉钉消息并回复。
用法：python3 demo_client.py --gateway ws://localhost:9100

依赖：pip install websockets httpx
"""

import argparse
import asyncio
import json
import logging
import time

import httpx
import websockets

logging.basicConfig(
    level=logging.INFO,
    format="%(asctime)s %(levelname)s %(message)s",
)
log = logging.getLogger("gateway-client")


async def send_heartbeat(ws):
    """每 30 秒发一次心跳。"""
    while True:
        await asyncio.sleep(30)
        try:
            await ws.send(json.dumps({"type": "heartbeat"}))
        except Exception:
            break


async def reply_text(session_webhook: str, text: str):
    """通过 session_webhook 回复文本消息。"""
    if not session_webhook:
        log.warning("No session_webhook, cannot reply")
        return
    body = {"msgtype": "text", "text": {"content": text}}
    async with httpx.AsyncClient() as client:
        try:
            resp = await client.post(session_webhook, json=body, timeout=10)
            if resp.status_code == 200:
                log.info("Reply sent successfully")
            else:
                log.warning("Reply failed: %s %s", resp.status_code, resp.text)
        except Exception as e:
            log.error("Reply error: %s", e)


async def reply_markdown(session_webhook: str, title: str, text: str):
    """通过 session_webhook 回复 Markdown 消息。"""
    if not session_webhook:
        log.warning("No session_webhook, cannot reply")
        return
    body = {"msgtype": "markdown", "markdown": {"title": title, "text": text}}
    async with httpx.AsyncClient() as client:
        try:
            resp = await client.post(session_webhook, json=body, timeout=10)
            if resp.status_code == 200:
                log.info("Markdown reply sent successfully")
            else:
                log.warning("Reply failed: %s %s", resp.status_code, resp.text)
        except Exception as e:
            log.error("Reply error: %s", e)


async def handle_message(envelope: dict):
    """
    处理从 gateway 收到的消息。

    envelope 格式：
    {
        "type": "message",
        "message_id": "xxx",
        "conversation_id": "xxx",
        "sender_id": "xxx",
        "data": "{原始钉钉消息 JSON 字符串}"
    }

    data 解析后的关键字段：
    - msgtype: "text" / "richText" / "picture" / "file"
    - text.content: 文本内容（msgtype=text 时）
    - senderNick: 发送者昵称
    - senderStaffId: 发送者员工 ID
    - conversationId: 会话 ID
    - conversationType: "1"=单聊, "2"=群聊
    - sessionWebhook: 回复用的 webhook URL（有时效）
    """
    msg_id = envelope.get("message_id", "")
    data_str = envelope.get("data", "{}")

    try:
        data = json.loads(data_str)
    except json.JSONDecodeError:
        log.error("Failed to parse message data: %s", data_str[:200])
        return

    sender = data.get("senderNick", "unknown")
    staff_id = data.get("senderStaffId", "")
    msgtype = data.get("msgtype", "")
    session_webhook = data.get("sessionWebhook", "")
    conversation_type = data.get("conversationType", "")

    # 提取文本内容
    if msgtype == "text":
        text = data.get("text", {}).get("content", "").strip()
    elif msgtype == "richText":
        # richText 包含多个段落
        rich = data.get("content", {}).get("richText", [])
        text = "".join(item.get("text", "") for item in rich).strip()
    else:
        text = f"[{msgtype} 消息]"

    chat_type = "私聊" if conversation_type == "1" else "群聊"
    log.info("[%s] %s(%s): %s", chat_type, sender, staff_id, text)

    # ========================================
    # 在这里实现你的业务逻辑
    # ========================================
    # 示例：echo 回复
    reply = f"收到你的消息：{text}\n\n(来自 Python Demo Client)"
    await reply_text(session_webhook, reply)


async def connect_gateway(gateway_url: str):
    """连接 gateway 并持续接收消息，断开自动重连。"""
    retry_delay = 3

    while True:
        try:
            log.info("Connecting to gateway: %s", gateway_url)
            async with websockets.connect(gateway_url) as ws:
                log.info("Connected to gateway")
                retry_delay = 3  # 重置重连延迟

                # 启动心跳
                hb_task = asyncio.create_task(send_heartbeat(ws))

                try:
                    async for raw in ws:
                        try:
                            envelope = json.loads(raw)
                        except json.JSONDecodeError:
                            log.warning("Invalid JSON from gateway: %s", raw[:200])
                            continue

                        msg_type = envelope.get("type", "")
                        if msg_type == "message":
                            # 异步处理，不阻塞读循环
                            asyncio.create_task(handle_message(envelope))
                        else:
                            log.debug("Ignoring message type: %s", msg_type)
                finally:
                    hb_task.cancel()

        except (websockets.ConnectionClosed, ConnectionRefusedError, OSError) as e:
            log.warning("Gateway disconnected: %s", e)
        except Exception as e:
            log.error("Unexpected error: %s", e)

        log.info("Reconnecting in %ds...", retry_delay)
        await asyncio.sleep(retry_delay)
        retry_delay = min(retry_delay * 2, 10)


def main():
    parser = argparse.ArgumentParser(description="TyClaw Gateway Demo Client")
    parser.add_argument(
        "--gateway",
        default="ws://localhost:9100",
        help="Gateway WebSocket URL (default: ws://localhost:9100)",
    )
    args = parser.parse_args()

    asyncio.run(connect_gateway(args.gateway))


if __name__ == "__main__":
    main()
