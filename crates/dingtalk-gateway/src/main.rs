//! DingTalk Gateway —— 钉钉消息网关。
//!
//! 向钉钉服务器建立多条 WebSocket 连接（可配置），
//! 将收到的消息统一分发给多个后端 TyClaw 实例。
//!
//! 架构：
//! ```
//! DingTalk Server
//!     ↕ (N 条 WebSocket，默认 30)
//! Gateway
//!     ↕ (M 条 WebSocket，每个 TyClaw 实例一条)
//! TyClaw Instance 1..M
//! ```
//!
//! TyClaw 实例连接网关后自动接收分配的消息。
//! 同一会话（conversation_id）始终路由到同一个实例（会话亲和）。
//! 实例断开后，其会话自动重新分配到其他实例。

mod config;
mod upstream;
mod downstream;

use std::sync::Arc;

use clap::Parser;
use tokio::sync::mpsc;
use tracing::info;
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(name = "dingtalk-gateway", about = "DingTalk message gateway for TyClaw")]
struct Args {
    /// 配置文件路径
    #[arg(short, long, default_value = "config.yaml")]
    config: String,
}

#[tokio::main]
async fn main() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt().with_env_filter(filter).init();

    let args = Args::parse();
    let cfg = config::load(std::path::Path::new(&args.config));

    info!(
        upstream_connections = cfg.dingtalk.upstream_connections,
        listen_addr = %cfg.gateway.listen_addr,
        client_id = %cfg.dingtalk.client_id,
        "DingTalk Gateway starting"
    );

    // 消息队列：上游 → 分发器
    let (msg_tx, mut msg_rx) = mpsc::channel::<upstream::IncomingMessage>(1024);

    // 启动上游连接池
    upstream::start_pool(
        cfg.dingtalk.client_id,
        cfg.dingtalk.client_secret,
        cfg.dingtalk.upstream_connections,
        msg_tx,
    );

    // 启动下游管理器
    let downstream = downstream::DownstreamManager::new(cfg.gateway.ready_wait_secs);
    let downstream_for_listen = Arc::clone(&downstream);
    let listen_addr = cfg.gateway.listen_addr.clone();
    tokio::spawn(async move {
        downstream_for_listen.listen(&listen_addr).await;
    });

    // 等待后端就绪（第一个连入后开始倒计时，窗口内无新连入则就绪）
    let downstream_for_ready = Arc::clone(&downstream);
    tokio::spawn(async move {
        downstream_for_ready.wait_ready().await;
    });

    // 消息分发循环
    info!("Message dispatcher started");
    let mut total_dispatched: u64 = 0;
    loop {
        tokio::select! {
            Some(msg) = msg_rx.recv() => {
                total_dispatched += 1;
                if total_dispatched % 100 == 0 {
                    info!(total_dispatched, "Dispatch milestone");
                }
                downstream.dispatch(&msg).await;
            }
            _ = tokio::signal::ctrl_c() => {
                info!("Received SIGINT, shutting down");
                break;
            }
        }
    }
}
