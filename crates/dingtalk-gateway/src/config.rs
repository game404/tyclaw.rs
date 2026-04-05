use serde::Deserialize;
use std::path::Path;

#[derive(Debug, Clone, Deserialize)]
pub struct GatewayConfig {
    pub dingtalk: DingTalkConfig,
    #[serde(default)]
    pub gateway: ServerConfig,
}

#[derive(Debug, Clone, Deserialize)]
pub struct DingTalkConfig {
    pub client_id: String,
    pub client_secret: String,
    #[serde(default = "default_upstream")]
    pub upstream_connections: usize,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ServerConfig {
    #[serde(default = "default_listen")]
    pub listen_addr: String,
    /// 就绪等待窗口（秒）：第一个后端连入后等待此时间，期间无新后端连入则认为就绪。
    /// 就绪前收到的消息会缓存排队。默认 10 秒。
    #[serde(default = "default_ready_wait")]
    pub ready_wait_secs: u64,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            listen_addr: default_listen(),
            ready_wait_secs: default_ready_wait(),
        }
    }
}

fn default_upstream() -> usize { 30 }
fn default_listen() -> String { "0.0.0.0:9100".into() }
fn default_ready_wait() -> u64 { 10 }

pub fn load(path: &Path) -> GatewayConfig {
    let text = std::fs::read_to_string(path).unwrap_or_else(|e| {
        eprintln!("Failed to read {}: {e}", path.display());
        std::process::exit(1);
    });
    serde_yaml::from_str(&text).unwrap_or_else(|e| {
        eprintln!("Failed to parse {}: {e}", path.display());
        std::process::exit(1);
    })
}
