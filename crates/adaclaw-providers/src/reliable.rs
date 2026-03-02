//! `ReliabilityChain` — 高可用 Provider 包装器
//!
//! 提供三层容错机制：
//! 1. **错误分类** — 区分可重试错误（5xx/Timeout）与不可重试错误（400/401/402/403）
//! 2. **指数退避重试** — 对可重试错误退避后重试，上限 `MAX_BACKOFF_MS`
//! 3. **熔断器（指数冷却）** — 连续失败后进入冷却期，冷却时长按次数指数增长
//! 4. **Provider 故障切换** — 当前 Provider 重试耗尽后，切换链中下一个
//!
//! ## 错误分类行为
//!
//! | 错误类型           | 退避重试？ | 触发熔断器？ | 说明                                      |
//! |------------------|---------|------------|------------------------------------------|
//! | 429 RateLimit    | 否       | 是          | 立即切换下一个 Provider，尊重 Retry-After     |
//! | 402 Billing      | 否       | 是          | 账单问题，立即切换下一个 Provider             |
//! | 401/403 Auth     | 否       | **否**      | 认证配置问题，切换下一个 Provider（不计熔断）    |
//! | 400 BadRequest   | 否       | **否**      | 请求格式问题，切换下一个 Provider（不计熔断）    |
//! | 5xx Server       | 是       | 是          | 服务端瞬态故障，退避后重试                     |
//! | 408 Timeout      | 是       | 是          | 超时，退避后重试                             |
//! | Unknown          | 是       | 是          | 保守策略：退避后重试                          |
//!
//! ## 熔断冷却公式（参考 picoclaw）
//!
//! ```text
//! cooldown = min(1h, 1min × 5^min(errors-1, 3))
//!   1 次失败 →  1 分钟
//!   2 次失败 →  5 分钟
//!   3 次失败 → 25 分钟
//!   4+ 次失败 →  1 小时（上限）
//! ```

use adaclaw_core::provider::{
    ChatMessage, ChatRequest, ChatResponse, Provider, ProviderCapabilities,
};
use anyhow::Result;
use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};
use tracing::{debug, warn};

use crate::error::{ProviderError, ProviderErrorKind, classify_error};

// ── 常量 ──────────────────────────────────────────────────────────────────────

/// 每个 Provider 的最大重试次数（不含首次尝试）。
const MAX_RETRIES: u32 = 3;

/// 首次退避时间（毫秒）。
const INITIAL_BACKOFF_MS: u64 = 1_000;

/// 退避倍率（指数增长）。
const BACKOFF_MULTIPLIER: u64 = 2;

/// 退避上限（毫秒，30 秒）。
const MAX_BACKOFF_MS: u64 = 30_000;

/// 熔断冷却基础时间（60 秒 = 1 分钟），用于指数计算基数。
const COOLDOWN_BASE_SECS: u64 = 60;

/// 熔断冷却上限（1 小时）。
const COOLDOWN_MAX_SECS: u64 = 3_600;

// ── CooldownTracker ───────────────────────────────────────────────────────────

/// 记录每个 Provider 的健康状态，实现带指数退避的熔断器逻辑。
///
/// 冷却公式（参考 picoclaw `calculateStandardCooldown`）：
/// `min(1h, 1min × 5^min(failures-1, 3))`
///
/// | 累计失败次数 | 冷却时长 |
/// |------------|---------|
/// | 1          | 1 分钟  |
/// | 2          | 5 分钟  |
/// | 3          | 25 分钟 |
/// | 4+         | 1 小时  |
///
/// 所有状态通过 `Mutex` 保护，支持 `&self` 跨 async 任务访问。
pub struct CooldownTracker {
    /// 熔断器打开的时间戳 + 冷却时长（per-provider）。
    cooldowns: Mutex<HashMap<String, (Instant, Duration)>>,
    /// 累计失败计数（per-provider，成功后清零）。
    failure_counts: Mutex<HashMap<String, u32>>,
}

impl CooldownTracker {
    pub fn new() -> Self {
        Self {
            cooldowns: Mutex::new(HashMap::new()),
            failure_counts: Mutex::new(HashMap::new()),
        }
    }

    /// 计算第 n 次累计失败后的冷却时长。
    ///
    /// 公式：`min(3600s, 60s × 5^min(n-1, 3))`
    fn calculate_cooldown(failure_count: u32) -> Duration {
        let n = failure_count.max(1) as f64;
        let exp = (n - 1.0).min(3.0);
        let secs = (COOLDOWN_BASE_SECS as f64 * 5f64.powf(exp)) as u64;
        Duration::from_secs(secs.min(COOLDOWN_MAX_SECS))
    }

    /// 检查 Provider 的熔断器是否处于打开（冷却）状态。
    ///
    /// 如果冷却期已过，自动恢复（关闭熔断器）并返回 `false`。
    pub fn is_in_cooldown(&self, provider_name: &str) -> bool {
        let elapsed_enough = {
            let cooldowns = self.cooldowns.lock().unwrap();
            match cooldowns.get(provider_name) {
                Some(&(since, duration)) => {
                    if since.elapsed() >= duration {
                        Some(true) // 冷却期已过，需恢复
                    } else {
                        Some(false) // 仍在冷却期
                    }
                }
                None => None, // 没有冷却记录
            }
        }; // cooldowns 锁释放

        match elapsed_enough {
            None => false,       // 未在冷却期
            Some(false) => true, // 仍在冷却期
            Some(true) => {
                // 自动恢复：冷却期已过，清除记录
                self.cooldowns.lock().unwrap().remove(provider_name);
                self.failure_counts.lock().unwrap().remove(provider_name);
                debug!(provider = provider_name, "Circuit breaker auto-recovered");
                false
            }
        }
    }

    /// 返回 Provider 还需冷却多久（0 表示已可用）。
    pub fn cooldown_remaining(&self, provider_name: &str) -> Duration {
        let cooldowns = self.cooldowns.lock().unwrap();
        match cooldowns.get(provider_name) {
            Some(&(since, duration)) => {
                let elapsed = since.elapsed();
                if elapsed < duration {
                    duration - elapsed
                } else {
                    Duration::ZERO
                }
            }
            None => Duration::ZERO,
        }
    }

    /// 记录一次计入熔断器的失败，并按失败次数设置指数冷却时长。
    ///
    /// **注意**：`AuthError`（401/403）和 `BadRequest`（400）不应调用此方法。
    pub fn record_failure(&self, provider_name: &str) {
        let count = {
            let mut counts = self.failure_counts.lock().unwrap();
            let c = counts.entry(provider_name.to_string()).or_insert(0);
            *c += 1;
            *c
        };

        let cooldown_duration = Self::calculate_cooldown(count);
        let mut cooldowns = self.cooldowns.lock().unwrap();
        // 始终更新冷却时间（后续失败延长冷却）
        cooldowns.insert(
            provider_name.to_string(),
            (Instant::now(), cooldown_duration),
        );

        warn!(
            provider = provider_name,
            consecutive_failures = count,
            cooldown_secs = cooldown_duration.as_secs(),
            "Circuit breaker opened/extended"
        );
    }

    /// 记录一次 Provider 成功。重置失败计数并关闭熔断器。
    pub fn record_success(&self, provider_name: &str) {
        self.failure_counts.lock().unwrap().remove(provider_name);
        self.cooldowns.lock().unwrap().remove(provider_name);
    }
}

impl Default for CooldownTracker {
    fn default() -> Self {
        Self::new()
    }
}

// ── ReliabilityChain ──────────────────────────────────────────────────────────

/// 高可用 Provider 链。
///
/// 按顺序尝试链中的每个 Provider，对每个 Provider 应用：
/// - 错误分类（区分可重试 / 不可重试）
/// - 指数退避重试（仅对可重试错误）
/// - 带指数冷却的熔断器保护（`AuthError` / `BadRequest` 不计入熔断计数）
/// - 速率限制/账单错误快速切换（立即跳到下一 Provider，不浪费重试）
///
/// 只有当所有 Provider 均失败时，才向上层返回错误。
pub struct ReliabilityChain {
    providers: Vec<Box<dyn Provider>>,
    tracker: CooldownTracker,
    max_retries: u32,
    initial_backoff_ms: u64,
}

impl ReliabilityChain {
    /// 创建 ReliabilityChain，使用默认参数。
    pub fn new(providers: Vec<Box<dyn Provider>>) -> Self {
        Self {
            providers,
            tracker: CooldownTracker::new(),
            max_retries: MAX_RETRIES,
            initial_backoff_ms: INITIAL_BACKOFF_MS,
        }
    }

    /// 设置每个 Provider 的最大重试次数（builder 模式）。
    pub fn with_max_retries(mut self, retries: u32) -> Self {
        self.max_retries = retries;
        self
    }

    /// 设置熔断器阈值（仅用于向后兼容旧测试，新版使用指数冷却无硬阈值）。
    ///
    /// 注意：新版 CooldownTracker 每次失败都会设置冷却，无需连续失败阈值。
    /// 传入 threshold=1 时，第一次失败即触发冷却。
    pub fn with_circuit_threshold(self, _threshold: u32) -> Self {
        // 新版不需要 threshold，指数冷却从第一次失败就生效
        self
    }

    /// 检查链中是否有可用（未冷却）的 Provider。
    fn has_available_provider(&self) -> bool {
        self.providers
            .iter()
            .any(|p| !self.tracker.is_in_cooldown(p.name()))
    }
}

#[async_trait]
impl Provider for ReliabilityChain {
    fn name(&self) -> &str {
        "reliability_chain"
    }

    fn capabilities(&self) -> ProviderCapabilities {
        // 返回第一个未冷却 Provider 的能力，fallback 到链中第一个
        self.providers
            .iter()
            .find(|p| !self.tracker.is_in_cooldown(p.name()))
            .or_else(|| self.providers.first())
            .map(|p| p.capabilities())
            .unwrap_or(ProviderCapabilities {
                native_tool_calling: false,
                vision: false,
                streaming: false,
            })
    }

    async fn chat(&self, req: ChatRequest<'_>, model: &str, temp: f64) -> Result<ChatResponse> {
        // 前置检查：若所有 Provider 均在冷却中，直接返回清晰错误
        if !self.has_available_provider() {
            return Err(anyhow::anyhow!(
                "All {} provider(s) in the ReliabilityChain are currently in circuit-breaker \
                 cooldown. Please wait before retrying.",
                self.providers.len()
            ));
        }

        let mut last_err = anyhow::anyhow!("ReliabilityChain: no providers available");

        'provider_loop: for provider in &self.providers {
            // 跳过处于熔断冷却中的 Provider
            if self.tracker.is_in_cooldown(provider.name()) {
                let remaining = self.tracker.cooldown_remaining(provider.name());
                warn!(
                    provider = provider.name(),
                    cooldown_remaining_secs = remaining.as_secs(),
                    "Skipping — circuit open (in cooldown)"
                );
                continue;
            }

            let mut backoff_ms = self.initial_backoff_ms;

            for attempt in 0..=self.max_retries {
                if attempt > 0 {
                    let sleep_ms = backoff_ms.min(MAX_BACKOFF_MS);
                    debug!(
                        provider = provider.name(),
                        attempt, sleep_ms, "Exponential backoff before retry"
                    );
                    tokio::time::sleep(Duration::from_millis(sleep_ms)).await;
                    backoff_ms = backoff_ms.saturating_mul(BACKOFF_MULTIPLIER);
                }

                match provider.chat(req.clone(), model, temp).await {
                    Ok(resp) => {
                        self.tracker.record_success(provider.name());
                        return Ok(resp);
                    }
                    Err(e) => {
                        let kind = classify_error(&e);

                        match kind {
                            // ── 不可重试 + 不计入熔断器 ─────────────────────
                            ProviderErrorKind::AuthError | ProviderErrorKind::BadRequest => {
                                warn!(
                                    provider = provider.name(),
                                    attempt,
                                    error_kind = %kind,
                                    error = %e,
                                    "Non-retryable error — skipping provider (not counting as circuit failure)"
                                );
                                last_err = e;
                                // 直接跳到下一个 Provider，不调用 record_failure
                                continue 'provider_loop;
                            }

                            // ── 速率限制：记录熔断失败，立即跳到下一个 Provider ──
                            ProviderErrorKind::RateLimit => {
                                let retry_after = e
                                    .downcast_ref::<ProviderError>()
                                    .and_then(|pe| pe.retry_after_secs);

                                if let Some(secs) = retry_after {
                                    warn!(
                                        provider = provider.name(),
                                        retry_after_secs = secs,
                                        "Rate limited (Retry-After: {}s) — switching to next provider",
                                        secs
                                    );
                                } else {
                                    warn!(
                                        provider = provider.name(),
                                        "Rate limit hit — switching to next provider immediately"
                                    );
                                }
                                self.tracker.record_failure(provider.name());
                                last_err = e;
                                continue 'provider_loop;
                            }

                            // ── 账单错误：记录熔断失败，立即跳到下一个 Provider ──
                            ProviderErrorKind::Billing => {
                                warn!(
                                    provider = provider.name(),
                                    error = %e,
                                    "Billing error — switching to next provider (circuit failure recorded)"
                                );
                                self.tracker.record_failure(provider.name());
                                last_err = e;
                                continue 'provider_loop;
                            }

                            // ── 可重试错误（5xx / Timeout / Unknown）──────────
                            ProviderErrorKind::ServerError
                            | ProviderErrorKind::Timeout
                            | ProviderErrorKind::Unknown => {
                                warn!(
                                    provider = provider.name(),
                                    attempt,
                                    max_retries = self.max_retries,
                                    error_kind = %kind,
                                    error = %e,
                                    "Retryable error — will backoff and retry"
                                );
                                last_err = e;
                                // 继续内层循环（退避后重试）
                            }
                        }
                    }
                }
            }

            // 该 Provider 所有重试耗尽（仅 ServerError / Timeout / Unknown 会到达这里）
            self.tracker.record_failure(provider.name());
        }

        Err(last_err)
    }

    /// 委托给 `self.chat()`，复用全部重试/熔断逻辑，避免代码重复。
    async fn chat_with_system(
        &self,
        system: Option<&str>,
        msg: &str,
        model: &str,
        temp: f64,
    ) -> Result<String> {
        let messages = vec![ChatMessage {
            role: "user".to_string(),
            content: msg.to_string(),
        }];
        let req = ChatRequest {
            messages: &messages,
            system,
        };
        Ok(self.chat(req, model, temp).await?.content)
    }
}

// ── 单元测试 ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::{ProviderError, ProviderErrorKind};

    // ── CooldownTracker 指数冷却公式测试 ─────────────────────────────────────

    #[test]
    fn test_cooldown_formula_1_failure() {
        // 1 次失败 → 60s（1 分钟）
        let d = CooldownTracker::calculate_cooldown(1);
        assert_eq!(d.as_secs(), 60);
    }

    #[test]
    fn test_cooldown_formula_2_failures() {
        // 2 次失败 → 300s（5 分钟）
        let d = CooldownTracker::calculate_cooldown(2);
        assert_eq!(d.as_secs(), 300);
    }

    #[test]
    fn test_cooldown_formula_3_failures() {
        // 3 次失败 → 1500s（25 分钟）
        let d = CooldownTracker::calculate_cooldown(3);
        assert_eq!(d.as_secs(), 1500);
    }

    #[test]
    fn test_cooldown_formula_4plus_failures_capped() {
        // 4+ 次失败 → 3600s（1 小时上限）
        let d = CooldownTracker::calculate_cooldown(4);
        assert_eq!(d.as_secs(), 3600);
        let d = CooldownTracker::calculate_cooldown(10);
        assert_eq!(d.as_secs(), 3600);
    }

    // ── CooldownTracker 行为测试 ──────────────────────────────────────────────

    #[test]
    fn test_cooldown_tracker_not_in_cooldown_initially() {
        let tracker = CooldownTracker::new();
        assert!(!tracker.is_in_cooldown("openai"));
    }

    #[test]
    fn test_cooldown_tracker_opens_after_first_failure() {
        // 新版：第一次失败就打开熔断器（60s 冷却）
        let tracker = CooldownTracker::new();
        tracker.record_failure("openai");
        assert!(tracker.is_in_cooldown("openai"));
    }

    #[test]
    fn test_cooldown_tracker_success_resets() {
        let tracker = CooldownTracker::new();
        tracker.record_failure("openai");
        assert!(tracker.is_in_cooldown("openai"));
        tracker.record_success("openai");
        assert!(!tracker.is_in_cooldown("openai"));
    }

    #[test]
    fn test_cooldown_tracker_independent_providers() {
        let tracker = CooldownTracker::new();
        tracker.record_failure("openai");
        assert!(tracker.is_in_cooldown("openai"));
        assert!(!tracker.is_in_cooldown("anthropic")); // anthropic 不受影响
    }

    #[test]
    fn test_cooldown_tracker_auto_recovery() {
        // 用极短冷却时间测试自动恢复
        let tracker = CooldownTracker::new();
        // 手动注入短冷却
        tracker.cooldowns.lock().unwrap().insert(
            "openai".to_string(),
            (
                Instant::now() - Duration::from_millis(100),
                Duration::from_millis(50),
            ),
        );
        // 冷却已过期，应自动恢复
        assert!(!tracker.is_in_cooldown("openai"));
    }

    #[test]
    fn test_cooldown_remaining() {
        let tracker = CooldownTracker::new();
        assert_eq!(tracker.cooldown_remaining("openai"), Duration::ZERO);
        tracker.record_failure("openai");
        let remaining = tracker.cooldown_remaining("openai");
        // 应接近 60s（第一次失败）
        assert!(remaining > Duration::from_secs(58));
        assert!(remaining <= Duration::from_secs(60));
    }

    // ── 错误分类行为测试 ──────────────────────────────────────────────────────

    #[test]
    fn test_error_kind_retryable_server() {
        let e = ProviderError::from_status(500, "internal server error", None);
        assert!(e.kind.is_retryable());
        assert!(e.kind.counts_as_circuit_failure());
    }

    #[test]
    fn test_error_kind_retryable_timeout() {
        let e = ProviderError::from_status(408, "request timeout", None);
        assert!(e.kind.is_retryable());
        assert!(e.kind.counts_as_circuit_failure());
    }

    #[test]
    fn test_error_kind_not_retryable_auth() {
        let e = ProviderError::from_status(401, "unauthorized", None);
        assert!(!e.kind.is_retryable());
        assert!(!e.kind.counts_as_circuit_failure());
    }

    #[test]
    fn test_error_kind_not_retryable_bad_request() {
        let e = ProviderError::from_status(400, "bad request", None);
        assert!(!e.kind.is_retryable());
        assert!(!e.kind.counts_as_circuit_failure());
    }

    #[test]
    fn test_error_kind_billing_not_retryable_but_counts_failure() {
        let e = ProviderError::from_status(402, "payment required", None);
        assert_eq!(e.kind, ProviderErrorKind::Billing);
        assert!(!e.kind.is_retryable());
        assert!(e.kind.counts_as_circuit_failure());
    }

    #[test]
    fn test_error_kind_rate_limit_not_retryable_but_counts_failure() {
        let e = ProviderError::from_status(429, "too many requests", Some(30));
        assert_eq!(e.kind, ProviderErrorKind::RateLimit);
        assert!(!e.kind.is_retryable());
        assert!(e.kind.counts_as_circuit_failure());
        assert_eq!(e.retry_after_secs, Some(30));
    }

    // ── has_available_provider 测试 ───────────────────────────────────────────

    #[test]
    fn test_has_available_provider_empty() {
        let chain = ReliabilityChain::new(vec![]);
        assert!(!chain.has_available_provider());
    }

    // ── ReliabilityChain async 集成测试 ──────────────────────────────────────

    /// Mock provider that always returns a specific error
    struct AlwaysErrorProvider {
        name: String,
        error_status: u16,
    }

    #[async_trait]
    impl Provider for AlwaysErrorProvider {
        fn name(&self) -> &str {
            &self.name
        }
        fn capabilities(&self) -> ProviderCapabilities {
            ProviderCapabilities {
                native_tool_calling: false,
                vision: false,
                streaming: false,
            }
        }
        async fn chat(
            &self,
            _req: ChatRequest<'_>,
            _model: &str,
            _temp: f64,
        ) -> Result<ChatResponse> {
            Err(anyhow::Error::new(ProviderError::from_status(
                self.error_status,
                "mock error",
                None,
            )))
        }
        async fn chat_with_system(
            &self,
            _s: Option<&str>,
            _m: &str,
            _mo: &str,
            _t: f64,
        ) -> Result<String> {
            Err(anyhow::Error::new(ProviderError::from_status(
                self.error_status,
                "mock error",
                None,
            )))
        }
    }

    /// Mock provider that always succeeds
    struct AlwaysOkProvider {
        name: String,
    }

    #[async_trait]
    impl Provider for AlwaysOkProvider {
        fn name(&self) -> &str {
            &self.name
        }
        fn capabilities(&self) -> ProviderCapabilities {
            ProviderCapabilities {
                native_tool_calling: false,
                vision: false,
                streaming: false,
            }
        }
        async fn chat(
            &self,
            _req: ChatRequest<'_>,
            _model: &str,
            _temp: f64,
        ) -> Result<ChatResponse> {
            Ok(ChatResponse {
                content: "ok".to_string(),
                reasoning_content: None,
            })
        }
        async fn chat_with_system(
            &self,
            _s: Option<&str>,
            _m: &str,
            _mo: &str,
            _t: f64,
        ) -> Result<String> {
            Ok("ok".to_string())
        }
    }

    #[tokio::test]
    async fn test_chain_falls_back_to_second_provider() {
        let chain = ReliabilityChain::new(vec![
            Box::new(AlwaysErrorProvider {
                name: "p1".to_string(),
                error_status: 500,
            }),
            Box::new(AlwaysOkProvider {
                name: "p2".to_string(),
            }),
        ])
        .with_max_retries(0);

        let messages = vec![ChatMessage {
            role: "user".to_string(),
            content: "hi".to_string(),
        }];
        let req = ChatRequest {
            messages: &messages,
            system: None,
        };
        let resp = chain.chat(req, "gpt-4", 0.7).await.unwrap();
        assert_eq!(resp.content, "ok");
    }

    #[tokio::test]
    async fn test_auth_error_skips_provider_without_circuit() {
        let chain = ReliabilityChain::new(vec![
            Box::new(AlwaysErrorProvider {
                name: "p1".to_string(),
                error_status: 401,
            }),
            Box::new(AlwaysOkProvider {
                name: "p2".to_string(),
            }),
        ])
        .with_max_retries(3);

        let messages = vec![ChatMessage {
            role: "user".to_string(),
            content: "hi".to_string(),
        }];
        let req = ChatRequest {
            messages: &messages,
            system: None,
        };
        let resp = chain.chat(req, "gpt-4", 0.7).await.unwrap();
        assert_eq!(resp.content, "ok");
        // Auth error 不应触发熔断器
        assert!(!chain.tracker.is_in_cooldown("p1"));
    }

    #[tokio::test]
    async fn test_bad_request_skips_provider_without_circuit() {
        let chain = ReliabilityChain::new(vec![
            Box::new(AlwaysErrorProvider {
                name: "p1".to_string(),
                error_status: 400,
            }),
            Box::new(AlwaysOkProvider {
                name: "p2".to_string(),
            }),
        ])
        .with_max_retries(3);

        let messages = vec![ChatMessage {
            role: "user".to_string(),
            content: "hi".to_string(),
        }];
        let req = ChatRequest {
            messages: &messages,
            system: None,
        };
        let resp = chain.chat(req, "gpt-4", 0.7).await.unwrap();
        assert_eq!(resp.content, "ok");
        assert!(!chain.tracker.is_in_cooldown("p1"));
    }

    #[tokio::test]
    async fn test_rate_limit_skips_provider_and_counts_failure() {
        let chain = ReliabilityChain::new(vec![
            Box::new(AlwaysErrorProvider {
                name: "p1".to_string(),
                error_status: 429,
            }),
            Box::new(AlwaysOkProvider {
                name: "p2".to_string(),
            }),
        ])
        .with_max_retries(3);

        let messages = vec![ChatMessage {
            role: "user".to_string(),
            content: "hi".to_string(),
        }];
        let req = ChatRequest {
            messages: &messages,
            system: None,
        };
        let resp = chain.chat(req, "gpt-4", 0.7).await.unwrap();
        assert_eq!(resp.content, "ok");
        // RateLimit 应触发熔断器
        assert!(chain.tracker.is_in_cooldown("p1"));
    }

    #[tokio::test]
    async fn test_billing_error_skips_provider_and_counts_failure() {
        let chain = ReliabilityChain::new(vec![
            Box::new(AlwaysErrorProvider {
                name: "p1".to_string(),
                error_status: 402,
            }),
            Box::new(AlwaysOkProvider {
                name: "p2".to_string(),
            }),
        ])
        .with_max_retries(3);

        let messages = vec![ChatMessage {
            role: "user".to_string(),
            content: "hi".to_string(),
        }];
        let req = ChatRequest {
            messages: &messages,
            system: None,
        };
        let resp = chain.chat(req, "gpt-4", 0.7).await.unwrap();
        assert_eq!(resp.content, "ok");
        // Billing 应触发熔断器
        assert!(chain.tracker.is_in_cooldown("p1"));
    }

    #[tokio::test]
    async fn test_timeout_error_is_retryable() {
        let chain = ReliabilityChain::new(vec![
            Box::new(AlwaysErrorProvider {
                name: "p1".to_string(),
                error_status: 408,
            }),
            Box::new(AlwaysOkProvider {
                name: "p2".to_string(),
            }),
        ])
        .with_max_retries(0); // 不重试，fallback 到 p2

        let messages = vec![ChatMessage {
            role: "user".to_string(),
            content: "hi".to_string(),
        }];
        let req = ChatRequest {
            messages: &messages,
            system: None,
        };
        let resp = chain.chat(req, "gpt-4", 0.7).await.unwrap();
        assert_eq!(resp.content, "ok");
        // Timeout 重试耗尽后计入熔断
        assert!(chain.tracker.is_in_cooldown("p1"));
    }

    #[tokio::test]
    async fn test_all_providers_fail_returns_error() {
        let chain = ReliabilityChain::new(vec![
            Box::new(AlwaysErrorProvider {
                name: "p1".to_string(),
                error_status: 500,
            }),
            Box::new(AlwaysErrorProvider {
                name: "p2".to_string(),
                error_status: 500,
            }),
        ])
        .with_max_retries(0);

        let messages = vec![ChatMessage {
            role: "user".to_string(),
            content: "hi".to_string(),
        }];
        let req = ChatRequest {
            messages: &messages,
            system: None,
        };
        let result = chain.chat(req, "gpt-4", 0.7).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_all_providers_in_cooldown_returns_clear_error() {
        let chain = ReliabilityChain::new(vec![Box::new(AlwaysErrorProvider {
            name: "p1".to_string(),
            error_status: 500,
        })])
        .with_max_retries(0);

        let messages = vec![ChatMessage {
            role: "user".to_string(),
            content: "hi".to_string(),
        }];
        let req = ChatRequest {
            messages: &messages,
            system: None,
        };
        // 第一次失败 → p1 进入冷却
        let _ = chain.chat(req.clone(), "gpt-4", 0.7).await;

        // 现在 p1 在冷却中，链应返回清晰错误
        let result = chain.chat(req, "gpt-4", 0.7).await;
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("circuit-breaker cooldown"),
            "错误消息应说明熔断器冷却，实际为: {}",
            err_msg
        );
    }
}
