use reqwest::StatusCode;
use serde_json::json;
use std::time::Duration;

const BATCH_SEND_URL: &str = "https://api.dingtalk.com/v1.0/robot/oToMessages/batchSend";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdminSendErrorKind {
    Unauthorized,
    RemoteStatus,
    Transport,
    InvalidInput,
}

#[derive(Debug)]
pub struct AdminSendError {
    kind: AdminSendErrorKind,
}

impl AdminSendError {
    pub fn kind(&self) -> AdminSendErrorKind {
        self.kind
    }
}

#[derive(Clone)]
pub struct AdminMarkdownSender {
    client: reqwest::Client,
    endpoint: String,
}

impl Default for AdminMarkdownSender {
    fn default() -> Self {
        Self::new()
    }
}

impl AdminMarkdownSender {
    pub fn new() -> Self {
        Self {
            client: reqwest::Client::new(),
            endpoint: BATCH_SEND_URL.into(),
        }
    }

    #[cfg(test)]
    fn with_endpoint(endpoint: String) -> Self {
        Self {
            client: reqwest::Client::new(),
            endpoint,
        }
    }

    pub async fn send(
        &self,
        token: &str,
        robot_code: &str,
        admin_user_ids: &[String],
        title: &str,
        text: &str,
        timeout: Duration,
    ) -> Result<(), AdminSendError> {
        if token.is_empty()
            || robot_code.is_empty()
            || admin_user_ids.is_empty()
            || admin_user_ids.iter().any(|id| id.trim().is_empty())
        {
            return Err(AdminSendError {
                kind: AdminSendErrorKind::InvalidInput,
            });
        }
        let msg_param = json!({ "title": title, "text": text }).to_string();
        let payload = json!({
            "robotCode": robot_code,
            "userIds": admin_user_ids,
            "msgKey": "sampleMarkdown",
            "msgParam": msg_param,
        });
        let response = self
            .client
            .post(&self.endpoint)
            .header("x-acs-dingtalk-access-token", token)
            .header("Content-Type", "application/json")
            .json(&payload)
            .timeout(timeout)
            .send()
            .await
            .map_err(|_| AdminSendError {
                kind: AdminSendErrorKind::Transport,
            })?;
        let status = response.status();
        if status.is_success() {
            return Ok(());
        }
        drop(response);
        let kind = if status == StatusCode::UNAUTHORIZED {
            AdminSendErrorKind::Unauthorized
        } else {
            AdminSendErrorKind::RemoteStatus
        };
        tracing::warn!(status = status.as_u16(), error_kind = ?kind, "DingTalk admin alert send failed");
        Err(AdminSendError { kind })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    async fn mock_server(
        status: &str,
        body: &'static str,
    ) -> (String, tokio::task::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let status = status.to_string();
        let handle = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut bytes = Vec::new();
            let mut buffer = [0u8; 4096];
            loop {
                let read = stream.read(&mut buffer).await.unwrap();
                if read == 0 {
                    break;
                }
                bytes.extend_from_slice(&buffer[..read]);
                let request = String::from_utf8_lossy(&bytes);
                if let Some(header_end) = request.find("\r\n\r\n") {
                    let content_length = request[..header_end]
                        .lines()
                        .find_map(|line| {
                            line.to_ascii_lowercase()
                                .strip_prefix("content-length:")
                                .map(|v| v.trim().parse::<usize>().unwrap())
                        })
                        .unwrap_or(0);
                    if bytes.len() >= header_end + 4 + content_length {
                        break;
                    }
                }
            }
            let response = format!(
                "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(response.as_bytes()).await.unwrap();
            String::from_utf8(bytes).unwrap()
        });
        (format!("http://{address}/batchSend"), handle)
    }

    #[tokio::test]
    async fn sends_all_admins_in_one_markdown_batch() {
        let (endpoint, request) = mock_server("200 OK", "{}").await;
        let sender = AdminMarkdownSender::with_endpoint(endpoint);
        sender
            .send(
                "fake-token",
                "robot",
                &["admin-a".into(), "admin-b".into()],
                "title",
                "text",
                Duration::from_secs(1),
            )
            .await
            .unwrap();
        let request = request.await.unwrap();
        assert_eq!(request.matches("POST /batchSend").count(), 1);
        assert!(request.contains("admin-a"));
        assert!(request.contains("admin-b"));
        assert!(request.contains("sampleMarkdown"));
    }

    #[tokio::test]
    async fn remote_error_never_returns_response_body_or_credentials() {
        let secret = "secret-admin-id-and-token";
        let (endpoint, request) = mock_server("500 Internal Server Error", secret).await;
        let sender = AdminMarkdownSender::with_endpoint(endpoint);
        let error = sender
            .send(
                secret,
                "robot",
                &[secret.into()],
                "title",
                "text",
                Duration::from_secs(1),
            )
            .await
            .unwrap_err();
        request.await.unwrap();
        assert_eq!(error.kind(), AdminSendErrorKind::RemoteStatus);
        assert!(!format!("{error:?}").contains(secret));
    }

    #[tokio::test]
    async fn unauthorized_is_structured_and_sanitized() {
        let (endpoint, request) = mock_server("401 Unauthorized", "fake-token").await;
        let sender = AdminMarkdownSender::with_endpoint(endpoint);
        let error = sender
            .send(
                "fake-token",
                "robot",
                &["admin".into()],
                "title",
                "text",
                Duration::from_secs(1),
            )
            .await
            .unwrap_err();
        request.await.unwrap();
        assert_eq!(error.kind(), AdminSendErrorKind::Unauthorized);
        assert!(!format!("{error:?}").contains("fake-token"));
    }
}
