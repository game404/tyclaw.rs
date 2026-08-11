//! 钉钉主动消息工具：从受控配置读取收件人并批量发送 Markdown。

use async_trait::async_trait;
use parking_lot::Mutex as ParkingMutex;
use reqwest::{Client, StatusCode};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::Mutex;
use tracing::{info, warn};

use crate::base::{brief_truncate, RiskLevel, Tool};
use crate::executor::{current_user_id, current_user_name};
use tyclaw_tool_abi::Sandbox;

const TOKEN_URL: &str = "https://api.dingtalk.com/v1.0/oauth2/accessToken";
const BATCH_SEND_URL: &str = "https://api.dingtalk.com/v1.0/robot/oToMessages/batchSend";
const DEFAULT_RECIPIENT_SECTION: &str = "finance-fund-daily-dingtalk";
const RECIPIENT_CONFIG_RELATIVE_PATH: &str = ".config/ty.config.toml";

async fn parse_json_response(response: reqwest::Response) -> Result<Value, String> {
    response
        .json::<Value>()
        .await
        .map_err(|_| "DingTalk API returned an invalid JSON response".to_string())
}

fn safe_response_field(data: Option<&Value>, names: &[&str]) -> Option<String> {
    let value = names.iter().find_map(|name| data?.get(*name)?.as_str())?;
    if value.is_empty()
        || value.len() > 128
        || !value
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '-' | '_' | ':'))
    {
        return None;
    }
    Some(value.to_string())
}

fn sanitized_api_error(context: &str, status: StatusCode, data: Option<&Value>) -> String {
    let code = safe_response_field(data, &["code", "errorCode", "errcode"]).filter(|code| {
        matches!(
            code.as_str(),
            "staffId.notExisted"
                | "InvalidAuthentication"
                | "Forbidden.AccessDenied"
                | "InvalidParameter"
                | "Throttling.User"
                | "TooManyRequests"
        )
    });
    let request_id = safe_response_field(data, &["requestId", "request_id", "requestid"]);

    let mut details = vec![format!("HTTP {}", status.as_u16())];
    if let Some(code) = code.as_deref() {
        details.push(format!("code={code}"));
        if code == "staffId.notExisted" {
            details.push("a configured DingTalk recipient does not exist".to_string());
        }
    }
    if let Some(request_id) = request_id {
        details.push(format!("requestId={request_id}"));
    }
    format!("{context} failed: {}", details.join("; "))
}

fn default_max_recipients() -> usize {
    20
}

fn default_max_markdown_bytes() -> usize {
    20_000
}

fn default_timeout_secs() -> u64 {
    15
}

fn default_idempotency_ttl_secs() -> u64 {
    600
}

fn default_per_user_cooldown_secs() -> u64 {
    60
}

fn default_recipient_sections() -> Vec<String> {
    vec![DEFAULT_RECIPIENT_SECTION.to_string()]
}

/// `config.yaml` 中的 `dingtalk.outbound` 配置。
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct DingTalkOutboundConfig {
    pub enabled: bool,
    pub max_recipients: usize,
    pub max_markdown_bytes: usize,
    pub timeout_secs: u64,
    pub idempotency_ttl_secs: u64,
    pub per_user_cooldown_secs: u64,
    /// 允许 Tool 读取的 `ty.config.toml` 配置节白名单。
    pub recipient_sections: Vec<String>,
}

impl Default for DingTalkOutboundConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            max_recipients: default_max_recipients(),
            max_markdown_bytes: default_max_markdown_bytes(),
            timeout_secs: default_timeout_secs(),
            idempotency_ttl_secs: default_idempotency_ttl_secs(),
            per_user_cooldown_secs: default_per_user_cooldown_secs(),
            recipient_sections: default_recipient_sections(),
        }
    }
}

/// 钉钉应用凭证。
#[derive(Clone)]
pub struct Credential {
    pub client_id: String,
    pub client_secret: String,
}

impl std::fmt::Debug for Credential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Credential")
            .field("client_id", &self.client_id)
            .field("client_secret", &"***")
            .finish()
    }
}

impl Credential {
    pub fn new(client_id: impl Into<String>, client_secret: impl Into<String>) -> Self {
        Self {
            client_id: client_id.into(),
            client_secret: client_secret.into(),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct TokenResponse {
    access_token: String,
    expire_in: Option<u64>,
}

struct TokenState {
    token: String,
    expiry: f64,
}

/// 线程安全的钉钉 access token 缓存。
#[derive(Clone)]
pub struct TokenManager {
    credential: Credential,
    client: Client,
    token_url: String,
    state: Arc<Mutex<TokenState>>,
}

impl TokenManager {
    pub fn new(credential: Credential) -> Self {
        Self {
            credential,
            client: Client::new(),
            token_url: TOKEN_URL.to_string(),
            state: Arc::new(Mutex::new(TokenState {
                token: String::new(),
                expiry: 0.0,
            })),
        }
    }

    pub async fn get_token(&self) -> Result<String, String> {
        let mut state = self.state.lock().await;
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs_f64();

        if !state.token.is_empty() && now < state.expiry {
            return Ok(state.token.clone());
        }

        info!("Refreshing DingTalk access token");
        let resp = self
            .client
            .post(&self.token_url)
            .json(&json!({
                "appKey": self.credential.client_id,
                "appSecret": self.credential.client_secret,
            }))
            .timeout(Duration::from_secs(10))
            .send()
            .await
            .map_err(|_| "DingTalk token request failed".to_string())?;

        if !resp.status().is_success() {
            let status = resp.status();
            let data = parse_json_response(resp).await.ok();
            return Err(sanitized_api_error(
                "DingTalk token API",
                status,
                data.as_ref(),
            ));
        }

        let token_resp: TokenResponse = parse_json_response(resp).await.and_then(|value| {
            serde_json::from_value(value)
                .map_err(|_| "DingTalk token API returned an invalid response".to_string())
        })?;
        if token_resp.access_token.is_empty() {
            return Err("DingTalk token API returned an invalid response".to_string());
        }
        let expire_in = token_resp.expire_in.unwrap_or(7200);
        state.token = token_resp.access_token.clone();
        state.expiry = now + expire_in as f64 - 60.0;
        info!(expire_in, "DingTalk token refreshed");
        Ok(token_resp.access_token)
    }

    pub async fn reset(&self) {
        let mut state = self.state.lock().await;
        state.token.clear();
        state.expiry = 0.0;
        warn!("DingTalk token cache reset");
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Recipient {
    name: String,
    user_id: String,
}

/// 从固定配置节向固定人员发送钉钉 Markdown 消息。
pub struct SendDingTalkMessageTool {
    config: DingTalkOutboundConfig,
    token_manager: TokenManager,
    robot_code: String,
    client: Client,
    batch_send_url: String,
    send_lock: Mutex<()>,
    sent: ParkingMutex<HashMap<u64, Instant>>,
    last_sent_by_user: ParkingMutex<HashMap<String, Instant>>,
}

impl SendDingTalkMessageTool {
    pub fn new(
        config: DingTalkOutboundConfig,
        token_manager: TokenManager,
        robot_code: impl Into<String>,
    ) -> Self {
        Self {
            config,
            token_manager,
            robot_code: robot_code.into(),
            client: Client::new(),
            batch_send_url: BATCH_SEND_URL.to_string(),
            send_lock: Mutex::new(()),
            sent: ParkingMutex::new(HashMap::new()),
            last_sent_by_user: ParkingMutex::new(HashMap::new()),
        }
    }

    fn allowed_section(&self, section: &str) -> bool {
        self.config
            .recipient_sections
            .iter()
            .any(|allowed| allowed == section)
    }

    #[cfg(test)]
    fn load_recipients_from_path(path: &Path, section: &str) -> Result<Vec<Recipient>, String> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| format!("Failed to read ty.config.toml: {e}"))?;
        Self::load_recipients_from_text(&text, section)
    }

    fn load_recipients_from_text(text: &str, section: &str) -> Result<Vec<Recipient>, String> {
        let root: toml::Value = toml::from_str(text)
            .map_err(|_| "Failed to parse ty.config.toml: invalid TOML syntax".to_string())?;
        let section_value = root
            .get(section)
            .ok_or_else(|| format!("ty.config.toml is missing [{section}]"))?;
        let to = section_value
            .get("to")
            .ok_or_else(|| format!("ty.config.toml [{section}] is missing 'to'"))?;

        let raw_entries: Vec<String> = match to {
            toml::Value::Array(values) => values
                .iter()
                .map(|value| {
                    value
                        .as_str()
                        .map(str::to_string)
                        .ok_or_else(|| format!("[{section}].to must contain only strings"))
                })
                .collect::<Result<_, _>>()?,
            toml::Value::String(value) => value
                .split([',', ';'])
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_string)
                .collect(),
            _ => {
                return Err(format!(
                    "ty.config.toml [{section}].to must be a string or string array"
                ))
            }
        };

        let mut recipients = Vec::new();
        let mut seen_ids = HashSet::new();
        for (index, entry) in raw_entries.into_iter().enumerate() {
            let (name, user_id) = entry.split_once('|').ok_or_else(|| {
                format!(
                    "Invalid recipient at position {}; expected '姓名|userId'",
                    index + 1
                )
            })?;
            let name = name.trim();
            let user_id = user_id.trim();
            if name.is_empty() || user_id.is_empty() {
                return Err(format!(
                    "Invalid recipient at position {}; name and userId must both be non-empty",
                    index + 1
                ));
            }
            if user_id.len() > 128
                || user_id
                    .chars()
                    .any(|ch| ch.is_whitespace() || ch.is_control() || ch == '|')
            {
                return Err(format!(
                    "Invalid userId configured at recipient position {}",
                    index + 1
                ));
            }
            if seen_ids.insert(user_id.to_string()) {
                recipients.push(Recipient {
                    name: name.to_string(),
                    user_id: user_id.to_string(),
                });
            }
        }

        if recipients.is_empty() {
            return Err(format!("ty.config.toml [{section}].to is empty"));
        }
        Ok(recipients)
    }

    async fn load_recipients_from_sandbox(
        &self,
        sandbox: &dyn Sandbox,
        section: &str,
    ) -> Result<Vec<Recipient>, String> {
        if !self.allowed_section(section) {
            return Err(format!(
                "Recipient config section '{section}' is not allowed by dingtalk.outbound.recipient_sections"
            ));
        }

        let config_path = Path::new(sandbox.workspace_root())
            .parent()
            .ok_or_else(|| "Current workspace root has no parent directory".to_string())?
            .join(RECIPIENT_CONFIG_RELATIVE_PATH);
        let config_path = config_path.to_string_lossy();
        let bytes = sandbox.read_file(&config_path).await.map_err(|_| {
            format!(
                "ty.config.toml not found in current workspace ({RECIPIENT_CONFIG_RELATIVE_PATH})"
            )
        })?;
        let text = std::str::from_utf8(&bytes)
            .map_err(|_| "Current workspace ty.config.toml is not valid UTF-8".to_string())?;
        Self::load_recipients_from_text(text, section)
    }

    fn normalize_text_file_path(path: &str) -> Result<PathBuf, String> {
        let path = Path::new(path);
        if path.as_os_str().is_empty() || path.is_absolute() {
            return Err("text_file must be relative to the workspace work directory".to_string());
        }

        let mut normalized = PathBuf::new();
        for component in path.components() {
            match component {
                Component::Normal(part) => normalized.push(part),
                Component::CurDir => {}
                Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                    return Err(
                        "text_file must stay within the workspace work directory".to_string()
                    )
                }
            }
        }
        if normalized.starts_with("work") {
            normalized = normalized
                .strip_prefix("work")
                .unwrap_or(&normalized)
                .to_path_buf();
        }
        if normalized.as_os_str().is_empty() {
            return Err(
                "text_file must identify a file in the workspace work directory".to_string(),
            );
        }
        Ok(normalized)
    }

    fn parse_common(
        &self,
        params: &HashMap<String, Value>,
    ) -> Result<(String, String, String), String> {
        for forbidden in ["to", "userIds", "user_ids", "recipients"] {
            if params.contains_key(forbidden) {
                return Err(format!(
                    "'{forbidden}' is not accepted; DingTalk recipients must come from the approved ty.config.toml section"
                ));
            }
        }
        let section = params
            .get("recipient_config")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| "'recipient_config' is required".to_string())?;
        let title = params
            .get("title")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| "'title' is required".to_string())?;
        let text = params
            .get("text")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        Ok((section.to_string(), title.to_string(), text))
    }

    fn append_sender_attribution(text: String) -> String {
        let name = current_user_name();
        let sender = if name.trim().is_empty() {
            current_user_id()
        } else {
            name
        };
        let sender = sender.replace(['\r', '\n'], " ");
        if sender.trim().is_empty() {
            text
        } else {
            format!("{text}\n\n---\n汇报人：{sender}\n由多多代为发送")
        }
    }

    fn validate_message(
        &self,
        recipients: &[Recipient],
        title: &str,
        text: &str,
    ) -> Result<(), String> {
        if recipients.len() > self.config.max_recipients.max(1) {
            return Err(format!(
                "Recipient count {} exceeds configured maximum {}",
                recipients.len(),
                self.config.max_recipients.max(1)
            ));
        }
        if title.chars().count() > 200 {
            return Err("Title exceeds 200 characters".to_string());
        }
        if text.trim().is_empty() {
            return Err("Either 'text' or 'text_file' must contain Markdown".to_string());
        }
        if text.len() > self.config.max_markdown_bytes.max(1) {
            return Err(format!(
                "Markdown body is {} bytes; configured maximum is {} bytes",
                text.len(),
                self.config.max_markdown_bytes.max(1)
            ));
        }
        Ok(())
    }

    fn idempotency_key(recipients: &[Recipient], title: &str, text: &str) -> u64 {
        let mut ids: Vec<&str> = recipients.iter().map(|r| r.user_id.as_str()).collect();
        ids.sort_unstable();
        let mut hasher = DefaultHasher::new();
        current_user_id().hash(&mut hasher);
        ids.hash(&mut hasher);
        title.hash(&mut hasher);
        text.hash(&mut hasher);
        hasher.finish()
    }

    fn check_idempotency(&self, key: u64, now: Instant) -> bool {
        let ttl = Duration::from_secs(self.config.idempotency_ttl_secs.max(1));
        let mut sent = self.sent.lock();
        sent.retain(|_, timestamp| now.saturating_duration_since(*timestamp) < ttl);
        sent.contains_key(&key)
    }

    fn check_cooldown(&self, now: Instant) -> Result<(), String> {
        let cooldown_secs = self.config.per_user_cooldown_secs;
        if cooldown_secs == 0 {
            return Ok(());
        }
        let user_id = current_user_id();
        if user_id.is_empty() {
            return Ok(());
        }
        let cooldown = Duration::from_secs(cooldown_secs);
        let last_sent = self.last_sent_by_user.lock();
        if let Some(previous) = last_sent.get(&user_id) {
            let elapsed = now.saturating_duration_since(*previous);
            if elapsed < cooldown {
                return Err(format!(
                    "DingTalk outbound cooldown active; retry after {} seconds",
                    cooldown.saturating_sub(elapsed).as_secs().max(1)
                ));
            }
        }
        Ok(())
    }

    fn batch_payload(&self, recipients: &[Recipient], title: &str, text: &str) -> Value {
        let user_ids: Vec<&str> = recipients
            .iter()
            .map(|recipient| recipient.user_id.as_str())
            .collect();
        json!({
            "robotCode": self.robot_code,
            "userIds": user_ids,
            "msgKey": "sampleMarkdown",
            "msgParam": json!({"title": title, "text": text}).to_string(),
        })
    }

    async fn deliver(
        &self,
        recipients: &[Recipient],
        title: &str,
        text: &str,
    ) -> Result<Value, String> {
        let payload = self.batch_payload(recipients, title, text);

        for attempt in 0..2 {
            let token = self.token_manager.get_token().await?;
            let resp = self
                .client
                .post(&self.batch_send_url)
                .header("Content-Type", "application/json")
                .header("x-acs-dingtalk-access-token", token)
                .json(&payload)
                .timeout(Duration::from_secs(self.config.timeout_secs.max(1)))
                .send()
                .await
                .map_err(|_| "DingTalk batchSend request failed".to_string())?;

            if resp.status() == StatusCode::UNAUTHORIZED && attempt == 0 {
                self.token_manager.reset().await;
                continue;
            }
            let status = resp.status();
            let data = parse_json_response(resp).await;
            if !status.is_success() {
                return Err(sanitized_api_error(
                    "DingTalk batchSend API",
                    status,
                    data.as_ref().ok(),
                ));
            }
            let data = data?;
            if !data.is_object() {
                return Err("DingTalk batchSend API returned an invalid response".to_string());
            }
            return Ok(data);
        }
        Err("DingTalk batchSend authorization failed after token refresh".to_string())
    }

    fn names_for_ids(recipients: &[Recipient], ids: &[String]) -> Vec<String> {
        let id_set: HashSet<&str> = ids.iter().map(String::as_str).collect();
        recipients
            .iter()
            .filter(|recipient| id_set.contains(recipient.user_id.as_str()))
            .map(|recipient| recipient.name.clone())
            .collect()
    }

    async fn send(
        &self,
        section: &str,
        recipients: Vec<Recipient>,
        title: &str,
        text: String,
    ) -> String {
        if !self.config.enabled {
            return "Error: DingTalk outbound messaging is disabled".to_string();
        }
        let text = Self::append_sender_attribution(text);
        if let Err(error) = self.validate_message(&recipients, title, &text) {
            return format!("Error: {error}");
        }

        let key = Self::idempotency_key(&recipients, title, &text);
        let _guard = self.send_lock.lock().await;
        let now = Instant::now();
        if self.check_idempotency(key, now) {
            return json!({
                "status": "duplicate_suppressed",
                "recipient_config": section,
                "recipients": recipients.iter().map(|r| r.name.as_str()).collect::<Vec<_>>(),
            })
            .to_string();
        }
        if let Err(error) = self.check_cooldown(now) {
            return format!("Error: {error}");
        }

        let response = match self.deliver(&recipients, title, &text).await {
            Ok(response) => response,
            Err(error) => return format!("Error: {error}"),
        };
        self.sent.lock().insert(key, now);
        let caller_id = current_user_id();
        if !caller_id.is_empty() {
            self.last_sent_by_user.lock().insert(caller_id, now);
        }

        let list = |key: &str| -> Vec<String> {
            response
                .get(key)
                .and_then(Value::as_array)
                .map(|values| {
                    values
                        .iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default()
        };
        let invalid = list("invalidStaffIdList");
        let flow_controlled = list("flowControlledStaffIdList");
        let filtered = list("filteredStaffIdList");
        let partial = !invalid.is_empty() || !flow_controlled.is_empty() || !filtered.is_empty();

        json!({
            "status": if partial { "partial" } else { "ok" },
            "recipient_config": section,
            "recipients": recipients.iter().map(|r| r.name.as_str()).collect::<Vec<_>>(),
            "invalid_recipients": Self::names_for_ids(&recipients, &invalid),
            "flow_controlled_recipients": Self::names_for_ids(&recipients, &flow_controlled),
            "filtered_recipients": Self::names_for_ids(&recipients, &filtered),
        })
        .to_string()
    }
}

#[async_trait]
impl Tool for SendDingTalkMessageTool {
    fn name(&self) -> &str {
        "send_dingtalk_message"
    }

    fn description(&self) -> &str {
        "Send one Markdown report to a fixed DingTalk recipient list loaded from an approved \
         ty.config.toml section. Recipients cannot be supplied or overridden in tool arguments."
    }

    fn brief(&self, args: &HashMap<String, Value>) -> Option<String> {
        let section = args
            .get("recipient_config")
            .and_then(Value::as_str)
            .unwrap_or("");
        let title = args.get("title").and_then(Value::as_str).unwrap_or("");
        Some(brief_truncate(
            &format!("send_dingtalk_message -> [{section}] | {title}"),
            80,
        ))
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "recipient_config": {
                    "type": "string",
                    "enum": self.config.recipient_sections,
                    "description": "Approved ty.config.toml section containing the fixed 'to' recipient list."
                },
                "title": {
                    "type": "string",
                    "description": "DingTalk Markdown message title."
                },
                "text": {
                    "type": "string",
                    "description": "Markdown body. Ignored when text_file is set."
                },
                "text_file": {
                    "type": "string",
                    "description": "UTF-8 Markdown file confined to the current workspace work/ directory. A leading work/ is accepted for compatibility. Overrides text when set."
                }
            },
            "required": ["recipient_config", "title"]
        })
    }

    fn risk_level(&self) -> RiskLevel {
        RiskLevel::Write
    }

    fn should_sandbox(&self) -> bool {
        true
    }

    async fn execute(&self, _params: HashMap<String, Value>) -> String {
        "Error: DingTalk outbound messaging requires the current workspace sandbox".to_string()
    }

    async fn execute_in_sandbox(
        &self,
        sandbox: &dyn Sandbox,
        params: HashMap<String, Value>,
    ) -> String {
        let (section, title, mut text) = match self.parse_common(&params) {
            Ok(parts) => parts,
            Err(error) => return format!("Error: {error}"),
        };
        if let Some(path) = params.get("text_file").and_then(Value::as_str) {
            let path = match Self::normalize_text_file_path(path) {
                Ok(path) => path,
                Err(error) => return format!("Error: {error}"),
            };
            text = match sandbox
                .read_workspace_file(
                    &path.to_string_lossy(),
                    self.config.max_markdown_bytes.max(1),
                )
                .await
            {
                Ok(bytes) => match String::from_utf8(bytes) {
                    Ok(text) => text,
                    Err(_) => return "Error: text_file is not valid UTF-8".to_string(),
                },
                Err(error) => return format!("Error: Failed to read text_file: {error}"),
            };
        }
        let recipients = match self.load_recipients_from_sandbox(sandbox, &section).await {
            Ok(recipients) => recipients,
            Err(error) => return format!("Error: {error}"),
        };
        self.send(&section, recipients, &title, text).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::thread;
    use tyclaw_tool_abi::SandboxPool;

    fn config() -> DingTalkOutboundConfig {
        DingTalkOutboundConfig {
            enabled: true,
            per_user_cooldown_secs: 0,
            ..Default::default()
        }
    }

    fn tool(_workspace: &Path) -> SendDingTalkMessageTool {
        SendDingTalkMessageTool::new(
            config(),
            TokenManager::new(Credential::new("app", "secret")),
            "app",
        )
    }

    fn test_tool(token_url: &str, batch_send_url: &str) -> SendDingTalkMessageTool {
        let mut token_manager = TokenManager::new(Credential::new("app", "secret"));
        token_manager.token_url = token_url.to_string();
        let mut tool = SendDingTalkMessageTool::new(config(), token_manager, "app");
        tool.batch_send_url = batch_send_url.to_string();
        tool
    }

    fn spawn_server(responses: Vec<(u16, &'static str)>) -> (String, thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = format!("http://{}", listener.local_addr().unwrap());
        let handle = thread::spawn(move || {
            for (status, body) in responses {
                let (mut stream, _) = listener.accept().unwrap();
                let mut request = Vec::new();
                let mut buffer = [0_u8; 4096];
                loop {
                    let read = stream.read(&mut buffer).unwrap();
                    if read == 0 {
                        break;
                    }
                    request.extend_from_slice(&buffer[..read]);
                    let Some(header_end) = request.windows(4).position(|part| part == b"\r\n\r\n")
                    else {
                        continue;
                    };
                    let headers = String::from_utf8_lossy(&request[..header_end]);
                    let content_length = headers
                        .lines()
                        .find_map(|line| {
                            line.split_once(':').and_then(|(name, value)| {
                                name.eq_ignore_ascii_case("content-length")
                                    .then(|| value.trim().parse::<usize>().ok())
                                    .flatten()
                            })
                        })
                        .unwrap_or(0);
                    if request.len() >= header_end + 4 + content_length {
                        break;
                    }
                }
                let reason = if status == 200 { "OK" } else { "Error" };
                write!(
                    stream,
                    "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .unwrap();
            }
        });
        (address, handle)
    }

    async fn sandbox_for(workspace_root: &Path) -> Arc<dyn Sandbox> {
        let work_dir = workspace_root.join("work");
        std::fs::create_dir_all(&work_dir).unwrap();
        let pool = tyclaw_sandbox::NoopPool::new(workspace_root.to_path_buf());
        pool.acquire("test-workspace", &work_dir, &[])
            .await
            .unwrap()
    }

    fn write_workspace_config(workspace_root: &Path, text: &str) {
        let config_dir = workspace_root.join(".config");
        std::fs::create_dir_all(&config_dir).unwrap();
        std::fs::write(config_dir.join("ty.config.toml"), text).unwrap();
    }

    #[test]
    fn parses_and_deduplicates_configured_recipients() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ty.config.toml");
        let mut file = std::fs::File::create(&path).unwrap();
        writeln!(
            file,
            "[finance-fund-daily-dingtalk]\nto = [\"张三|u1\", \"李四|u2\", \"重复|u1\"]"
        )
        .unwrap();

        let recipients =
            SendDingTalkMessageTool::load_recipients_from_path(&path, DEFAULT_RECIPIENT_SECTION)
                .unwrap();
        assert_eq!(
            recipients,
            vec![
                Recipient {
                    name: "张三".into(),
                    user_id: "u1".into()
                },
                Recipient {
                    name: "李四".into(),
                    user_id: "u2".into()
                }
            ]
        );
    }

    #[test]
    fn rejects_malformed_recipient_atomically() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ty.config.toml");
        std::fs::write(
            &path,
            "[finance-fund-daily-dingtalk]\nto = [\"张三|u1\", \"李四\"]\n",
        )
        .unwrap();
        let error =
            SendDingTalkMessageTool::load_recipients_from_path(&path, DEFAULT_RECIPIENT_SECTION)
                .unwrap_err();
        assert!(error.contains("expected '姓名|userId'"));
        assert!(!error.contains("u1"));
    }

    #[tokio::test]
    async fn sandbox_reloads_recipients_after_config_changes() {
        let dir = tempfile::tempdir().unwrap();
        write_workspace_config(
            dir.path(),
            "[finance-fund-daily-dingtalk]\nto = [\"甲|test-user-a\"]\n",
        );
        let sandbox = sandbox_for(dir.path()).await;
        let tool = tool(dir.path());

        let first = tool
            .load_recipients_from_sandbox(sandbox.as_ref(), DEFAULT_RECIPIENT_SECTION)
            .await
            .unwrap();
        assert_eq!(first[0].user_id, "test-user-a");

        write_workspace_config(
            dir.path(),
            "[finance-fund-daily-dingtalk]\nto = [\"乙|test-user-b\"]\n",
        );
        let second = tool
            .load_recipients_from_sandbox(sandbox.as_ref(), DEFAULT_RECIPIENT_SECTION)
            .await
            .unwrap();
        assert_eq!(second[0].user_id, "test-user-b");
    }

    #[tokio::test]
    async fn sandbox_recipient_configs_are_isolated_by_workspace() {
        let first_dir = tempfile::tempdir().unwrap();
        let second_dir = tempfile::tempdir().unwrap();
        write_workspace_config(
            first_dir.path(),
            "[finance-fund-daily-dingtalk]\nto = [\"甲|workspace-a-user\"]\n",
        );
        write_workspace_config(
            second_dir.path(),
            "[finance-fund-daily-dingtalk]\nto = [\"乙|workspace-b-user\"]\n",
        );
        let first_sandbox = sandbox_for(first_dir.path()).await;
        let second_sandbox = sandbox_for(second_dir.path()).await;
        let tool = tool(first_dir.path());

        let first = tool
            .load_recipients_from_sandbox(first_sandbox.as_ref(), DEFAULT_RECIPIENT_SECTION)
            .await
            .unwrap();
        let second = tool
            .load_recipients_from_sandbox(second_sandbox.as_ref(), DEFAULT_RECIPIENT_SECTION)
            .await
            .unwrap();

        assert_eq!(first[0].user_id, "workspace-a-user");
        assert_eq!(second[0].user_id, "workspace-b-user");
    }

    #[tokio::test]
    async fn sandbox_config_errors_do_not_expose_recipient_ids() {
        let dir = tempfile::tempdir().unwrap();
        let sensitive_id = "should-not-appear-in-error";
        write_workspace_config(
            dir.path(),
            &format!("[finance-fund-daily-dingtalk]\nto = [\"甲|{sensitive_id}\"\n"),
        );
        let sandbox = sandbox_for(dir.path()).await;
        let tool = tool(dir.path());

        let error = tool
            .load_recipients_from_sandbox(sandbox.as_ref(), DEFAULT_RECIPIENT_SECTION)
            .await
            .unwrap_err();
        assert!(error.contains("invalid TOML syntax"));
        assert!(!error.contains(sensitive_id));
    }

    #[test]
    fn rejects_unapproved_config_section() {
        let dir = tempfile::tempdir().unwrap();
        let tool = tool(dir.path());
        assert!(!tool.allowed_section("other-recipients"));
    }

    #[test]
    fn definition_does_not_accept_recipient_ids() {
        let dir = tempfile::tempdir().unwrap();
        let tool = tool(dir.path());
        let schema = tool.parameters();
        assert!(schema["properties"].get("recipient_config").is_some());
        assert!(schema["properties"].get("user_ids").is_none());
        assert!(schema["properties"].get("userIds").is_none());
        assert!(schema["properties"].get("to").is_none());
        assert_eq!(schema["additionalProperties"], false);
        assert_eq!(tool.risk_level(), RiskLevel::Write);
    }

    #[test]
    fn rejects_dynamic_recipient_overrides() {
        let dir = tempfile::tempdir().unwrap();
        let tool = tool(dir.path());
        let mut params = HashMap::from([
            ("recipient_config".into(), json!(DEFAULT_RECIPIENT_SECTION)),
            ("title".into(), json!("日报")),
            ("text".into(), json!("正文")),
        ]);

        for forbidden in ["to", "userIds", "user_ids", "recipients"] {
            params.insert(forbidden.into(), json!(["u-outside-config"]));
            let error = tool.parse_common(&params).unwrap_err();
            assert!(error.contains("is not accepted"));
            params.remove(forbidden);
        }
    }

    #[test]
    fn text_file_is_confined_to_work_and_supports_legacy_prefix() {
        assert_eq!(
            SendDingTalkMessageTool::normalize_text_file_path("reports/daily.md").unwrap(),
            PathBuf::from("reports/daily.md")
        );
        assert_eq!(
            SendDingTalkMessageTool::normalize_text_file_path("work/reports/daily.md").unwrap(),
            PathBuf::from("reports/daily.md")
        );
        for invalid in [
            "",
            "work",
            "/tmp/report.md",
            "../report.md",
            "work/../config",
        ] {
            assert!(SendDingTalkMessageTool::normalize_text_file_path(invalid).is_err());
        }
    }

    #[tokio::test]
    async fn host_execution_fails_closed_without_reading_or_sending() {
        let directory = tempfile::tempdir().unwrap();
        write_workspace_config(
            directory.path(),
            "[finance-fund-daily-dingtalk]\nto = [\"甲|must-not-be-used\"]\n",
        );
        let params = HashMap::from([
            ("recipient_config".into(), json!(DEFAULT_RECIPIENT_SECTION)),
            ("title".into(), json!("测试")),
            ("text".into(), json!("正文")),
        ]);

        let result = tool(directory.path()).execute(params).await;
        assert!(result.contains("requires the current workspace sandbox"));
        assert!(!result.contains("must-not-be-used"));
    }

    #[test]
    fn cloned_token_managers_share_one_cache() {
        let manager = TokenManager::new(Credential::new("app", "secret"));
        let clone = manager.clone();

        assert!(Arc::ptr_eq(&manager.state, &clone.state));
    }

    #[test]
    fn maps_batch_failure_ids_back_to_names() {
        let recipients = vec![
            Recipient {
                name: "张三".into(),
                user_id: "u1".into(),
            },
            Recipient {
                name: "李四".into(),
                user_id: "u2".into(),
            },
        ];
        assert_eq!(
            SendDingTalkMessageTool::names_for_ids(&recipients, &["u2".into()]),
            vec!["李四"]
        );
    }

    #[test]
    fn batch_payload_uses_user_ids_array_and_preserves_https_markdown_link() {
        let dir = tempfile::tempdir().unwrap();
        let tool = tool(dir.path());
        let recipients = vec![
            Recipient {
                name: "张三".into(),
                user_id: "u1".into(),
            },
            Recipient {
                name: "李四".into(),
                user_id: "u2".into(),
            },
        ];
        let text = "[查看 HTML 报告](https://example.com/report.html)";

        let payload = tool.batch_payload(&recipients, "资金日报", text);
        assert_eq!(payload["userIds"], json!(["u1", "u2"]));
        assert_eq!(payload["msgKey"], "sampleMarkdown");
        let msg_param: Value = serde_json::from_str(payload["msgParam"].as_str().unwrap()).unwrap();
        assert_eq!(msg_param["text"], text);
    }

    #[tokio::test]
    async fn batch_send_retries_once_after_unauthorized() {
        let (base_url, server) = spawn_server(vec![
            (200, r#"{"accessToken":"token-1","expireIn":7200}"#),
            (
                401,
                r#"{"code":"InvalidAuthentication","requestId":"request-1"}"#,
            ),
            (200, r#"{"accessToken":"token-2","expireIn":7200}"#),
            (200, r#"{}"#),
        ]);
        let tool = test_tool(&format!("{base_url}/token"), &format!("{base_url}/batch"));
        let recipients = vec![Recipient {
            name: "测试用户".into(),
            user_id: "test-user".into(),
        }];

        assert!(tool.deliver(&recipients, "标题", "正文").await.is_ok());
        server.join().unwrap();
    }

    #[tokio::test]
    async fn api_error_is_sanitized_and_does_not_expose_remote_message() {
        let sensitive_id = "sensitive-user-id-in-remote-message";
        let error_body = format!(
            r#"{{"code":"staffId.notExisted","message":"staff {sensitive_id} not found","requestId":"request-2"}}"#
        );
        let leaked_body: &'static str = Box::leak(error_body.into_boxed_str());
        let (base_url, server) = spawn_server(vec![
            (200, r#"{"accessToken":"token","expireIn":7200}"#),
            (400, leaked_body),
        ]);
        let tool = test_tool(&format!("{base_url}/token"), &format!("{base_url}/batch"));
        let recipients = vec![Recipient {
            name: "测试用户".into(),
            user_id: sensitive_id.into(),
        }];

        let error = tool.deliver(&recipients, "标题", "正文").await.unwrap_err();
        assert!(error.contains("staffId.notExisted"));
        assert!(error.contains("request-2"));
        assert!(!error.contains(sensitive_id));
        server.join().unwrap();
    }

    #[tokio::test]
    async fn http_200_non_json_response_is_sanitized_protocol_error() {
        let (base_url, server) = spawn_server(vec![
            (200, r#"{"accessToken":"token","expireIn":7200}"#),
            (200, "sensitive raw response"),
        ]);
        let tool = test_tool(&format!("{base_url}/token"), &format!("{base_url}/batch"));
        let recipients = vec![Recipient {
            name: "测试用户".into(),
            user_id: "test-user".into(),
        }];

        let error = tool.deliver(&recipients, "标题", "正文").await.unwrap_err();
        assert!(error.contains("invalid JSON"));
        assert!(!error.contains("sensitive raw response"));
        server.join().unwrap();
    }

    #[tokio::test]
    async fn all_partial_result_types_are_mapped_to_names_without_ids() {
        let (base_url, server) = spawn_server(vec![
            (200, r#"{"accessToken":"token","expireIn":7200}"#),
            (
                200,
                r#"{"invalidStaffIdList":["id-invalid"],"flowControlledStaffIdList":["id-flow"],"filteredStaffIdList":["id-filtered"],"processQueryKey":"private-key"}"#,
            ),
        ]);
        let tool = test_tool(&format!("{base_url}/token"), &format!("{base_url}/batch"));
        let recipients = vec![
            Recipient {
                name: "无效用户".into(),
                user_id: "id-invalid".into(),
            },
            Recipient {
                name: "限流用户".into(),
                user_id: "id-flow".into(),
            },
            Recipient {
                name: "过滤用户".into(),
                user_id: "id-filtered".into(),
            },
        ];

        let result = tool
            .send(DEFAULT_RECIPIENT_SECTION, recipients, "标题", "正文".into())
            .await;
        assert!(result.contains(r#""status":"partial""#));
        assert!(result.contains("无效用户"));
        assert!(result.contains("限流用户"));
        assert!(result.contains("过滤用户"));
        for sensitive in ["id-invalid", "id-flow", "id-filtered", "private-key"] {
            assert!(!result.contains(sensitive));
        }
        server.join().unwrap();
    }
}
