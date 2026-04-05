# DingTalk Gateway

钉钉消息网关，负责维护与钉钉服务器的连接，将消息分发给后端 TyClaw 实例。

## 架构

```
钉钉服务器
    ↕ (30 条 WebSocket)
  Gateway
    ↕ (M 条 WebSocket)
TyClaw 实例 1..M
```

- **上游**：Gateway 向钉钉建立 N 条 WebSocket 连接（默认 30），接收所有消息
- **下游**：TyClaw 实例主动连接 Gateway，Gateway 按 `conversation_id` hash 分发

## 快速开始

### 1. 启动 Gateway

```bash
cd tools/dingtalk-gateway
cargo run --release
```

默认读取 `config.yaml`：

```yaml
dingtalk:
  client_id: "your_app_key"
  client_secret: "your_app_secret"
  upstream_connections: 30

gateway:
  listen_addr: "0.0.0.0:9100"
  ready_wait_secs: 10        # 第一个后端连入后等待窗口
```

### 2. 连接后端（Rust 主应用）

在主应用 `config/config.yaml` 中配置：

```yaml
dingtalk:
  client_id: "your_app_key"        # 仍需要，用于下载图片和发送消息
  client_secret: "your_app_secret"
  gateway_url: "ws://localhost:9100" # 去掉此行则直连钉钉
```

### 3. 连接后端（Python Demo）

```bash
pip install websockets httpx
python3 demo_client.py --gateway ws://localhost:9100
```

## 消息协议

Gateway → 后端的 WebSocket 消息格式：

```json
{
  "type": "message",
  "message_id": "msg_xxx",
  "conversation_id": "cid_xxx",
  "sender_id": "staff_xxx",
  "data": "{原始钉钉消息 JSON}"
}
```

`data` 字段是钉钉原始消息的 JSON 字符串，解析后包含 `msgtype`、`text`、`senderNick`、`sessionWebhook` 等字段。通过 `sessionWebhook` 可直接回复消息，无需额外鉴权。

后端 → Gateway 目前只需发心跳：

```json
{"type": "heartbeat"}
```

## 路由策略

`conversation_id` 哈希取模（`hash(conversation_id) % 后端数`），同一会话始终路由到同一实例。

## 就绪机制

Gateway 启动后不立即分发消息：

1. 等待第一个后端连入
2. 开始倒计时（默认 10 秒）
3. 每有新后端连入，重置倒计时
4. 倒计时结束 → READY，开始分发
5. READY 前收到的消息丢弃

## 注意事项

- `client_id` / `client_secret` 只在 Gateway 配置，后端不直连钉钉 Stream
- 后端仍需 `client_id` / `client_secret` 用于调用钉钉 API（下载文件、主动发消息等）
- READY 后新后端连入会改变 hash mod 的 N，导致会话路由重分配
