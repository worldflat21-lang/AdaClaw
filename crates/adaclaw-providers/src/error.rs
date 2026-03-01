//! 结构化 Provider 错误类型。
//!
//! Phase 14-P0-2: 全面使用 `thiserror` 替代手动 `Display` / `Error` impl。
//! 调用方可通过 `err.downcast_ref::<ProviderError>()` 匹配具体错误类型。
//!
//! | 错误类型              | 重试？ | 触发熔断器？ |
//! |---------------------|-------|------------|
//! | `RateLimit` (429)   | 否     | 是          |
//! | `Billing` (402)     | 否     | 是          |
//! | `AuthError` (401/403)| 否   | **否**      |
//! | `BadRequest` (400)  | 否     | **否**      |
//! | `ServerError` (5xx) | 是     | 是          |
//! | `Timeout` (408/net) | 是     | 是          |
//! | `Unknown`           | 是     | 是          |

// ── 错误类型枚举 ──────────────────────────────────────────────────────────────

/// HTTP 状态码转换后的 Provider 错误类型。
///
/// Phase 14-P0-2: `#[derive(thiserror::Error)]` + `#[error(...)]` on each
/// variant replaces the hand-written `impl fmt::Display`.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ProviderErrorKind {
    /// 429 Too Many Requests.
    #[error("RateLimit")]
    RateLimit,

    /// 402 Payment Required / insufficient credits.
    #[error("Billing")]
    Billing,

    /// 401 / 403 auth failure.
    #[error("AuthError")]
    AuthError,

    /// 400 Bad Request / malformed request.
    #[error("BadRequest")]
    BadRequest,

    /// 5xx Server Error.
    #[error("ServerError")]
    ServerError,

    /// 408 / network timeout.
    #[error("Timeout")]
    Timeout,

    /// Unclassified error.
    #[error("Unknown")]
    Unknown,
}

impl ProviderErrorKind {
    /// Whether this error kind should trigger a retry on the same provider.
    pub fn is_retryable(&self) -> bool {
        matches!(self, Self::ServerError | Self::Timeout | Self::Unknown)
    }

    /// Whether this error kind should count towards the circuit-breaker failure counter.
    /// `AuthError` and `BadRequest` do NOT count (not a provider availability issue).
    pub fn counts_as_circuit_failure(&self) -> bool {
        matches!(
            self,
            Self::ServerError | Self::Timeout | Self::Unknown | Self::RateLimit | Self::Billing
        )
    }
}

// ── ProviderError 结构体 ──────────────────────────────────────────────────────

/// Structured Provider error with HTTP status code and optional `Retry-After`.
///
/// Wrap with `anyhow::Error::new(pe)` to pass through the error chain.
/// Recover with `err.downcast_ref::<ProviderError>()` in `ReliabilityChain`.
///
/// Phase 14-P0-2: `#[derive(thiserror::Error)]` generates `Display` + `Error`.
/// The `#[error(...)]` format string references `{kind}` which requires
/// `ProviderErrorKind: Display` — satisfied by its own `#[error(...)]` attrs.
#[derive(Debug, thiserror::Error)]
#[error("HTTP {status} ({kind}): {message}")]
pub struct ProviderError {
    /// Error classification.
    pub kind: ProviderErrorKind,
    /// Raw HTTP status code (e.g. 429, 500).
    pub status: u16,
    /// Response body text for debug logging.
    pub message: String,
    /// Seconds from `Retry-After` header (only for 429).
    pub retry_after_secs: Option<u64>,
}

impl ProviderError {
    /// Construct a `ProviderError` from an HTTP status code.
    pub fn from_status(status: u16, body: &str, retry_after_secs: Option<u64>) -> Self {
        let kind = match status {
            400 => ProviderErrorKind::BadRequest,
            401 | 403 => ProviderErrorKind::AuthError,
            402 => ProviderErrorKind::Billing,
            408 => ProviderErrorKind::Timeout,
            429 => ProviderErrorKind::RateLimit,
            500..=599 => ProviderErrorKind::ServerError,
            _ => ProviderErrorKind::Unknown,
        };
        Self {
            kind,
            status,
            message: body.to_string(),
            retry_after_secs,
        }
    }
}

// ── 辅助函数 ──────────────────────────────────────────────────────────────────

/// Extract a `ProviderErrorKind` from an `anyhow::Error`.
///
/// Prefers downcast to `ProviderError` (structured); falls back to string
/// pattern matching for providers that still use plain `anyhow` errors.
pub fn classify_error(err: &anyhow::Error) -> ProviderErrorKind {
    if let Some(pe) = err.downcast_ref::<ProviderError>() {
        return pe.kind.clone();
    }

    let s = err.to_string().to_lowercase();

    if contains_any(&s, &["429", "rate limit", "rate_limit", "ratelimit", "too many requests",
        "exceeded your current quota", "exceeded quota", "resource has been exhausted",
        "resource_exhausted", "quota exceeded", "usage limit"]) {
        return ProviderErrorKind::RateLimit;
    }
    if contains_any(&s, &["overloaded_error", "overloaded"]) {
        return ProviderErrorKind::RateLimit;
    }
    if contains_any(&s, &["402", "payment required", "insufficient credits", "credit balance",
        "plans & billing", "insufficient balance", "billing"]) {
        return ProviderErrorKind::Billing;
    }
    if contains_any(&s, &["408", "timeout", "timed out", "deadline exceeded",
        "context deadline exceeded", "request timeout", "connection timed out"]) {
        return ProviderErrorKind::Timeout;
    }
    if contains_any(&s, &["401", "403", "invalid api key", "invalid_api_key",
        "incorrect api key", "invalid token", "authentication", "re-authenticate",
        "oauth token refresh failed", "unauthorized", "forbidden", "access denied",
        "expired", "token has expired", "no credentials found", "no api key found"]) {
        return ProviderErrorKind::AuthError;
    }
    if contains_any(&s, &["400", "bad request", "invalid request", "invalid_request_error",
        "string should match pattern", "tool_use.id", "tool_use_id",
        "messages.1.content.1.tool_use.id", "invalid request format"]) {
        return ProviderErrorKind::BadRequest;
    }
    if contains_any(&s, &["500", "502", "503", "521", "522", "523", "524", "529",
        "internal server error", "service unavailable", "bad gateway"]) {
        return ProviderErrorKind::ServerError;
    }

    ProviderErrorKind::Unknown
}

fn contains_any(haystack: &str, needles: &[&str]) -> bool {
    needles.iter().any(|n| haystack.contains(n))
}

// ── 单元测试 ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_from_status_400() {
        let e = ProviderError::from_status(400, "bad request", None);
        assert_eq!(e.kind, ProviderErrorKind::BadRequest);
        assert!(!e.kind.is_retryable());
        assert!(!e.kind.counts_as_circuit_failure());
    }

    #[test]
    fn test_from_status_401() {
        let e = ProviderError::from_status(401, "unauthorized", None);
        assert_eq!(e.kind, ProviderErrorKind::AuthError);
        assert!(!e.kind.is_retryable());
        assert!(!e.kind.counts_as_circuit_failure());
    }

    #[test]
    fn test_from_status_402_billing() {
        let e = ProviderError::from_status(402, "payment required", None);
        assert_eq!(e.kind, ProviderErrorKind::Billing);
        assert!(!e.kind.is_retryable());
        assert!(e.kind.counts_as_circuit_failure());
    }

    #[test]
    fn test_from_status_403() {
        let e = ProviderError::from_status(403, "forbidden", None);
        assert_eq!(e.kind, ProviderErrorKind::AuthError);
    }

    #[test]
    fn test_from_status_408_timeout() {
        let e = ProviderError::from_status(408, "request timeout", None);
        assert_eq!(e.kind, ProviderErrorKind::Timeout);
        assert!(e.kind.is_retryable());
        assert!(e.kind.counts_as_circuit_failure());
    }

    #[test]
    fn test_from_status_429_with_retry_after() {
        let e = ProviderError::from_status(429, "rate limited", Some(30));
        assert_eq!(e.kind, ProviderErrorKind::RateLimit);
        assert_eq!(e.retry_after_secs, Some(30));
        assert!(!e.kind.is_retryable());
        assert!(e.kind.counts_as_circuit_failure());
    }

    #[test]
    fn test_from_status_500() {
        let e = ProviderError::from_status(500, "internal error", None);
        assert_eq!(e.kind, ProviderErrorKind::ServerError);
        assert!(e.kind.is_retryable());
        assert!(e.kind.counts_as_circuit_failure());
    }

    #[test]
    fn test_from_status_unknown() {
        let e = ProviderError::from_status(418, "I'm a teapot", None);
        assert_eq!(e.kind, ProviderErrorKind::Unknown);
        assert!(e.kind.is_retryable());
    }

    #[test]
    fn test_classify_error_downcast() {
        let pe = ProviderError::from_status(429, "rate limited", Some(10));
        let ae = anyhow::Error::new(pe);
        assert_eq!(classify_error(&ae), ProviderErrorKind::RateLimit);
    }

    #[test]
    fn test_classify_error_downcast_billing() {
        let pe = ProviderError::from_status(402, "payment required", None);
        let ae = anyhow::Error::new(pe);
        assert_eq!(classify_error(&ae), ProviderErrorKind::Billing);
    }

    #[test]
    fn test_classify_rate_limit_429_string() {
        assert_eq!(classify_error(&anyhow::anyhow!("HTTP 429 Too Many Requests")), ProviderErrorKind::RateLimit);
    }

    #[test]
    fn test_classify_rate_limit_patterns() {
        for pattern in &[
            "rate limit exceeded", "rate_limit reached", "too many requests",
            "exceeded your current quota", "resource has been exhausted",
            "resource_exhausted", "quota exceeded", "usage limit reached",
        ] {
            assert_eq!(classify_error(&anyhow::anyhow!("{}", pattern)), ProviderErrorKind::RateLimit,
                "pattern should classify as RateLimit: {pattern}");
        }
    }

    #[test]
    fn test_classify_overloaded_as_rate_limit() {
        for pattern in &["overloaded_error", r#"{"type": "overloaded_error"}"#, "server is overloaded"] {
            assert_eq!(classify_error(&anyhow::anyhow!("{}", pattern)), ProviderErrorKind::RateLimit,
                "overloaded should → RateLimit: {pattern}");
        }
    }

    #[test]
    fn test_classify_billing_patterns() {
        for pattern in &[
            "402 payment required", "payment required", "insufficient credits to complete the request",
            "credit balance too low", "visit plans & billing page", "insufficient balance in your account",
        ] {
            assert_eq!(classify_error(&anyhow::anyhow!("{}", pattern)), ProviderErrorKind::Billing,
                "billing pattern: {pattern}");
        }
    }

    #[test]
    fn test_classify_timeout_patterns() {
        for pattern in &[
            "408 request timeout", "request timeout", "connection timed out",
            "deadline exceeded", "context deadline exceeded",
        ] {
            assert_eq!(classify_error(&anyhow::anyhow!("{}", pattern)), ProviderErrorKind::Timeout,
                "timeout pattern: {pattern}");
        }
    }

    #[test]
    fn test_classify_auth_patterns() {
        for pattern in &[
            "HTTP 401 Unauthorized: invalid api key", "invalid_api_key",
            "incorrect api key provided", "authentication failed",
            "re-authenticate to continue", "oauth token refresh failed",
            "access denied for this resource", "token has expired",
            "no credentials found", "no api key found",
        ] {
            assert_eq!(classify_error(&anyhow::anyhow!("{}", pattern)), ProviderErrorKind::AuthError,
                "auth pattern: {pattern}");
        }
    }

    #[test]
    fn test_classify_bad_request_patterns() {
        for pattern in &[
            "HTTP 400 Bad Request: invalid_request_error", "invalid request format",
            "string should match pattern for tool_use.id", "tool_use.id is required",
            "messages.1.content.1.tool_use.id must be a string",
        ] {
            assert_eq!(classify_error(&anyhow::anyhow!("{}", pattern)), ProviderErrorKind::BadRequest,
                "bad request pattern: {pattern}");
        }
    }

    #[test]
    fn test_classify_server_error_patterns() {
        for pattern in &["HTTP 500 internal server error", "502 bad gateway", "503 service unavailable"] {
            assert_eq!(classify_error(&anyhow::anyhow!("{}", pattern)), ProviderErrorKind::ServerError,
                "server error pattern: {pattern}");
        }
    }

    #[test]
    fn test_classify_unknown() {
        assert_eq!(classify_error(&anyhow::anyhow!("some completely random error")), ProviderErrorKind::Unknown);
    }

    #[test]
    fn test_display() {
        let e = ProviderError::from_status(429, "rate limited", Some(30));
        assert!(e.to_string().contains("429"));
        assert!(e.to_string().contains("RateLimit"));
    }

    #[test]
    fn test_display_billing() {
        let e = ProviderError::from_status(402, "insufficient credits", None);
        assert!(e.to_string().contains("402"));
        assert!(e.to_string().contains("Billing"));
    }

    #[test]
    fn test_display_timeout() {
        let e = ProviderError::from_status(408, "request timeout", None);
        assert!(e.to_string().contains("408"));
        assert!(e.to_string().contains("Timeout"));
    }
}
