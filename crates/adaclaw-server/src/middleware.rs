//! Gateway 认证中间件
//!
//! # Bearer Token 认证
//!
//! 受保护端点（`/v1/chat`、`/v1/stop`）要求请求携带有效的 Bearer token：
//! ```text
//! Authorization: Bearer <token>
//! ```
//!
//! Token 在 daemon 启动时通过 [`set_bearer_token`] 注入。
//! 若未配置 token（`None`），所有请求**放行**，以兼容未启用认证的本地开发场景。
//!
//! # 常量时间比较
//!
//! 使用 `subtle::ConstantTimeEq` 防止时序攻击（timing attack）。

use axum::{
    extract::Request,
    http::{StatusCode, header},
    middleware::Next,
    response::{IntoResponse, Response},
};
use std::sync::OnceLock;

// ── 全局 Bearer token 存储 ─────────────────────────────────────────────────────

/// 全局 Bearer token。由 daemon 在启动时调用 [`set_bearer_token`] 注入一次。
/// `None` = 未配置认证，所有请求放行。
static BEARER_TOKEN: OnceLock<Option<String>> = OnceLock::new();

/// 注入 Bearer token（daemon 启动时调用一次）。
///
/// 若 `token` 为 `None` 或空字符串，认证功能不启用（所有请求放行，并打印 WARN 日志）。
///
/// 由于使用 `OnceLock`，重复调用时返回 `false` 但不会 panic。
pub fn set_bearer_token(token: Option<String>) -> bool {
    let normalized = token.filter(|t| !t.trim().is_empty());

    if normalized.is_none() {
        tracing::warn!(
            "Gateway: no bearer_token configured — all requests to /v1/chat and /v1/stop are \
             accessible without authentication. Set gateway.bearer_token in config.toml \
             for production use."
        );
    }

    BEARER_TOKEN.set(normalized).is_ok()
}

/// 获取当前配置的 Bearer token（`None` = 未配置）。
pub fn get_bearer_token() -> Option<&'static str> {
    BEARER_TOKEN.get()?.as_deref()
}

// ── require_auth 中间件 ────────────────────────────────────────────────────────

/// Bearer Token 认证中间件。
///
/// 仅当 [`BEARER_TOKEN`] 已通过 [`set_bearer_token`] 注入且不为 `None` 时才执行校验。
/// 通过 `axum::middleware::from_fn(require_auth)` 挂载到需要保护的路由上。
///
/// ## 响应
/// - `200`：token 正确，继续处理请求
/// - `401 Unauthorized`：token 缺失或不匹配
pub async fn require_auth(req: Request, next: Next) -> Response {
    // 如果 BEARER_TOKEN 未初始化（daemon 尚未调用 set_bearer_token），直接放行
    let configured_token = match BEARER_TOKEN.get() {
        Some(t) => t,
        None => return next.run(req).await,
    };

    // 若未配置 token，放行所有请求
    let expected = match configured_token {
        Some(t) => t.as_str(),
        None => return next.run(req).await,
    };

    // 提取请求中的 Authorization 头
    let provided = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "))
        .unwrap_or("");

    // 常量时间比较，防止时序攻击
    if !constant_time_eq(provided.as_bytes(), expected.as_bytes()) {
        return (
            StatusCode::UNAUTHORIZED,
            [("WWW-Authenticate", "Bearer realm=\"AdaClaw Gateway\"")],
            "Unauthorized: invalid or missing Bearer token",
        )
            .into_response();
    }

    next.run(req).await
}

// ── 辅助函数 ──────────────────────────────────────────────────────────────────

/// 常量时间字节比较（防止时序攻击）。
///
/// 两个切片长度不同时立即返回 `false`（长度泄露可接受 — token 长度不是秘密）。
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    // XOR fold：任意字节不匹配时 acc != 0
    let acc = a
        .iter()
        .zip(b.iter())
        .fold(0u8, |acc, (x, y)| acc | (x ^ y));
    acc == 0
}

// ── 单元测试 ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_constant_time_eq_identical() {
        assert!(constant_time_eq(b"secret-token-abc", b"secret-token-abc"));
    }

    #[test]
    fn test_constant_time_eq_different() {
        assert!(!constant_time_eq(b"secret-token-abc", b"secret-token-xyz"));
    }

    #[test]
    fn test_constant_time_eq_different_length() {
        assert!(!constant_time_eq(b"short", b"much-longer-token"));
    }

    #[test]
    fn test_constant_time_eq_empty() {
        assert!(constant_time_eq(b"", b""));
        assert!(!constant_time_eq(b"", b"x"));
    }
}
