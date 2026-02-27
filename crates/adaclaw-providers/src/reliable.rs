//! `ReliabilityChain` — 高可用 Provider 包装器
//!
//! 提供三层容错机制：
//! 1. **指数退避重试** — 每个 Provider 最多重试 `MAX_RETRIES` 次，初始 1 s，每次翻倍，上限 30 s
//! 2. **熔断器** — 连续失败 `threshold` 次后进入冷却期；冷却期结束自动恢复
//! 3. **Provider 故障切换** — 当前 Provider 全部重试耗尽后，切换到链中的下一个

use adaclaw_core::provider::{ChatRequest, ChatResponse, Provider, ProviderCapabilities};
use anyhow::Result;
use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};
use tracing::{debug, warn};

// ── 常量 ──────────────────────────────────────────────────────────────────────

/// 每个 Provider 的最大重试次数（不含首次尝试）。
const MAX_RETRIES: u32 = 3;

/// 首次退避时间（毫秒）。
const INITIAL_BACKOFF_MS: u64 = 1_000;

/// 退避倍率（指数增长）。
const BACKOFF_MULTIPLIER: u64 = 2;

/// 退避上限（毫秒，30 秒）。
const MAX_BACKOFF_MS: u64 = 30_000;

/// 默认连续失败次数阈值，超出后打开熔断器。
const DEFAULT_CIRCUIT_THRESHOLD: u32 = 3;

/// 默认熔断冷却时长（秒）。
const DEFAULT_COOLDOWN_SECS: u64 = 60;

// ── CooldownTracker ───────────────────────────────────────────────────────────

/// 记录每个 Provider 的健康状态，实现熔断器逻辑。
///
/// 所有状态通过 `Mutex` 保护，支持 `&self` 跨 async 任务访问。
pub struct CooldownTracker {
    /// 熔断器打开的时间戳（per-provider）。
    cooldowns: Mutex<HashMap<String, Instant>>,
    /// 连续失败计数（per-provider，成功后清零）。
    failure_counts: Mutex<HashMap<String, u32>>,
    /// 打开熔断器所需的连续失败次数。
    threshold: u32,
    /// 熔断冷却时长（到期后自动恢复）。
    cooldown_duration: Duration,
}

impl CooldownTracker {
    pub fn new(threshold: u32, cooldown_duration: Duration) -> Self {
        Self {
            cooldowns: Mutex::new(HashMap::new()),
            failure_counts: Mutex::new(HashMap::new()),
            threshold,
            cooldown_duration,
        }
    }

    /// 检查 Provider 的熔断器是否处于打开（冷却）状态。
    ///
    /// 如果冷却期已过，自动恢复（关闭熔断器）并返回 `false`。
    pub fn is_in_cooldown(&self, provider_name: &str) -> bool {
        // 先持有 cooldowns 锁做只读检查，尽快释放
        let elapsed_enough = {
            let cooldowns = self.cooldowns.lock().unwrap();
            match cooldowns.get(provider_name) {
                Some(&since) => {
                    if since.elapsed() >= self.cooldown_duration {
                        Some(true) // 冷却期已过，需恢复
                    } else {
                        Some(false) // 仍在冷却期
                    }
                }
                None => None, // 没有冷却记录
            }
        }; // cooldowns 锁释放

        match elapsed_enough {
            None => false, // 未在冷却期
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

    /// 记录一次 Provider 失败。连续失败达到阈值时打开熔断器。
    pub fn record_failure(&self, provider_name: &str) {
        let count = {
            let mut counts = self.failure_counts.lock().unwrap();
            let c = counts.entry(provider_name.to_string()).or_insert(0);
            *c += 1;
            *c
        };

        if count >= self.threshold {
            let mut cooldowns = self.cooldowns.lock().unwrap();
            // 仅在熔断器尚未打开时记录打开时间（避免重置冷却计时器）
            cooldowns
                .entry(provider_name.to_string())
                .or_insert_with(Instant::now);
            warn!(
                provider = provider_name,
                consecutive_failures = count,
                cooldown_secs = self.cooldown_duration.as_secs(),
                "Circuit breaker opened"
            );
        }
    }

    /// 记录一次 Provider 成功。重置失败计数并关闭熔断器。
    pub fn record_success(&self, provider_name: &str) {
        self.failure_counts.lock().unwrap().remove(provider_name);
        self.cooldowns.lock().unwrap().remove(provider_name);
    }
}

// ── ReliabilityChain ──────────────────────────────────────────────────────────

/// 高可用 Provider 链。
///
/// 按顺序尝试链中的每个 Provider，对每个 Provider 应用指数退避重试和熔断器保护。
/// 只有当所有 Provider 均失败时，才向上层返回错误。
pub struct ReliabilityChain {
    providers: Vec<Box<dyn Provider>>,
    tracker: CooldownTracker,
    max_retries: u32,
    initial_backoff_ms: u64,
}

impl ReliabilityChain {
    /// 创建 ReliabilityChain，使用默认参数（最大 3 次重试，60 秒熔断冷却）。
    pub fn new(providers: Vec<Box<dyn Provider>>) -> Self {
        Self {
            providers,
            tracker: CooldownTracker::new(
                DEFAULT_CIRCUIT_THRESHOLD,
                Duration::from_secs(DEFAULT_COOLDOWN_SECS),
            ),
            max_retries: MAX_RETRIES,
            initial_backoff_ms: INITIAL_BACKOFF_MS,
        }
    }

    /// 设置每个 Provider 的最大重试次数（builder 模式）。
    pub fn with_max_retries(mut self, retries: u32) -> Self {
        self.max_retries = retries;
        self
    }

    /// 设置熔断器阈值（连续失败次数，builder 模式）。
    pub fn with_circuit_threshold(mut self, threshold: u32) -> Self {
        self.tracker = CooldownTracker::new(threshold, Duration::from_secs(DEFAULT_COOLDOWN_SECS));
        self
    }

    /// 设置熔断冷却时长（builder 模式）。
    pub fn with_cooldown_secs(mut self, secs: u64) -> Self {
        self.tracker =
            CooldownTracker::new(DEFAULT_CIRCUIT_THRESHOLD, Duration::from_secs(secs));
        self
    }
}

#[async_trait]
impl Provider for ReliabilityChain {
    fn name(&self) -> &str {
        "reliability_chain"
    }

    fn capabilities(&self) -> ProviderCapabilities {
        // 返回第一个非冷却 Provider 的能力，fallback 到链中第一个
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
        let mut last_err = anyhow::anyhow!("ReliabilityChain: no providers configured");

        'provider_loop: for provider in &self.providers {
            // 跳过处于熔断冷却中的 Provider
            if self.tracker.is_in_cooldown(provider.name()) {
                warn!(
                    provider = provider.name(),
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
                        attempt,
                        sleep_ms,
                        "Exponential backoff before retry"
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
                        warn!(
                            provider = provider.name(),
                            attempt,
                            max_retries = self.max_retries,
                            error = %e,
                            "Provider chat failed"
                        );
                        last_err = e;

                        // 速率限制（429）：立即跳到下一个 Provider，不浪费重试次数
                        let err_str = last_err.to_string().to_lowercase();
                        if err_str.contains("429") || err_str.contains("rate limit") {
                            warn!(
                                provider = provider.name(),
                                "Rate limit hit — switching to next provider immediately"
                            );
                            self.tracker.record_failure(provider.name());
                            continue 'provider_loop;
                        }
                    }
                }
            }

            // 该 Provider 所有重试耗尽
            self.tracker.record_failure(provider.name());
        }

        Err(last_err)
    }

    async fn chat_with_system(
        &self,
        system: Option<&str>,
        msg: &str,
        model: &str,
        temp: f64,
    ) -> Result<String> {
        let mut last_err = anyhow::anyhow!("ReliabilityChain: no providers configured");

        'provider_loop: for provider in &self.providers {
            if self.tracker.is_in_cooldown(provider.name()) {
                warn!(
                    provider = provider.name(),
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
                        attempt,
                        sleep_ms,
                        "Exponential backoff before retry"
                    );
                    tokio::time::sleep(Duration::from_millis(sleep_ms)).await;
                    backoff_ms = backoff_ms.saturating_mul(BACKOFF_MULTIPLIER);
                }

                match provider.chat_with_system(system, msg, model, temp).await {
                    Ok(resp) => {
                        self.tracker.record_success(provider.name());
                        return Ok(resp);
                    }
                    Err(e) => {
                        warn!(
                            provider = provider.name(),
                            attempt,
                            max_retries = self.max_retries,
                            error = %e,
                            "Provider chat_with_system failed"
                        );
                        last_err = e;

                        let err_str = last_err.to_string().to_lowercase();
                        if err_str.contains("429") || err_str.contains("rate limit") {
                            warn!(
                                provider = provider.name(),
                                "Rate limit hit — switching to next provider immediately"
                            );
                            self.tracker.record_failure(provider.name());
                            continue 'provider_loop;
                        }
                    }
                }
            }

            self.tracker.record_failure(provider.name());
        }

        Err(last_err)
    }
}

// ── 单元测试 ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cooldown_tracker_not_in_cooldown_initially() {
        let tracker = CooldownTracker::new(3, Duration::from_secs(60));
        assert!(!tracker.is_in_cooldown("openai"));
    }

    #[test]
    fn test_cooldown_tracker_opens_after_threshold() {
        let tracker = CooldownTracker::new(3, Duration::from_secs(60));
        tracker.record_failure("openai");
        tracker.record_failure("openai");
        assert!(!tracker.is_in_cooldown("openai")); // 2 failures, threshold=3
        tracker.record_failure("openai");
        assert!(tracker.is_in_cooldown("openai")); // 3rd failure → circuit open
    }

    #[test]
    fn test_cooldown_tracker_success_resets() {
        let tracker = CooldownTracker::new(2, Duration::from_secs(60));
        tracker.record_failure("openai");
        tracker.record_failure("openai"); // circuit opens
        assert!(tracker.is_in_cooldown("openai"));
        tracker.record_success("openai");
        assert!(!tracker.is_in_cooldown("openai")); // success clears circuit
    }

    #[test]
    fn test_cooldown_tracker_independent_providers() {
        let tracker = CooldownTracker::new(2, Duration::from_secs(60));
        tracker.record_failure("openai");
        tracker.record_failure("openai"); // openai opens
        assert!(tracker.is_in_cooldown("openai"));
        assert!(!tracker.is_in_cooldown("anthropic")); // anthropic unaffected
    }

    #[test]
    fn test_cooldown_tracker_auto_recovery() {
        // Use a very short cooldown for testing
        let tracker = CooldownTracker::new(1, Duration::from_millis(10));
        tracker.record_failure("openai"); // threshold=1 → immediate open
        assert!(tracker.is_in_cooldown("openai"));
        // Wait for cooldown to expire
        std::thread::sleep(Duration::from_millis(20));
        assert!(!tracker.is_in_cooldown("openai")); // auto-recovered
    }
}
