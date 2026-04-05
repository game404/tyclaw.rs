//! SSRF 防护 —— URL 校验、DNS 解析后 IP 检查。

use std::net::IpAddr;
use tokio::net::lookup_host;
use url::Url;

/// 检查 IP 地址是否属于内网/保留地址段。
fn is_private(addr: &IpAddr) -> bool {
    match addr {
        IpAddr::V4(v4) => {
            v4.is_loopback()                              // 127.0.0.0/8
                || v4.is_private()                        // 10/8, 172.16/12, 192.168/16
                || v4.is_link_local()                     // 169.254.0.0/16
                || v4.is_unspecified()                    // 0.0.0.0/8
                || v4.octets()[0] == 100                  // 100.64.0.0/10 carrier-grade NAT
                    && (v4.octets()[1] & 0xC0) == 64
        }
        IpAddr::V6(v6) => {
            v6.is_loopback()                              // ::1
                || v6.is_unspecified()                    // ::
                || {
                    let seg0 = v6.segments()[0];
                    (seg0 & 0xfe00) == 0xfc00             // fc00::/7  unique local
                        || (seg0 & 0xffc0) == 0xfe80      // fe80::/10 link-local
                }
        }
    }
}

/// 仅校验 URL 的 scheme 和 domain 格式（不做 DNS 解析）。
pub fn validate_url(raw: &str) -> Result<Url, String> {
    let parsed = Url::parse(raw).map_err(|e| format!("Invalid URL: {e}"))?;
    match parsed.scheme() {
        "http" | "https" => {}
        other => return Err(format!("Only http/https allowed, got '{other}'")),
    }
    if parsed.host_str().is_none() {
        return Err("Missing domain".into());
    }
    Ok(parsed)
}

/// 完整 SSRF 校验：scheme + domain + DNS 解析 + IP 检查。
pub async fn validate_url_safe(raw: &str) -> Result<Url, String> {
    let parsed = validate_url(raw)?;
    let host = parsed.host_str().unwrap();
    let port = parsed.port_or_known_default().unwrap_or(443);
    let lookup = format!("{host}:{port}");

    let addrs = lookup_host(&lookup)
        .await
        .map_err(|_| format!("Cannot resolve hostname: {host}"))?;

    for addr in addrs {
        if is_private(&addr.ip()) {
            return Err(format!(
                "Blocked: {host} resolves to private/internal address {}",
                addr.ip()
            ));
        }
    }
    Ok(parsed)
}

/// 校验重定向后的目标 URL（只检查 IP，不阻止域名解析失败）。
pub async fn validate_resolved_url(raw: &str) -> Result<(), String> {
    let parsed = match Url::parse(raw) {
        Ok(p) => p,
        Err(_) => return Ok(()),
    };
    let host = match parsed.host_str() {
        Some(h) => h,
        None => return Ok(()),
    };

    if let Ok(ip) = host.parse::<IpAddr>() {
        if is_private(&ip) {
            return Err(format!("Redirect target is a private address: {ip}"));
        }
        return Ok(());
    }

    let port = parsed.port_or_known_default().unwrap_or(443);
    let lookup = format!("{host}:{port}");
    if let Ok(addrs) = lookup_host(&lookup).await {
        for addr in addrs {
            if is_private(&addr.ip()) {
                return Err(format!(
                    "Redirect target {host} resolves to private address {}",
                    addr.ip()
                ));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_validate_url_rejects_file_scheme() {
        assert!(validate_url("file:///etc/passwd").is_err());
    }

    #[test]
    fn test_validate_url_rejects_ftp() {
        assert!(validate_url("ftp://example.com/file").is_err());
    }

    #[test]
    fn test_validate_url_accepts_https() {
        assert!(validate_url("https://example.com/path").is_ok());
    }

    #[test]
    fn test_validate_url_accepts_http() {
        assert!(validate_url("http://example.com").is_ok());
    }

    #[test]
    fn test_validate_url_rejects_no_domain() {
        assert!(validate_url("http://").is_err());
    }

    #[test]
    fn test_is_private_loopback_v4() {
        let ip: IpAddr = "127.0.0.1".parse().unwrap();
        assert!(is_private(&ip));
    }

    #[test]
    fn test_is_private_10_network() {
        let ip: IpAddr = "10.0.0.1".parse().unwrap();
        assert!(is_private(&ip));
    }

    #[test]
    fn test_is_private_192_168() {
        let ip: IpAddr = "192.168.1.1".parse().unwrap();
        assert!(is_private(&ip));
    }

    #[test]
    fn test_is_private_link_local() {
        let ip: IpAddr = "169.254.1.1".parse().unwrap();
        assert!(is_private(&ip));
    }

    #[test]
    fn test_is_private_public_ip() {
        let ip: IpAddr = "8.8.8.8".parse().unwrap();
        assert!(!is_private(&ip));
    }

    #[test]
    fn test_is_private_ipv6_loopback() {
        let ip: IpAddr = "::1".parse().unwrap();
        assert!(is_private(&ip));
    }

    #[tokio::test]
    async fn test_validate_url_safe_rejects_localhost() {
        let result = validate_url_safe("http://localhost/secret").await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_validate_url_safe_rejects_127() {
        let result = validate_url_safe("http://127.0.0.1/admin").await;
        assert!(result.is_err());
    }
}
