//! Gateway 配对码生成
//!
//! # P1-3 修复：密码学安全 + 一次性机制
//!
//! 改动点：
//! - 使用 `OsRng`（密码学安全 RNG）替代 `rand::thread_rng()`
//! - 全局 `OnceLock<Mutex<PairingState>>` 存储当前有效配对码（含过期时间）
//! - `GET /pair`：生成新码（旧码作废），有效期 10 分钟
//! - 配对码格式：6 位数字字符串（`100000`–`999999`）
//!
//! # 使用方式
//!
//! ```text
//! GET /pair
//! → {"pairing_code": "382041", "expires_in_secs": 600}
//! ```
//!
//! 用户取得配对码后，可将其传给客户端在 `/v1/chat` 请求的 Bearer token 位置
//! 进行首次身份验证（一次性，消耗后失效）。

use axum::{Json, response::IntoResponse};
use rand::RngCore;
use rand::rngs::OsRng;
use serde_json::json;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

// ── 配对码有效期 ──────────────────────────────────────────────────────────────

/// 配对码有效期（10 分钟）
const PAIRING_TTL: Duration = Duration::from_secs(600);

// ── 全局状态 ──────────────────────────────────────────────────────────────────

struct PairingState {
    code: Option<String>,
    generated_at: Option<Instant>,
}

impl PairingState {
    fn new() -> Self {
        Self {
            code: None,
            generated_at: None,
        }
    }

    /// 返回当前有效配对码（未过期），若无则 `None`。
    fn current_valid(&self) -> Option<&str> {
        let code = self.code.as_deref()?;
        let at = self.generated_at?;
        if at.elapsed() < PAIRING_TTL {
            Some(code)
        } else {
            None
        }
    }

    /// 生成一个新的 6 位配对码，使旧码作废。
    /// 使用 `OsRng` 保证密码学安全。
    fn generate_new(&mut self) -> &str {
        let code = generate_secure_code();
        self.code = Some(code);
        self.generated_at = Some(Instant::now());
        self.code.as_deref().unwrap()
    }

    /// 尝试验证并消耗配对码。
    /// 返回 `true` 表示码正确且未过期（已消耗，不可重用）。
    pub fn consume(&mut self, candidate: &str) -> bool {
        if let Some(code) = self.current_valid()
            && constant_time_eq(code.as_bytes(), candidate.as_bytes())
        {
            // 消耗：清除码，防止重放
            self.code = None;
            self.generated_at = None;
            return true;
        }
        false
    }
}

static PAIRING: OnceLock<Mutex<PairingState>> = OnceLock::new();

fn pairing_state() -> &'static Mutex<PairingState> {
    PAIRING.get_or_init(|| Mutex::new(PairingState::new()))
}

// ── 密码学安全 6 位数码生成 ───────────────────────────────────────────────────

/// 使用 `OsRng` 生成密码学安全的 6 位数字配对码（`100000`–`999999`）。
pub fn generate_pairing_code() -> String {
    generate_secure_code()
}

fn generate_secure_code() -> String {
    // 生成均匀分布的 [100_000, 999_999] 整数
    // 使用拒绝采样避免模偏差
    let range = 900_000u32; // 999_999 - 100_000 + 1
    loop {
        let mut buf = [0u8; 4];
        OsRng.fill_bytes(&mut buf);
        let n = u32::from_be_bytes(buf);
        if n < u32::MAX - (u32::MAX % range) {
            return format!("{}", 100_000 + (n % range));
        }
        // 极小概率重试（< 0.02%）
    }
}

/// 常量时间字符串比较（防时序攻击）。
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let diff = a
        .iter()
        .zip(b.iter())
        .fold(0u8, |acc, (x, y)| acc | (x ^ y));
    diff == 0
}

// ── Handler ───────────────────────────────────────────────────────────────────

/// `GET /pair`
///
/// 生成一个新的 6 位一次性配对码（旧码作废），有效期 10 分钟。
/// 用户或客户端在该窗口内使用此码进行身份验证。
pub async fn pair() -> impl IntoResponse {
    let mut state = pairing_state().lock().unwrap();
    let code = state.generate_new().to_string();
    let expires_in = PAIRING_TTL.as_secs();

    Json(json!({
        "pairing_code": code,
        "expires_in_secs": expires_in,
    }))
}

/// 验证并消耗配对码（供其他端点调用，如首次 /v1/chat 认证）。
///
/// 返回 `true` 表示码正确且未过期（已消耗）。
pub fn verify_and_consume(candidate: &str) -> bool {
    pairing_state().lock().unwrap().consume(candidate)
}

// ── 单元测试 ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_generate_pairing_code_range() {
        for _ in 0..1000 {
            let code = generate_pairing_code();
            let n: u32 = code.parse().expect("must be numeric");
            assert!(n >= 100_000 && n <= 999_999, "code out of range: {}", code);
        }
    }

    #[test]
    fn test_pairing_state_lifecycle() {
        let mut state = PairingState::new();
        assert!(
            state.current_valid().is_none(),
            "new state should have no code"
        );

        let code = state.generate_new().to_string();
        assert!(
            state.current_valid().is_some(),
            "code should be valid after generation"
        );

        // Wrong code → rejected, code still present
        assert!(!state.consume("000000"));
        assert!(
            state.current_valid().is_some(),
            "wrong code must not consume the valid one"
        );

        // Correct code → consumed
        assert!(state.consume(&code));
        assert!(
            state.current_valid().is_none(),
            "consumed code must be cleared"
        );

        // Replay attempt → rejected
        assert!(!state.consume(&code));
    }

    #[test]
    fn test_constant_time_eq() {
        assert!(constant_time_eq(b"382041", b"382041"));
        assert!(!constant_time_eq(b"382041", b"382042"));
        assert!(!constant_time_eq(b"short", b"longer_string"));
    }
}
