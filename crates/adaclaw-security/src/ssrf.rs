//! SSRF (Server-Side Request Forgery) 防护模块
//!
//! 对 `http_request` 工具执行的 URL 做 DNS 解析级别的 IP 过滤，阻断
//! LLM 被 prompt injection 诱导访问内网地址的攻击向量。
//!
//! ## 阻断范围
//!
//! | 地址类型        | 示例                           |
//! |----------------|-------------------------------|
//! | Loopback        | 127.0.0.0/8, ::1              |
//! | 私有地址        | 10/8, 172.16-31/12, 192.168/16|
//! | Link-local      | 169.254.0.0/16                |
//! | CGNAT           | 100.64.0.0/10 (100.64–100.127)|
//! | 元数据端点      | 169.254.169.254               |
//! | 未指定地址      | 0.0.0.0, ::                   |
//!
//! ## 使用
//!
//! ```rust,no_run
//! # async fn example() -> anyhow::Result<()> {
//! adaclaw_security::ssrf::check_ssrf_url("https://example.com").await?;
//! adaclaw_security::ssrf::check_ssrf_url("http://192.168.1.1").await
//!     .expect_err("should block private IP");
//! # Ok(())
//! # }
//! ```

use anyhow::{Result, anyhow};
use std::net::{IpAddr, Ipv4Addr};

// ── IP 分类 ───────────────────────────────────────────────────────────────────

/// 返回 `true` 表示该 IP 应被 SSRF 过滤器阻断。
///
/// 覆盖：loopback / 私有 / link-local / CGNAT / 未指定地址。
pub fn is_blocked_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            v4.is_loopback()           // 127.0.0.0/8
                || v4.is_private()     // 10/8, 172.16-31/12, 192.168/16
                || v4.is_link_local()  // 169.254.0.0/16 (incl. metadata endpoint)
                || is_cgnat_v4(v4)     // 100.64.0.0/10
                || v4.is_unspecified() // 0.0.0.0
                || v4.is_broadcast() // 255.255.255.255
        }
        IpAddr::V6(v6) => {
            v6.is_loopback()       // ::1
                || v6.is_unspecified() // ::
                // IPv6 link-local: fe80::/10
                || {
                    let segs = v6.segments();
                    (segs[0] & 0xffc0) == 0xfe80
                }
        }
    }
}

/// Returns `true` for CGNAT space: 100.64.0.0/10 (100.64.x.x – 100.127.x.x).
fn is_cgnat_v4(ip: Ipv4Addr) -> bool {
    let octets = ip.octets();
    octets[0] == 100 && (64..=127).contains(&octets[1])
}

// ── URL 解析工具 ──────────────────────────────────────────────────────────────

/// Parse the host and default port from an `http://` or `https://` URL.
///
/// Returns `(host, port)` where `port` defaults to 80 or 443 based on scheme.
fn extract_host_port(url: &str) -> Option<(String, u16)> {
    let (default_port, rest) = if let Some(r) = url.strip_prefix("https://") {
        (443u16, r)
    } else if let Some(r) = url.strip_prefix("http://") {
        (80u16, r)
    } else {
        return None;
    };

    // The authority part is everything before the first `/`, `?`, or `#`.
    let authority = rest.split(['/', '?', '#']).next()?;

    // Handle IPv6 literal addresses: [::1]:8080
    if authority.starts_with('[') {
        if let Some(bracket_end) = authority.find(']') {
            let host = authority[1..bracket_end].to_string();
            let port = if let Some(port_str) = authority.get(bracket_end + 2..) {
                port_str.parse().unwrap_or(default_port)
            } else {
                default_port
            };
            return Some((host, port));
        }
        return None;
    }

    // Regular host: "example.com:8080" or "example.com"
    if let Some(colon_pos) = authority.rfind(':') {
        let host = authority[..colon_pos].to_string();
        let port = authority[colon_pos + 1..].parse().unwrap_or(default_port);
        Some((host, port))
    } else {
        Some((authority.to_string(), default_port))
    }
}

// ── 主检查函数 ────────────────────────────────────────────────────────────────

/// Verify that `url` does not resolve to a private/internal address.
///
/// Performs:
/// 1. Parse the URL to extract hostname and port.
/// 2. If the hostname is a raw IP literal, check it directly.
/// 3. Otherwise, resolve via `tokio::net::lookup_host` and check all results.
///
/// # Errors
///
/// Returns `Err` with a human-readable message when:
/// - The URL scheme is not `http://` or `https://`
/// - The hostname resolves to any blocked IP address
/// - The resolved address list is empty (treated as safe — DNS failure could be transient)
pub async fn check_ssrf_url(url: &str) -> Result<()> {
    let (host, port) = extract_host_port(url)
        .ok_or_else(|| anyhow!("SSRF check: cannot parse URL scheme/host from '{}'", url))?;

    // ── Fast path: raw IP literal ─────────────────────────────────────────
    if let Ok(ip) = host.parse::<IpAddr>() {
        if is_blocked_ip(ip) {
            return Err(anyhow!(
                "SSRF blocked: '{}' is a private/internal IP address",
                ip
            ));
        }
        return Ok(());
    }

    // ── DNS resolution path ───────────────────────────────────────────────
    let addr_str = format!("{}:{}", host, port);
    match tokio::net::lookup_host(&addr_str).await {
        Ok(addrs) => {
            for sock_addr in addrs {
                let ip = sock_addr.ip();
                if is_blocked_ip(ip) {
                    return Err(anyhow!(
                        "SSRF blocked: '{}' resolves to private/internal IP {}",
                        host,
                        ip
                    ));
                }
            }
            Ok(())
        }
        Err(e) => {
            // DNS resolution failure: log and allow (transient failure should
            // not permanently block requests to legitimate external services).
            tracing::warn!(
                host = %host,
                error = %e,
                "SSRF check: DNS resolution failed; allowing request (may be transient)"
            );
            Ok(())
        }
    }
}

// ── 单元测试 ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── is_blocked_ip ─────────────────────────────────────────────────────────

    #[test]
    fn test_loopback_v4_blocked() {
        assert!(is_blocked_ip("127.0.0.1".parse().unwrap()));
        assert!(is_blocked_ip("127.255.255.255".parse().unwrap()));
    }

    #[test]
    fn test_loopback_v6_blocked() {
        assert!(is_blocked_ip("::1".parse().unwrap()));
    }

    #[test]
    fn test_private_ipv4_blocked() {
        assert!(is_blocked_ip("10.0.0.1".parse().unwrap()));
        assert!(is_blocked_ip("172.16.0.1".parse().unwrap()));
        assert!(is_blocked_ip("172.31.255.255".parse().unwrap()));
        assert!(is_blocked_ip("192.168.1.1".parse().unwrap()));
    }

    #[test]
    fn test_link_local_blocked() {
        // 169.254.169.254 is the AWS metadata endpoint — must be blocked
        assert!(is_blocked_ip("169.254.169.254".parse().unwrap()));
        assert!(is_blocked_ip("169.254.0.1".parse().unwrap()));
    }

    #[test]
    fn test_cgnat_blocked() {
        assert!(is_blocked_ip("100.64.0.1".parse().unwrap()));
        assert!(is_blocked_ip("100.127.255.255".parse().unwrap()));
    }

    #[test]
    fn test_cgnat_boundary_allowed() {
        // 100.63.x.x is NOT CGNAT (just below the range)
        assert!(!is_blocked_ip("100.63.255.255".parse().unwrap()));
        // 100.128.x.x is NOT CGNAT (just above the range)
        assert!(!is_blocked_ip("100.128.0.1".parse().unwrap()));
    }

    #[test]
    fn test_unspecified_blocked() {
        assert!(is_blocked_ip("0.0.0.0".parse().unwrap()));
        assert!(is_blocked_ip("::".parse().unwrap()));
    }

    #[test]
    fn test_public_ipv4_allowed() {
        assert!(!is_blocked_ip("8.8.8.8".parse().unwrap()));
        assert!(!is_blocked_ip("1.1.1.1".parse().unwrap()));
        assert!(!is_blocked_ip("93.184.216.34".parse().unwrap())); // example.com
    }

    #[test]
    fn test_public_ipv6_allowed() {
        // 2606:2800:21f:cb07:6820:80da:af6b:8b2c (example.com)
        assert!(!is_blocked_ip(
            "2606:2800:21f:cb07:6820:80da:af6b:8b2c".parse().unwrap()
        ));
    }

    #[test]
    fn test_ipv6_link_local_blocked() {
        assert!(is_blocked_ip("fe80::1".parse().unwrap()));
        assert!(is_blocked_ip("fe80::dead:beef".parse().unwrap()));
    }

    // ── extract_host_port ─────────────────────────────────────────────────────

    #[test]
    fn test_extract_host_port_https_default() {
        let (host, port) = extract_host_port("https://example.com/path").unwrap();
        assert_eq!(host, "example.com");
        assert_eq!(port, 443);
    }

    #[test]
    fn test_extract_host_port_http_default() {
        let (host, port) = extract_host_port("http://api.example.com/v1").unwrap();
        assert_eq!(host, "api.example.com");
        assert_eq!(port, 80);
    }

    #[test]
    fn test_extract_host_port_explicit_port() {
        let (host, port) = extract_host_port("http://localhost:8080/api").unwrap();
        assert_eq!(host, "localhost");
        assert_eq!(port, 8080);
    }

    #[test]
    fn test_extract_host_port_ipv6_literal() {
        let (host, port) = extract_host_port("http://[::1]:9000/").unwrap();
        assert_eq!(host, "::1");
        assert_eq!(port, 9000);
    }

    #[test]
    fn test_extract_host_port_invalid_scheme() {
        assert!(extract_host_port("ftp://example.com").is_none());
        assert!(extract_host_port("not-a-url").is_none());
    }

    // ── check_ssrf_url (async) ────────────────────────────────────────────────

    #[tokio::test]
    async fn test_check_ssrf_blocks_private_ip_literal() {
        let err = check_ssrf_url("http://192.168.1.1/admin").await;
        assert!(err.is_err(), "private IP must be blocked");
        assert!(err.unwrap_err().to_string().contains("SSRF blocked"));
    }

    #[tokio::test]
    async fn test_check_ssrf_blocks_loopback() {
        assert!(check_ssrf_url("http://127.0.0.1:8080").await.is_err());
    }

    #[tokio::test]
    async fn test_check_ssrf_blocks_metadata_endpoint() {
        // AWS metadata / GCP metadata endpoint
        assert!(
            check_ssrf_url("http://169.254.169.254/latest/meta-data/")
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn test_check_ssrf_blocks_ipv6_loopback() {
        assert!(check_ssrf_url("http://[::1]:80/").await.is_err());
    }

    #[tokio::test]
    async fn test_check_ssrf_invalid_scheme() {
        assert!(check_ssrf_url("ftp://files.example.com").await.is_err());
    }
}
