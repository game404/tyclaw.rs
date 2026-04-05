//! 下游连接管理 —— WebSocket 服务端，接受 TyClaw 实例连接。
//!
//! 每个 TyClaw 实例建立一条 WebSocket 连接到网关。
//! 网关按会话亲和（conversation_id hash）分发消息。
//! 某个实例断开后，其绑定的会话自动重新分配。

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use futures_util::{SinkExt, StreamExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, Notify, RwLock};
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tracing::{info, warn};

use crate::upstream::IncomingMessage;

/// 下游实例信息。
struct Backend {
    id: String,
    tx: mpsc::Sender<String>,
}

/// 下游连接管理器。
pub struct DownstreamManager {
    backends: Arc<RwLock<Vec<Backend>>>,
    /// 就绪标志：所有后端连接完毕后设为 true
    ready: AtomicBool,
    /// 有新后端连入时通知就绪检测任务
    backend_connected: Notify,
    /// 就绪等待窗口（秒）
    ready_wait_secs: u64,
}

impl DownstreamManager {
    pub fn new(ready_wait_secs: u64) -> Arc<Self> {
        Arc::new(Self {
            backends: Arc::new(RwLock::new(Vec::new())),
            ready: AtomicBool::new(false),
            backend_connected: Notify::new(),
            ready_wait_secs,
        })
    }

    /// 是否就绪（所有后端已连接完毕）。
    pub fn is_ready(&self) -> bool {
        self.ready.load(Ordering::Relaxed)
    }

    /// 等待就绪。第一个后端连入后开始倒计时，窗口内无新后端则就绪。
    pub async fn wait_ready(&self) {
        // 等第一个后端连入
        loop {
            if !self.backends.read().await.is_empty() {
                break;
            }
            self.backend_connected.notified().await;
        }
        let count = self.backends.read().await.len();
        info!(backends = count, "First backend connected, starting ready window ({}s)", self.ready_wait_secs);

        // 窗口内等待：每次有新后端连入就重置计时
        loop {
            match tokio::time::timeout(
                std::time::Duration::from_secs(self.ready_wait_secs),
                self.backend_connected.notified(),
            ).await {
                Ok(()) => {
                    // 有新后端连入，重置窗口
                    let count = self.backends.read().await.len();
                    info!(backends = count, "New backend connected, resetting ready window");
                }
                Err(_) => {
                    // 窗口超时，无新后端，就绪
                    break;
                }
            }
        }

        let count = self.backends.read().await.len();
        self.ready.store(true, Ordering::Relaxed);
        info!(backends = count, "Gateway READY — dispatching messages");
    }

    /// 启动 WebSocket 服务端，接受 TyClaw 实例连接。
    pub async fn listen(self: &Arc<Self>, addr: &str) {
        let listener = TcpListener::bind(addr).await.unwrap_or_else(|e| {
            eprintln!("Failed to bind {addr}: {e}");
            std::process::exit(1);
        });
        info!(addr, "Gateway listening for TyClaw instances");

        loop {
            let (stream, peer) = match listener.accept().await {
                Ok(v) => v,
                Err(e) => {
                    warn!(error = %e, "Accept failed");
                    continue;
                }
            };
            let mgr = Arc::clone(self);
            tokio::spawn(async move {
                mgr.handle_backend(stream, peer).await;
            });
        }
    }

    /// 处理单个 TyClaw 实例的 WebSocket 连接。
    async fn handle_backend(self: &Arc<Self>, stream: TcpStream, peer: SocketAddr) {
        let ws = match tokio_tungstenite::accept_async(stream).await {
            Ok(ws) => ws,
            Err(e) => {
                warn!(peer = %peer, error = %e, "WebSocket handshake failed");
                return;
            }
        };

        let backend_id = format!("tyclaw-{}", uuid::Uuid::new_v4().to_string()[..8].to_string());
        info!(backend_id, peer = %peer, "TyClaw instance connected");

        let (mut ws_write, mut ws_read) = ws.split();
        let (tx, mut rx) = mpsc::channel::<String>(256);

        // 注册后端
        let backend_idx = {
            let mut backends = self.backends.write().await;
            let idx = backends.len();
            backends.push(Backend {
                id: backend_id.clone(),
                tx,
            });
            self.backend_connected.notify_waiters();
            idx
        };

        // 写任务：从 channel 读消息发给 TyClaw
        let write_task = tokio::spawn(async move {
            while let Some(msg) = rx.recv().await {
                if ws_write.send(WsMessage::Text(msg.into())).await.is_err() {
                    break;
                }
            }
        });

        // 读任务：TyClaw 发来的消息（心跳/状态）
        while let Some(msg) = ws_read.next().await {
            match msg {
                Ok(WsMessage::Text(text)) => {
                    // TyClaw 可以发送状态更新（如 busy/idle）
                    if text.contains("\"type\":\"ping\"") || text.contains("\"type\":\"heartbeat\"") {
                        // 心跳，忽略
                        continue;
                    }
                    // 其他消息可扩展处理
                }
                Ok(WsMessage::Ping(d)) => {
                    // 自动 pong 由 tungstenite 处理
                    let _ = d;
                }
                Ok(WsMessage::Close(_)) => break,
                Err(e) => {
                    warn!(backend_id, error = %e, "Backend read error");
                    break;
                }
                _ => {}
            }
        }

        // 清理：移除后端，重新分配其会话
        info!(backend_id, "TyClaw instance disconnected, reassigning conversations");
        write_task.abort();
        self.remove_backend(backend_idx).await;
    }

    /// 移除断开的后端。
    async fn remove_backend(&self, idx: usize) {
        let mut backends = self.backends.write().await;
        if idx < backends.len() {
            let removed_id = backends[idx].id.clone();
            backends.remove(idx);
            info!(removed_id, remaining = backends.len(), "Backend removed");
        }
    }

    /// 将消息分发给下游 TyClaw 实例。
    ///
    /// 路由策略：conversation_id 哈希取模。
    /// 同一个会话永远路由到同一个实例（只要实例数不变）。
    /// 实例数变化时部分会话会重分配，这是预期行为。
    pub async fn dispatch(&self, msg: &IncomingMessage) {
        let no_backend = if !self.is_ready() {
            warn!(message_id = %msg.message_id, "Gateway not ready, message dropped");
            true
        } else {
            self.backends.read().await.is_empty()
        };

        if no_backend {
            // 没有后端可用，直接通过 sessionWebhook 回复维护提示
            if let Ok(data) = serde_json::from_str::<serde_json::Value>(&msg.data) {
                if let Some(webhook) = data.get("sessionWebhook").and_then(|v| v.as_str()) {
                    let body = serde_json::json!({
                        "msgtype": "text",
                        "text": { "content": "请耐心等待，服务维护中..." }
                    });
                    let client = reqwest::Client::new();
                    match client.post(webhook).json(&body).timeout(std::time::Duration::from_secs(5)).send().await {
                        Ok(_) => info!(message_id = %msg.message_id, "Sent maintenance reply"),
                        Err(e) => warn!(message_id = %msg.message_id, error = %e, "Failed to send maintenance reply"),
                    }
                }
            }
            return;
        }

        let backends = self.backends.read().await;

        let target_idx = hash_route(&msg.conversation_id, backends.len());

        info!(
            message_id = %msg.message_id,
            conversation_id = %msg.conversation_id,
            sender_id = %msg.sender_id,
            target_backend = %backends[target_idx].id,
            target_idx,
            total_backends = backends.len(),
            data_len = msg.data.len(),
            "Dispatching message to backend"
        );

        let envelope = serde_json::json!({
            "type": "message",
            "message_id": msg.message_id,
            "conversation_id": msg.conversation_id,
            "sender_id": msg.sender_id,
            "data": msg.data,
        });

        if let Ok(json) = serde_json::to_string(&envelope) {
            if backends[target_idx].tx.send(json).await.is_err() {
                warn!(
                    backend = %backends[target_idx].id,
                    message_id = %msg.message_id,
                    "Failed to send to backend"
                );
            }
        }
    }
}

/// conversation_id 哈希取模，稳定路由。
fn hash_route(conversation_id: &str, n: usize) -> usize {
    let mut hasher = DefaultHasher::new();
    conversation_id.hash(&mut hasher);
    (hasher.finish() as usize) % n
}
