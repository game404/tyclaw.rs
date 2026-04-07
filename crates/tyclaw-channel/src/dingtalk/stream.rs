//! 钉钉 Stream 客户端 —— WebSocket 连接管理。

use futures_util::{SinkExt, StreamExt};
use reqwest::Client;
use serde::Deserialize;
use serde_json::json;
use std::collections::HashSet;
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tracing::{error, info, warn};

use super::credential::Credential;
use super::handler::ChatbotHandler;
use super::message::{AckMessage, CallbackMessage, StreamFrame};

#[derive(Debug, Deserialize)]
struct ConnectionResponse {
    endpoint: String,
    ticket: String,
}

pub struct DingTalkStreamClient {
    credential: Credential,
    http_client: Client,
    handlers: Arc<Mutex<Vec<(String, Arc<dyn ChatbotHandler>)>>>,
    /// 已处理的 msg_id 集合，用于去重（防止钉钉重复投递）。
    processed_msg_ids: Arc<Mutex<HashSet<String>>>,
}

impl DingTalkStreamClient {
    pub fn new(credential: Credential) -> Self {
        Self {
            credential,
            http_client: Client::new(),
            handlers: Arc::new(Mutex::new(Vec::new())),
            processed_msg_ids: Arc::new(Mutex::new(HashSet::new())),
        }
    }

    pub async fn register_handler(
        &self,
        topic: impl Into<String>,
        handler: Arc<dyn ChatbotHandler>,
    ) {
        let mut handlers = self.handlers.lock().await;
        handlers.push((topic.into(), handler));
    }

    async fn open_connection(&self) -> Result<ConnectionResponse, String> {
        let handlers = self.handlers.lock().await;
        let subscriptions: Vec<serde_json::Value> = handlers
            .iter()
            .map(|(topic, _)| {
                json!({
                    "type": "CALLBACK",
                    "topic": topic,
                })
            })
            .collect();
        drop(handlers);

        let resp = self
            .http_client
            .post("https://api.dingtalk.com/v1.0/gateway/connections/open")
            .json(&json!({
                "clientId": self.credential.client_id,
                "clientSecret": self.credential.client_secret,
                "subscriptions": subscriptions,
            }))
            .timeout(std::time::Duration::from_secs(10))
            .send()
            .await
            .map_err(|e| format!("Connection open failed: {e}"))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(format!("Connection API returned {status}: {body}"));
        }

        resp.json()
            .await
            .map_err(|e| format!("Connection parse error: {e}"))
    }

    async fn run_once(&self) -> Result<(), String> {
        let conn = self.open_connection().await?;
        let ws_url = format!(
            "{}?ticket={}",
            conn.endpoint,
            urlencoding::encode(&conn.ticket)
        );
        let (ws_stream, _) = tokio_tungstenite::connect_async(&ws_url)
            .await
            .map_err(|e| format!("WebSocket connect failed: {e}"))?;

        let (write, mut read) = ws_stream.split();
        let write = Arc::new(Mutex::new(write));

        // 心跳：每 60s 发 ping（与钉钉服务端心跳频率一致，避免过于频繁被判为异常）。
        // 如果 5 分钟没收到任何消息（含 pong），判定连接僵死。
        let last_activity = Arc::new(tokio::sync::Mutex::new(tokio::time::Instant::now()));
        let last_activity_ping = last_activity.clone();
        let write_ping = write.clone();
        let ping_task = tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(60));
            loop {
                interval.tick().await;
                let elapsed = last_activity_ping.lock().await.elapsed();
                if elapsed > std::time::Duration::from_secs(300) {
                    warn!(
                        elapsed_secs = elapsed.as_secs(),
                        "DingTalk Stream: no activity for 5min, connection likely dead"
                    );
                    break;
                }
                let mut w = write_ping.lock().await;
                if w.send(WsMessage::Ping(vec![].into())).await.is_err() {
                    break;
                }
            }
        });

        loop {
            // 10 分钟 read 超时：兜底，防止 read.next() 永远阻塞
            let read_result = tokio::time::timeout(
                std::time::Duration::from_secs(600),
                read.next(),
            ).await;

            match read_result {
                Err(_) => {
                    warn!("DingTalk Stream: read timeout (600s), forcing reconnect");
                    break;
                }
                Ok(None) => break, // stream ended
                Ok(Some(msg_result)) => {
                    // 更新最后活动时间
                    *last_activity.lock().await = tokio::time::Instant::now();

                    match msg_result {
                        Ok(WsMessage::Text(text)) => match serde_json::from_str::<StreamFrame>(&text) {
                            Ok(frame) => self.handle_frame(&frame, &write).await,
                            Err(e) => warn!(error = %e, "Failed to parse stream frame"),
                        },
                        Ok(WsMessage::Ping(data)) => {
                            let mut w = write.lock().await;
                            let _ = w.send(WsMessage::Pong(data)).await;
                        }
                        Ok(WsMessage::Close(reason)) => {
                            warn!(reason = ?reason, "DingTalk Stream: WebSocket closed by server");
                            break;
                        }
                        Err(e) => {
                            error!(error = %e, "DingTalk Stream: WebSocket error");
                            break;
                        }
                        _ => {}
                    }
                }
            }
        }

        ping_task.abort();
        Err("WebSocket disconnected".to_string())
    }

    async fn handle_frame(
        &self,
        frame: &StreamFrame,
        write: &Arc<
            Mutex<
                futures_util::stream::SplitSink<
                    tokio_tungstenite::WebSocketStream<
                        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
                    >,
                    WsMessage,
                >,
            >,
        >,
    ) {
        let topic = frame.topic().to_string();

        if frame.msg_type == "SYSTEM" {
            let ack = AckMessage::ok(frame.headers.clone());
            self.send_ack(write, &ack).await;
            return;
        }

        let handlers = self.handlers.lock().await;
        let handler = handlers
            .iter()
            .find(|(t, _)| *t == topic)
            .map(|(_, h)| h.clone());
        drop(handlers);

        if let Some(handler) = handler {
            let ack = AckMessage::ok(frame.headers.clone());
            self.send_ack(write, &ack).await;

            let msg_id = frame.message_id().to_string();

            // 消息去重：防止钉钉重复投递
            {
                let mut seen = self.processed_msg_ids.lock().await;
                if !seen.insert(msg_id.clone()) {
                    warn!(msg_id = %msg_id, "DingTalk Stream: duplicate message, skipping");
                    return;
                }
                // 防止内存无限增长，保留最近 1000 条
                if seen.len() > 1000 {
                    seen.clear();
                }
            }

            let callback = CallbackMessage::from_frame(frame);
            info!(msg_id = %msg_id, topic = %topic, "DingTalk Stream: dispatching message (async)");
            tokio::spawn(async move {
                let start = std::time::Instant::now();
                let (code, _message) = handler.process(&callback).await;
                let elapsed = start.elapsed().as_secs_f64();
                info!(
                    msg_id = %msg_id,
                    code,
                    elapsed_secs = elapsed,
                    "DingTalk Stream: handler completed"
                );
            });
        } else {
            warn!(topic = %topic, "DingTalk Stream: no handler for topic");
            let ack = AckMessage {
                code: AckMessage::STATUS_NOT_FOUND,
                headers: frame.headers.clone(),
                message: "No handler".into(),
                data: String::new(),
            };
            self.send_ack(write, &ack).await;
        }
    }

    async fn send_ack(
        &self,
        write: &Arc<
            Mutex<
                futures_util::stream::SplitSink<
                    tokio_tungstenite::WebSocketStream<
                        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
                    >,
                    WsMessage,
                >,
            >,
        >,
        ack: &AckMessage,
    ) {
        if let Ok(text) = serde_json::to_string(ack) {
            let mut w = write.lock().await;
            if let Err(e) = w.send(WsMessage::Text(text.into())).await {
                warn!(error = %e, "Failed to send ACK");
            }
        }
    }

    pub async fn start_forever(&self) {
        let mut retry_delay = 3u64;
        loop {
            match self.run_once().await {
                Ok(()) => break,
                Err(e) => {
                    error!(
                        error = %e,
                        retry_in = retry_delay,
                        "DingTalk Stream: disconnected, reconnecting in {retry_delay}s..."
                    );
                    tokio::time::sleep(std::time::Duration::from_secs(retry_delay)).await;
                    retry_delay = (retry_delay * 2).min(10);
                }
            }
        }
    }
}
