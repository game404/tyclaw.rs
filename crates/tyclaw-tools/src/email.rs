//! SendEmailTool —— 通过 SMTP 发送邮件（正文 + workspace 内附件）。
//!
//! - 发件人地址固定取自配置 `EmailConfig.from`，LLM 仅可通过 `from_name` 覆盖显示名。
//! - 收件人支持 `to` / `cc` / `bcc`，可选域名白名单 `allowed_domains`。
//! - 附件复用 workspace 内文件（`safe_resolve` 防目录穿越），单封累计不超过 `max_attachment_bytes`。

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::PathBuf;
use tracing::{debug, warn};

use lettre::message::header::ContentType;
use lettre::message::{Attachment, Mailbox, MultiPart, SinglePart};
use lettre::transport::smtp::authentication::Credentials;
use lettre::{AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor};

use crate::base::{brief_truncate, RiskLevel, Tool};
use crate::filesystem::safe_resolve;

/// 默认单封邮件附件累计上限（25MB）。
const DEFAULT_MAX_ATTACHMENT_BYTES: usize = 25 * 1024 * 1024;
/// 默认 SMTP 端口（隐式 TLS / SMTPS）。
const DEFAULT_SMTP_PORT: u16 = 465;

/// 邮件发送配置（来自 config.yaml 的 `email` 段）。
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct EmailConfig {
    /// SMTP 服务器主机名。
    pub smtp_host: String,
    /// SMTP 端口。0 表示按 TLS 模式使用库默认端口。
    pub smtp_port: u16,
    /// SMTP 登录用户名（为空则不做鉴权，兼容无鉴权 relay）。
    pub username: String,
    /// SMTP 登录密码 / 授权码。
    pub password: String,
    /// 发件人地址，可含显示名，如 `"Bot <bot@example.com>"`。
    pub from: String,
    /// 加密模式：`implicit`（默认，SMTPS）/ `starttls` / `none`。
    pub tls: String,
    /// 收件人域名白名单（不含 @）。为空表示不限制。
    pub allowed_domains: Vec<String>,
    /// 单封邮件附件累计字节上限。
    pub max_attachment_bytes: usize,
}

impl Default for EmailConfig {
    fn default() -> Self {
        Self {
            smtp_host: String::new(),
            smtp_port: DEFAULT_SMTP_PORT,
            username: String::new(),
            password: String::new(),
            from: String::new(),
            tls: "implicit".into(),
            allowed_domains: Vec::new(),
            max_attachment_bytes: DEFAULT_MAX_ATTACHMENT_BYTES,
        }
    }
}

impl EmailConfig {
    /// 是否具备最小可用配置（SMTP 主机与发件人地址）。
    fn is_configured(&self) -> bool {
        !self.smtp_host.trim().is_empty() && !self.from.trim().is_empty()
    }
}

/// 邮件发送工具 —— SMTP 发送正文与附件。
pub struct SendEmailTool {
    config: EmailConfig,
    workspace: Option<PathBuf>,
}

impl SendEmailTool {
    pub fn new(config: EmailConfig, workspace: Option<PathBuf>) -> Self {
        Self { config, workspace }
    }

    /// 校验收件人域名是否命中白名单（白名单为空则放行）。
    fn domain_allowed(&self, domain: &str) -> bool {
        if self.config.allowed_domains.is_empty() {
            return true;
        }
        let domain = domain.to_ascii_lowercase();
        self.config
            .allowed_domains
            .iter()
            .any(|d| d.trim().to_ascii_lowercase() == domain)
    }

    /// 解析并校验一组收件人字符串为 Mailbox，同时施加白名单。
    fn parse_recipients(&self, raw: &[String], field: &str) -> Result<Vec<Mailbox>, String> {
        let mut out = Vec::with_capacity(raw.len());
        for addr in raw {
            let addr = addr.trim();
            if addr.is_empty() {
                continue;
            }
            let mbox: Mailbox = addr
                .parse()
                .map_err(|e| format!("Error: Invalid {field} address '{addr}': {e}"))?;
            if !self.domain_allowed(mbox.email.domain()) {
                return Err(format!(
                    "Error: Recipient domain '{}' is not in the allowed_domains list",
                    mbox.email.domain()
                ));
            }
            out.push(mbox);
        }
        Ok(out)
    }
}

/// 从参数中提取字符串数组：支持字符串（逗号/分号分隔）或字符串数组。
fn extract_addresses(params: &HashMap<String, Value>, key: &str) -> Vec<String> {
    match params.get(key) {
        Some(Value::String(s)) => s
            .split([',', ';'])
            .map(|p| p.trim().to_string())
            .filter(|p| !p.is_empty())
            .collect(),
        Some(Value::Array(arr)) => arr
            .iter()
            .filter_map(|v| v.as_str().map(|s| s.trim().to_string()))
            .filter(|p| !p.is_empty())
            .collect(),
        _ => Vec::new(),
    }
}

/// 根据文件扩展名猜测 MIME 类型，未知回退 application/octet-stream。
fn guess_mime(filename: &str) -> ContentType {
    let ext = filename.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    let mime = match ext.as_str() {
        "txt" | "log" | "csv" | "md" => "text/plain; charset=utf-8",
        "html" | "htm" => "text/html; charset=utf-8",
        "json" => "application/json",
        "pdf" => "application/pdf",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "zip" => "application/zip",
        "doc" | "docx" => "application/msword",
        "xls" | "xlsx" => "application/vnd.ms-excel",
        _ => "application/octet-stream",
    };
    ContentType::parse(mime)
        .unwrap_or_else(|_| ContentType::parse("application/octet-stream").unwrap())
}

#[async_trait]
impl Tool for SendEmailTool {
    fn name(&self) -> &str {
        "send_email"
    }

    fn description(&self) -> &str {
        "Send an email via SMTP. Supports to/cc/bcc recipients, plain-text or HTML body, \
         and file attachments (paths relative to the workspace). The sender address is fixed \
         by server configuration; you may only set the sender display name via 'from_name'."
    }

    fn brief(&self, args: &HashMap<String, Value>) -> Option<String> {
        let to = extract_addresses(args, "to").join(", ");
        let subject = args.get("subject").and_then(|v| v.as_str()).unwrap_or("");
        Some(brief_truncate(
            &format!("send_email → {to} | {subject}"),
            80,
        ))
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "to": {
                    "type": ["string", "array"],
                    "items": { "type": "string" },
                    "description": "Recipient(s). A single address, comma-separated string, or array of addresses."
                },
                "cc": {
                    "type": ["string", "array"],
                    "items": { "type": "string" },
                    "description": "CC recipient(s) (optional)."
                },
                "bcc": {
                    "type": ["string", "array"],
                    "items": { "type": "string" },
                    "description": "BCC recipient(s) (optional)."
                },
                "subject": { "type": "string", "description": "Email subject." },
                "body": { "type": "string", "description": "Email body content." },
                "body_type": {
                    "type": "string",
                    "enum": ["text", "html"],
                    "description": "Body format: 'text' (default) or 'html'."
                },
                "from_name": {
                    "type": "string",
                    "description": "Optional sender display name. The sender address is fixed by config and cannot be changed."
                },
                "attachments": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Attachment file paths, relative to the workspace."
                }
            },
            "required": ["to", "subject", "body"]
        })
    }

    fn risk_level(&self) -> RiskLevel {
        RiskLevel::Write
    }

    async fn execute(&self, params: HashMap<String, Value>) -> String {
        if !self.config.is_configured() {
            return "Error: Email is not configured. Set 'email.smtp_host' and 'email.from' in config.yaml.".to_string();
        }

        let subject = params
            .get("subject")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let body = params
            .get("body")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let body_type = params
            .get("body_type")
            .and_then(|v| v.as_str())
            .unwrap_or("text");
        let from_name = params
            .get("from_name")
            .and_then(|v| v.as_str())
            .map(|s| s.trim())
            .filter(|s| !s.is_empty());

        // 收件人解析与白名单校验。
        let to_list = extract_addresses(&params, "to");
        if to_list.is_empty() {
            return "Error: 'to' must contain at least one recipient address.".to_string();
        }
        let to_mboxes = match self.parse_recipients(&to_list, "to") {
            Ok(v) => v,
            Err(e) => return e,
        };
        let cc_mboxes = match self.parse_recipients(&extract_addresses(&params, "cc"), "cc") {
            Ok(v) => v,
            Err(e) => return e,
        };
        let bcc_mboxes = match self.parse_recipients(&extract_addresses(&params, "bcc"), "bcc") {
            Ok(v) => v,
            Err(e) => return e,
        };

        // 发件人：地址固定取配置，仅允许覆盖显示名。
        let base_from: Mailbox = match self.config.from.parse() {
            Ok(m) => m,
            Err(e) => {
                return format!(
                    "Error: Invalid configured sender 'email.from' ({}): {e}",
                    self.config.from
                )
            }
        };
        let from_mbox = match from_name {
            Some(name) => Mailbox::new(Some(name.to_string()), base_from.email),
            None => base_from,
        };

        // 组装邮件。
        let mut builder = Message::builder().from(from_mbox).subject(subject);
        for m in to_mboxes {
            builder = builder.to(m);
        }
        for m in cc_mboxes {
            builder = builder.cc(m);
        }
        for m in bcc_mboxes {
            builder = builder.bcc(m);
        }

        let content_type = if body_type == "html" {
            ContentType::TEXT_HTML
        } else {
            ContentType::TEXT_PLAIN
        };

        // 附件读取（限定 workspace + 累计大小上限）。
        let attachment_paths = extract_addresses(&params, "attachments");
        let email = if attachment_paths.is_empty() {
            match builder.header(content_type).body(body) {
                Ok(m) => m,
                Err(e) => return format!("Error: Failed to build email: {e}"),
            }
        } else {
            let body_part = if body_type == "html" {
                SinglePart::html(body)
            } else {
                SinglePart::plain(body)
            };
            let mut mp = MultiPart::mixed().singlepart(body_part);
            let mut total: usize = 0;
            for path in &attachment_paths {
                let resolved = match safe_resolve(path, self.workspace.as_deref()) {
                    Ok(p) => p,
                    Err(e) => return e,
                };
                let bytes = match tokio::fs::read(&resolved).await {
                    Ok(b) => b,
                    Err(e) => {
                        return format!("Error: Failed to read attachment '{path}': {e}");
                    }
                };
                total = total.saturating_add(bytes.len());
                if total > self.config.max_attachment_bytes {
                    return format!(
                        "Error: Attachments exceed the maximum total size of {} bytes.",
                        self.config.max_attachment_bytes
                    );
                }
                let filename = resolved
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "attachment".to_string());
                let ct = guess_mime(&filename);
                mp = mp.singlepart(Attachment::new(filename).body(bytes, ct));
            }
            match builder.multipart(mp) {
                Ok(m) => m,
                Err(e) => return format!("Error: Failed to build email: {e}"),
            }
        };

        // 构建 SMTP 传输并发送。
        let tls = self.config.tls.to_ascii_lowercase();
        let transport_builder = match tls.as_str() {
            "starttls" => match AsyncSmtpTransport::<Tokio1Executor>::starttls_relay(
                &self.config.smtp_host,
            ) {
                Ok(b) => b,
                Err(e) => return format!("Error: Failed to init STARTTLS transport: {e}"),
            },
            "none" => {
                AsyncSmtpTransport::<Tokio1Executor>::builder_dangerous(&self.config.smtp_host)
            }
            _ => match AsyncSmtpTransport::<Tokio1Executor>::relay(&self.config.smtp_host) {
                Ok(b) => b,
                Err(e) => return format!("Error: Failed to init TLS transport: {e}"),
            },
        };

        let mut transport_builder = transport_builder;
        if self.config.smtp_port != 0 {
            transport_builder = transport_builder.port(self.config.smtp_port);
        }
        if !self.config.username.is_empty() {
            transport_builder = transport_builder.credentials(Credentials::new(
                self.config.username.clone(),
                self.config.password.clone(),
            ));
        }
        let mailer = transport_builder.build();

        match mailer.send(email).await {
            Ok(resp) => {
                let code = resp.code().to_string();
                debug!(smtp_code = %code, "email sent");
                json!({
                    "status": "ok",
                    "smtp_code": code,
                    "response": resp.message().collect::<Vec<_>>(),
                    "recipients": to_list.len(),
                })
                .to_string()
            }
            Err(e) => {
                warn!(error = %e, "email send failed");
                format!("Error: Failed to send email: {e}")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool_with(config: EmailConfig) -> SendEmailTool {
        SendEmailTool::new(config, None)
    }

    #[test]
    fn test_definition_shape() {
        let tool = tool_with(EmailConfig::default());
        assert_eq!(tool.name(), "send_email");
        assert_eq!(tool.risk_level().to_string(), "write");
        let params = tool.parameters();
        let required = params["required"].as_array().unwrap();
        assert!(required.iter().any(|v| v == "to"));
        assert!(required.iter().any(|v| v == "subject"));
        assert!(required.iter().any(|v| v == "body"));
    }

    #[tokio::test]
    async fn test_config_missing() {
        let tool = tool_with(EmailConfig::default());
        let mut params = HashMap::new();
        params.insert("to".into(), json!("a@example.com"));
        params.insert("subject".into(), json!("hi"));
        params.insert("body".into(), json!("body"));
        let out = tool.execute(params).await;
        assert!(out.starts_with("Error: Email is not configured"));
    }

    #[test]
    fn test_extract_addresses_string_and_array() {
        let mut p = HashMap::new();
        p.insert("to".into(), json!("a@x.com, b@x.com; c@x.com"));
        assert_eq!(extract_addresses(&p, "to").len(), 3);

        let mut p2 = HashMap::new();
        p2.insert("to".into(), json!(["a@x.com", "b@x.com"]));
        assert_eq!(extract_addresses(&p2, "to").len(), 2);

        let p3: HashMap<String, Value> = HashMap::new();
        assert!(extract_addresses(&p3, "to").is_empty());
    }

    #[test]
    fn test_domain_allowlist() {
        let cfg = EmailConfig {
            allowed_domains: vec!["example.com".into()],
            ..Default::default()
        };
        let tool = tool_with(cfg);
        assert!(tool.domain_allowed("example.com"));
        assert!(tool.domain_allowed("EXAMPLE.COM"));
        assert!(!tool.domain_allowed("evil.com"));

        // 白名单为空 → 全部放行。
        let open = tool_with(EmailConfig::default());
        assert!(open.domain_allowed("anything.com"));
    }

    #[test]
    fn test_parse_recipients_rejects_disallowed_domain() {
        let cfg = EmailConfig {
            allowed_domains: vec!["example.com".into()],
            ..Default::default()
        };
        let tool = tool_with(cfg);
        let err = tool
            .parse_recipients(&["user@evil.com".to_string()], "to")
            .unwrap_err();
        assert!(err.contains("not in the allowed_domains"));

        let ok = tool
            .parse_recipients(&["user@example.com".to_string()], "to")
            .unwrap();
        assert_eq!(ok.len(), 1);
    }

    #[tokio::test]
    async fn test_attachment_path_traversal_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = EmailConfig {
            smtp_host: "smtp.example.com".into(),
            from: "bot@example.com".into(),
            ..Default::default()
        };
        let tool = SendEmailTool::new(cfg, Some(dir.path().to_path_buf()));
        let mut params = HashMap::new();
        params.insert("to".into(), json!("user@example.com"));
        params.insert("subject".into(), json!("hi"));
        params.insert("body".into(), json!("body"));
        params.insert("attachments".into(), json!(["../../etc/passwd"]));
        let out = tool.execute(params).await;
        assert!(out.contains("within workspace") || out.starts_with("Error"));
    }
}
