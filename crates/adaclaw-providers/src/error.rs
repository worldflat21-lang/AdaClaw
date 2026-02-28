//! 结构化 Provider 错误类型。
//!
//! 各 Provider 实现（openai.rs / anthropic.rs 等）在收到 HTTP 错误响应时应返回
//! [`ProviderError`]，以便 [`crate::reliable::ReliabilityChain`] 做出精确的重试决策：
//!
//! | 错误类型              | 重试？ | 触发熔断器？ | 说明                                    |
//! |---------------------|-------|------------|----------------------------------------|
//! | `RateLimit` (429)   | 否     | 是          | 立即切换下一个 Provider，尊重 Retry-After   |
//! | `Billing` (402)     | 否     | 是          | 账单问题，切换下一个 Provider               |
//! | `AuthError` (401/403)| 否    | **否**      | 认证配置问题，不是 Provider 可用性故障        |
//! | `BadRequest` (400)  | 否     | **否**      | 请求格式问题（含 tool_use.id 等），重试无意义  |
//! | `ServerError` (5xx) | 是     | 是          | 服务端瞬态故障，退避后重试                   |
//! | `Timeout` (408/网络) | 是    | 是          | 超时，退避后重试                           |
//! | `Unknown`           | 是     | 是          | 不确定，保守策略：尝试重试                   |

use std::fmt;

// ── 错误类型枚举 ──────────────────────────────────────────────────────────────

/// HTTP 状态码转换后的 Provider 错误类型。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProviderErrorKind {
    /// 429 Too Many Requests — 速率限制。立即切换到下一个 Provider，
    /// 并可选择尊重 `Retry-After` 头指定的等待时间。
    RateLimit,

    /// 402 Payment Required / "insufficient credits" 等账单相关错误。
    /// 不重试，计入熔断器（账单问题可能需要较长时间恢复）。
    Billing,

    /// 401 Unauthorized / 403 Forbidden — 认证/权限错误。
    /// 不重试，不计入熔断器（这不是 Provider 本身的可用性问题）。
    AuthError,

    /// 400 Bad Request — 请求格式错误、参数非法、或 tool_use.id 格式错误等。
    /// 不重试，不计入熔断器（重试也无法解决）。
    BadRequest,

    /// 5xx Server Error — 服务端内部错误。
    /// 应退避重试；计入熔断器失败计数。
    ServerError,

    /// 408 / "timeout" / "timed out" / "deadline exceeded" — 超时。
    /// 应退避重试；计入熔断器失败计数。
    Timeout,

    /// 其他无法分类的错误（DNS 解析失败等）。
    /// 保守策略：退避重试；计入熔断器失败计数。
    Unknown,
}

impl ProviderErrorKind {
    /// 该类型错误是否应触发重试（在当前 Provider 上退避重试）。
    pub fn is_retryable(&self) -> bool {
        matches!(self, Self::ServerError | Self::Timeout | Self::Unknown)
    }

    /// 该类型错误是否应计入熔断器失败计数。
    /// `AuthError` 和 `BadRequest` 不计入，因为这类错误与 Provider 可用性无关。
    pub fn counts_as_circuit_failure(&self) -> bool {
        matches!(
            self,
            Self::ServerError | Self::Timeout | Self::Unknown | Self::RateLimit | Self::Billing
        )
    }
}

impl fmt::Display for ProviderErrorKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RateLimit => write!(f, "RateLimit"),
            Self::Billing => write!(f, "Billing"),
            Self::AuthError => write!(f, "AuthError"),
            Self::BadRequest => write!(f, "BadRequest"),
            Self::ServerError => write!(f, "ServerError"),
            Self::Timeout => write!(f, "Timeout"),
            Self::Unknown => write!(f, "Unknown"),
        }
    }
}

// ── ProviderError 结构体 ──────────────────────────────────────────────────────

/// 结构化 Provider 错误，携带 HTTP 状态码、错误类型及可选的 `Retry-After` 秒数。
///
/// 实现了 [`std::error::Error`]，可通过 `anyhow::Error::new(pe)` 包装，
/// 并通过 `err.downcast_ref::<ProviderError>()` 在 `ReliabilityChain` 中还原。
#[derive(Debug)]
pub struct ProviderError {
    /// 错误分类。
    pub kind: ProviderErrorKind,
    /// 原始 HTTP 状态码（如 429、500）。
    pub status: u16,
    /// 响应体文本（用于调试日志）。
    pub message: String,
    /// 来自 `Retry-After` 响应头的等待秒数（仅 429 时可能有值）。
    pub retry_after_secs: Option<u64>,
}

impl ProviderError {
    /// 根据 HTTP 状态码构造 `ProviderError`。
    ///
    /// # 参数
    /// - `status` — HTTP 状态码
    /// - `body` — 响应体文本（用于错误信息）
    /// - `retry_after_secs` — `Retry-After` 头值（秒，仅 429 时传入）
    pub fn from_status(status: u16, body: &str, retry_after_secs: Option<u64>) -> Self {
        let kind = match status {
            400 => {
                // 细化：检查 body 里是否有明确的 format 错误模式
                // 这些是 Anthropic 的 tool_use 格式错误，即使是 400 也明确是 format 问题
                ProviderErrorKind::BadRequest
            }
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

impl fmt::Display for ProviderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "HTTP {} ({}): {}", self.status, self.kind, self.message)
    }
}

impl std::error::Error for ProviderError {}

// ── 辅助函数 ──────────────────────────────────────────────────────────────────

/// 从 `anyhow::Error` 中提取 `ProviderErrorKind`。
///
/// 优先通过 downcast 获取结构化类型；降级为字符串模式匹配（兼容未升级的 Provider）。
/// 模式覆盖范围参考 picoclaw error_classifier.go (~40 patterns)。
pub fn classify_error(err: &anyhow::Error) -> ProviderErrorKind {
    // 优先：通过 downcast 获取结构化错误
    if let Some(pe) = err.downcast_ref::<ProviderError>() {
        return pe.kind.clone();
    }

    // 降级：字符串模式匹配（兼容未使用 ProviderError 的 Provider）
    let s = err.to_string().to_lowercase();

    // ── 速率限制（RateLimit）────────────────────────────────────────────────
    if contains_any(
        &s,
        &[
            "429",
            "rate limit",
            "rate_limit",
            "ratelimit",
            "too many requests",
            "exceeded your current quota",
            "exceeded quota",
            "resource has been exhausted",
            "resource_exhausted",
            "quota exceeded",
            "usage limit",
        ],
    ) {
        return ProviderErrorKind::RateLimit;
    }

    // ── 过载（Overloaded）→ 等同 RateLimit ──────────────────────────────────
    // 参考 picoclaw: overloaded 归入 rate_limit 处理
    if contains_any(&s, &["overloaded_error", "overloaded"]) {
        return ProviderErrorKind::RateLimit;
    }

    // ── 账单（Billing）──────────────────────────────────────────────────────
    if contains_any(
        &s,
        &[
            "402",
            "payment required",
            "insufficient credits",
            "credit balance",
            "plans & billing",
            "insufficient balance",
            "billing",
        ],
    ) {
        return ProviderErrorKind::Billing;
    }

    // ── 超时（Timeout）──────────────────────────────────────────────────────
    if contains_any(
        &s,
        &[
            "408",
            "timeout",
            "timed out",
            "deadline exceeded",
            "context deadline exceeded",
            "request timeout",
            "connection timed out",
        ],
    ) {
        return ProviderErrorKind::Timeout;
    }

    // ── 认证错误（AuthError）────────────────────────────────────────────────
    if contains_any(
        &s,
        &[
            "401",
            "403",
            "invalid api key",
            "invalid_api_key",
            "incorrect api key",
            "invalid token",
            "authentication",
            "re-authenticate",
            "oauth token refresh failed",
            "unauthorized",
            "forbidden",
            "access denied",
            "expired",
            "token has expired",
            "no credentials found",
            "no api key found",
        ],
    ) {
        return ProviderErrorKind::AuthError;
    }

    // ── 请求格式错误（BadRequest）────────────────────────────────────────────
    // 包含 Anthropic tool_use 格式错误（即使状态码是 400，消息体可能更有辨识度）
    if contains_any(
        &s,
        &[
            "400",
            "bad request",
            "invalid request",
            "invalid_request_error",
            "string should match pattern",
            "tool_use.id",
            "tool_use_id",
            "messages.1.content.1.tool_use.id",
            "invalid request format",
        ],
    ) {
        return ProviderErrorKind::BadRequest;
    }

    // ── 服务端错误（ServerError）────────────────────────────────────────────
    if contains_any(
        &s,
        &[
            "500", "502", "503", "521", "522", "523", "524", "529",
            "internal server error",
            "service unavailable",
            "bad gateway",
        ],
    ) {
        return ProviderErrorKind::ServerError;
    }

    ProviderErrorKind::Unknown
}

/// 检查字符串是否包含给定列表中的任一子串（已小写）。
fn contains_any(haystack: &str, needles: &[&str]) -> bool {
    needles.iter().any(|n| haystack.contains(n))
}

// ── 单元测试 ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── from_status ──────────────────────────────────────────────────────────

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

    // ── classify_error: downcast path ────────────────────────────────────────

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

    // ── classify_error: rate limit patterns ─────────────────────────────────

    #[test]
    fn test_classify_rate_limit_429_string() {
        assert_eq!(
            classify_error(&anyhow::anyhow!("HTTP 429 Too Many Requests")),
            ProviderErrorKind::RateLimit
        );
    }

    #[test]
    fn test_classify_rate_limit_patterns() {
        for pattern in &[
            "rate limit exceeded",
            "rate_limit reached",
            "too many requests",
            "exceeded your current quota",
            "resource has been exhausted",
            "resource_exhausted",
            "quota exceeded",
            "usage limit reached",
        ] {
            assert_eq!(
                classify_error(&anyhow::anyhow!("{}", pattern)),
                ProviderErrorKind::RateLimit,
                "pattern should classify as RateLimit: {pattern}"
            );
        }
    }

    // ── classify_error: overloaded → RateLimit ───────────────────────────────

    #[test]
    fn test_classify_overloaded_as_rate_limit() {
        for pattern in &[
            "overloaded_error",
            r#"{"type": "overloaded_error", "message": "server is overloaded"}"#,
            "server is overloaded",
        ] {
            assert_eq!(
                classify_error(&anyhow::anyhow!("{}", pattern)),
                ProviderErrorKind::RateLimit,
                "overloaded pattern should classify as RateLimit: {pattern}"
            );
        }
    }

    // ── classify_error: billing patterns ─────────────────────────────────────

    #[test]
    fn test_classify_billing_patterns() {
        for pattern in &[
            "402 payment required",
            "payment required",
            "insufficient credits to complete the request",
            "credit balance too low",
            "visit plans & billing page",
            "insufficient balance in your account",
        ] {
            assert_eq!(
                classify_error(&anyhow::anyhow!("{}", pattern)),
                ProviderErrorKind::Billing,
                "billing pattern should classify as Billing: {pattern}"
            );
        }
    }

    // ── classify_error: timeout patterns ─────────────────────────────────────

    #[test]
    fn test_classify_timeout_patterns() {
        for pattern in &[
            "408 request timeout",
            "request timeout",
            "connection timed out",
            "deadline exceeded",
            "context deadline exceeded",
        ] {
            assert_eq!(
                classify_error(&anyhow::anyhow!("{}", pattern)),
                ProviderErrorKind::Timeout,
                "timeout pattern should classify as Timeout: {pattern}"
            );
        }
    }

    // ── classify_error: auth patterns ────────────────────────────────────────

    #[test]
    fn test_classify_auth_patterns() {
        for pattern in &[
            "HTTP 401 Unauthorized: invalid api key",
            "invalid_api_key",
            "incorrect api key provided",
            "authentication failed",
            "re-authenticate to continue",
            "oauth token refresh failed",
            "access denied for this resource",
            "token has expired",
            "no credentials found",
            "no api key found",
        ] {
            assert_eq!(
                classify_error(&anyhow::anyhow!("{}", pattern)),
                ProviderErrorKind::AuthError,
                "auth pattern should classify as AuthError: {pattern}"
            );
        }
    }

    // ── classify_error: bad request / format patterns ────────────────────────

    #[test]
    fn test_classify_bad_request_patterns() {
        for pattern in &[
            "HTTP 400 Bad Request: invalid_request_error",
            "invalid request format",
            // Anthropic tool_use format errors
            "string should match pattern for tool_use.id",
            "tool_use.id is required",
            "messages.1.content.1.tool_use.id must be a string",
        ] {
            assert_eq!(
                classify_error(&anyhow::anyhow!("{}", pattern)),
                ProviderErrorKind::BadRequest,
                "bad request pattern should classify as BadRequest: {pattern}"
            );
        }
    }

    // ── classify_error: server error patterns ────────────────────────────────

    #[test]
    fn test_classify_server_error_patterns() {
        for pattern in &[
            "HTTP 500 internal server error",
            "502 bad gateway",
            "503 service unavailable",
        ] {
            assert_eq!(
                classify_error(&anyhow::anyhow!("{}", pattern)),
                ProviderErrorKind::ServerError,
                "server error pattern: {pattern}"
            );
        }
    }

    // ── classify_error: unknown ───────────────────────────────────────────────

    #[test]
    fn test_classify_unknown() {
        assert_eq!(
            classify_error(&anyhow::anyhow!("some completely random error")),
            ProviderErrorKind::Unknown
        );
    }

    // ── Display ───────────────────────────────────────────────────────────────

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
