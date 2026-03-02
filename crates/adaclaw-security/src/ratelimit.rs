//! Rate Limiting — per-user, per-channel, daily cost budget, hourly action limit.
//!
//! Uses an in-memory sliding window counter (no external Redis/DB dependency).
//! All counters are protected by a `Mutex` and are reset per their time window.
//!
//! # Configuration
//!
//! ```toml
//! [security.rate_limit]
//! per_user            = 60    # max messages per user per minute (0 = unlimited)
//! per_channel         = 120   # max messages per channel per minute (0 = unlimited)
//! max_actions_per_hour = 200  # max tool calls per hour (0 = unlimited)
//! daily_cost_budget_usd = 5.0 # max daily LLM spend in USD (0.0 = unlimited)
//! ```

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Mutex;
use tracing::warn;

// ── Configuration ─────────────────────────────────────────────────────────────

/// Rate limit configuration (nested under `[security.rate_limit]`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RateLimitConfig {
    /// Max inbound messages per user per minute. `0` = unlimited.
    #[serde(default = "default_per_user")]
    pub per_user: u32,

    /// Max inbound messages per channel per minute. `0` = unlimited.
    #[serde(default = "default_per_channel")]
    pub per_channel: u32,

    /// Max tool-call actions per hour across all agents. `0` = unlimited.
    #[serde(default = "default_max_actions")]
    pub max_actions_per_hour: u32,

    /// Daily LLM cost budget in USD. `0.0` = unlimited.
    /// Costs are recorded via `record_cost()` — the calling code must
    /// estimate tokens and pass the cost in USD.
    #[serde(default)]
    pub daily_cost_budget_usd: f64,
}

fn default_per_user() -> u32 {
    60
}
/// Default per-channel rate limit.
///
/// Unified with `schema::RateLimitConfig::default_per_channel()` (both 200).
/// Previously this was 120, which differed from the schema default and caused
/// the effective limit to depend on which code path was taken.
fn default_per_channel() -> u32 {
    200
}
fn default_max_actions() -> u32 {
    200
}

impl Default for RateLimitConfig {
    fn default() -> Self {
        Self {
            per_user: default_per_user(),
            per_channel: default_per_channel(),
            max_actions_per_hour: default_max_actions(),
            daily_cost_budget_usd: 0.0,
        }
    }
}

// ── Sliding-window counter ────────────────────────────────────────────────────

/// A simple sliding-window counter backed by a `Vec<DateTime<Utc>>`.
///
/// Old timestamps outside the window are evicted on each call to `try_record()`.
struct WindowCounter {
    /// Timestamps of each recorded event within the current window.
    events: Vec<DateTime<Utc>>,
    /// Width of the sliding window.
    window: Duration,
    /// Maximum events allowed within the window (`0` = unlimited).
    limit: u32,
}

impl WindowCounter {
    fn new(window: Duration, limit: u32) -> Self {
        Self {
            events: Vec::new(),
            window,
            limit,
        }
    }

    /// Try to record a new event. Returns `true` if allowed, `false` if rate-limited.
    fn try_record(&mut self) -> bool {
        if self.limit == 0 {
            return true; // unlimited
        }
        let now = Utc::now();
        let cutoff = now - self.window;
        // Evict stale events
        self.events.retain(|&t| t > cutoff);

        if self.events.len() as u32 >= self.limit {
            false
        } else {
            self.events.push(now);
            true
        }
    }

    /// Current number of events in the window (for error messages).
    fn current_count(&self) -> u32 {
        let now = Utc::now();
        let cutoff = now - self.window;
        self.events.iter().filter(|&&t| t > cutoff).count() as u32
    }
}

// ── RateLimiter ───────────────────────────────────────────────────────────────

/// Thread-safe rate limiter.
///
/// Create once and share via `Arc<RateLimiter>`.
pub struct RateLimiter {
    config: RateLimitConfig,
    /// Per-user sliding window counters (keyed by sender_id).
    user_counters: Mutex<HashMap<String, WindowCounter>>,
    /// Per-channel sliding window counters (keyed by channel name).
    channel_counters: Mutex<HashMap<String, WindowCounter>>,
    /// Global hourly tool-action counter.
    action_counter: Mutex<WindowCounter>,
    /// Daily cost accumulator: (total_usd, day_start).
    daily_cost: Mutex<(f64, DateTime<Utc>)>,
}

impl RateLimiter {
    /// Create a new `RateLimiter` from the given config.
    pub fn new(config: RateLimitConfig) -> Self {
        let action_limit = config.max_actions_per_hour;
        Self {
            config,
            user_counters: Mutex::new(HashMap::new()),
            channel_counters: Mutex::new(HashMap::new()),
            action_counter: Mutex::new(WindowCounter::new(Duration::hours(1), action_limit)),
            daily_cost: Mutex::new((0.0, Utc::now())),
        }
    }

    // ── Message rate checks ───────────────────────────────────────────────────

    /// Check whether an inbound message from `sender_id` on `channel` is allowed.
    ///
    /// Records the event if allowed. Returns `Err(reason)` if rate-limited.
    /// Should be called before dispatching to the agent loop.
    pub fn check_message(&self, sender_id: &str, channel: &str) -> Result<(), String> {
        // ── Per-user limit ────────────────────────────────────────────────────
        if self.config.per_user > 0 {
            let mut map = self.user_counters.lock().unwrap();
            let counter = map
                .entry(sender_id.to_string())
                .or_insert_with(|| WindowCounter::new(Duration::minutes(1), self.config.per_user));

            if !counter.try_record() {
                let msg = format!(
                    "Rate limit: too many messages from user {} ({}/{} per minute). Try again later.",
                    sender_id,
                    counter.current_count(),
                    self.config.per_user
                );
                warn!(sender_id, channel, "Per-user rate limit exceeded");
                return Err(msg);
            }
        }

        // ── Per-channel limit ─────────────────────────────────────────────────
        if self.config.per_channel > 0 {
            let mut map = self.channel_counters.lock().unwrap();
            let counter = map.entry(channel.to_string()).or_insert_with(|| {
                WindowCounter::new(Duration::minutes(1), self.config.per_channel)
            });

            if !counter.try_record() {
                let msg = format!(
                    "Rate limit: channel '{}' is too busy ({}/{} per minute). Try again later.",
                    channel,
                    counter.current_count(),
                    self.config.per_channel
                );
                warn!(channel, "Per-channel rate limit exceeded");
                return Err(msg);
            }
        }

        Ok(())
    }

    // ── Tool-action limit ─────────────────────────────────────────────────────

    /// Record a tool execution and check the hourly action limit.
    ///
    /// Returns `Err(reason)` if the hourly action budget is exhausted.
    /// Call this before executing each tool call.
    pub fn record_action(&self) -> Result<(), String> {
        if self.config.max_actions_per_hour == 0 {
            return Ok(());
        }

        let mut counter = self.action_counter.lock().unwrap();
        if !counter.try_record() {
            let msg = format!(
                "Rate limit: too many tool calls this hour ({}/{} per hour).",
                counter.current_count(),
                self.config.max_actions_per_hour
            );
            warn!("Hourly tool-action limit exceeded");
            return Err(msg);
        }

        Ok(())
    }

    // ── Daily cost budget ─────────────────────────────────────────────────────

    /// Record an LLM cost (in USD) and check the daily budget.
    ///
    /// Returns `Err(reason)` if the daily budget would be exceeded **after**
    /// adding this cost. The cost is always added regardless of the error
    /// (over-budget calls are not silently discarded).
    pub fn record_cost(&self, cost_usd: f64) -> Result<(), String> {
        if self.config.daily_cost_budget_usd <= 0.0 {
            return Ok(()); // unlimited
        }

        let mut daily = self.daily_cost.lock().unwrap();

        // Reset accumulator if it's a new UTC day
        let now = Utc::now();
        if now.date_naive() != daily.1.date_naive() {
            *daily = (0.0, now);
        }

        daily.0 += cost_usd;

        if daily.0 > self.config.daily_cost_budget_usd {
            warn!(
                total_usd = %daily.0,
                budget_usd = %self.config.daily_cost_budget_usd,
                "Daily LLM cost budget exceeded"
            );
            return Err(format!(
                "Daily LLM cost budget exceeded: ${:.4} spent / ${:.2} budget. \
                 Resets at midnight UTC.",
                daily.0, self.config.daily_cost_budget_usd
            ));
        }

        Ok(())
    }

    // ── Getters ───────────────────────────────────────────────────────────────

    /// Return the accumulated cost for the current UTC day (in USD).
    pub fn daily_cost_usd(&self) -> f64 {
        self.daily_cost.lock().unwrap().0
    }

    /// Return the config.
    pub fn config(&self) -> &RateLimitConfig {
        &self.config
    }
}

// ── unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn unlimited() -> RateLimiter {
        RateLimiter::new(RateLimitConfig {
            per_user: 0,
            per_channel: 0,
            max_actions_per_hour: 0,
            daily_cost_budget_usd: 0.0,
        })
    }

    #[test]
    fn test_unlimited_allows_everything() {
        let lim = unlimited();
        for _ in 0..1000 {
            assert!(lim.check_message("user1", "cli").is_ok());
            assert!(lim.record_action().is_ok());
            assert!(lim.record_cost(100.0).is_ok());
        }
    }

    #[test]
    fn test_per_user_rate_limit() {
        let lim = RateLimiter::new(RateLimitConfig {
            per_user: 3,
            per_channel: 0,
            max_actions_per_hour: 0,
            daily_cost_budget_usd: 0.0,
        });
        assert!(lim.check_message("user1", "cli").is_ok()); // 1
        assert!(lim.check_message("user1", "cli").is_ok()); // 2
        assert!(lim.check_message("user1", "cli").is_ok()); // 3
        assert!(lim.check_message("user1", "cli").is_err()); // 4 → denied

        // Different user should not be affected
        assert!(lim.check_message("user2", "cli").is_ok());
    }

    #[test]
    fn test_per_channel_rate_limit() {
        let lim = RateLimiter::new(RateLimitConfig {
            per_user: 0,
            per_channel: 2,
            max_actions_per_hour: 0,
            daily_cost_budget_usd: 0.0,
        });
        assert!(lim.check_message("u1", "telegram").is_ok()); // 1
        assert!(lim.check_message("u2", "telegram").is_ok()); // 2
        assert!(lim.check_message("u3", "telegram").is_err()); // 3 → denied

        // Different channel unaffected
        assert!(lim.check_message("u1", "discord").is_ok());
    }

    #[test]
    fn test_action_rate_limit() {
        let lim = RateLimiter::new(RateLimitConfig {
            per_user: 0,
            per_channel: 0,
            max_actions_per_hour: 3,
            daily_cost_budget_usd: 0.0,
        });
        assert!(lim.record_action().is_ok()); // 1
        assert!(lim.record_action().is_ok()); // 2
        assert!(lim.record_action().is_ok()); // 3
        assert!(lim.record_action().is_err()); // 4 → denied
    }

    #[test]
    fn test_daily_cost_budget_unlimited() {
        let lim = unlimited();
        assert!(lim.record_cost(999.99).is_ok());
        assert_eq!(lim.daily_cost_usd(), 0.0); // not tracked when unlimited
    }

    #[test]
    fn test_daily_cost_budget_exceeded() {
        let lim = RateLimiter::new(RateLimitConfig {
            per_user: 0,
            per_channel: 0,
            max_actions_per_hour: 0,
            daily_cost_budget_usd: 1.0,
        });
        assert!(lim.record_cost(0.50).is_ok());
        assert!((lim.daily_cost_usd() - 0.50).abs() < 1e-9);
        assert!(lim.record_cost(0.51).is_err()); // total = 1.01 > 1.0
    }

    #[test]
    fn test_daily_cost_accumulates() {
        let lim = RateLimiter::new(RateLimitConfig {
            per_user: 0,
            per_channel: 0,
            max_actions_per_hour: 0,
            daily_cost_budget_usd: 10.0,
        });
        lim.record_cost(1.0).unwrap();
        lim.record_cost(2.5).unwrap();
        assert!((lim.daily_cost_usd() - 3.5).abs() < 1e-9);
    }

    #[test]
    fn test_rate_limit_error_message_content() {
        let lim = RateLimiter::new(RateLimitConfig {
            per_user: 1,
            per_channel: 0,
            max_actions_per_hour: 0,
            daily_cost_budget_usd: 0.0,
        });
        lim.check_message("user1", "cli").unwrap();
        let err = lim.check_message("user1", "cli").unwrap_err();
        assert!(err.contains("Rate limit"));
        assert!(err.contains("user1"));
    }
}
