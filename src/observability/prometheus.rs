//! Prometheus-compatible metrics observer.
//!
//! Implements a lightweight, dependency-free Prometheus text format exporter
//! using `std::sync::atomic` counters. The output is served on `GET /metrics`.
//!
//! ## Metrics exposed
//! - `adaclaw_agent_turns_total{agent_id, provider, model}`
//! - `adaclaw_agent_turn_errors_total{agent_id}`
//! - `adaclaw_tool_calls_total{tool, success}`
//! - `adaclaw_llm_requests_total{provider, model, success}`
//! - `adaclaw_llm_input_tokens_total{provider, model}`
//! - `adaclaw_llm_output_tokens_total{provider, model}`
//! - `adaclaw_channel_messages_total{channel, direction}`
//! - `adaclaw_heartbeat_ticks_total`
//! - `adaclaw_errors_total{component}`

use super::{Observer, ObserverEvent};
use std::any::Any;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

/// A labeled counter set, stored as `Arc<Mutex<HashMap<String, u64>>>`.
#[derive(Clone, Default)]
struct LabeledCounter {
    inner: Arc<Mutex<HashMap<String, u64>>>,
}

impl LabeledCounter {
    fn inc(&self, key: &str) {
        let mut guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        *guard.entry(key.to_string()).or_insert(0) += 1;
    }

    fn add(&self, key: &str, amount: u64) {
        let mut guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        *guard.entry(key.to_string()).or_insert(0) += amount;
    }

    fn snapshot(&self) -> HashMap<String, u64> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
}

pub struct PrometheusObserver {
    // Simple atomic counters
    heartbeat_ticks: AtomicU64,

    // Labeled counters
    agent_turns: LabeledCounter,
    agent_turn_errors: LabeledCounter,
    tool_calls: LabeledCounter,
    llm_requests: LabeledCounter,
    llm_input_tokens: LabeledCounter,
    llm_output_tokens: LabeledCounter,
    channel_messages: LabeledCounter,
    errors: LabeledCounter,
}

impl PrometheusObserver {
    pub fn new() -> Self {
        Self {
            heartbeat_ticks: AtomicU64::new(0),
            agent_turns: LabeledCounter::default(),
            agent_turn_errors: LabeledCounter::default(),
            tool_calls: LabeledCounter::default(),
            llm_requests: LabeledCounter::default(),
            llm_input_tokens: LabeledCounter::default(),
            llm_output_tokens: LabeledCounter::default(),
            channel_messages: LabeledCounter::default(),
            errors: LabeledCounter::default(),
        }
    }

    /// Export all metrics in Prometheus text exposition format.
    pub fn encode(&self) -> String {
        let mut out = String::with_capacity(2048);

        // heartbeat
        append_counter(
            &mut out,
            "adaclaw_heartbeat_ticks_total",
            "Total heartbeat ticks",
            &[],
            self.heartbeat_ticks.load(Ordering::Relaxed),
        );

        // agent turns
        append_labeled_counter(
            &mut out,
            "adaclaw_agent_turns_total",
            "Total agent turns started",
            &self.agent_turns.snapshot(),
        );

        // agent turn errors
        append_labeled_counter(
            &mut out,
            "adaclaw_agent_turn_errors_total",
            "Total agent turn errors",
            &self.agent_turn_errors.snapshot(),
        );

        // tool calls
        append_labeled_counter(
            &mut out,
            "adaclaw_tool_calls_total",
            "Total tool calls",
            &self.tool_calls.snapshot(),
        );

        // LLM requests
        append_labeled_counter(
            &mut out,
            "adaclaw_llm_requests_total",
            "Total LLM provider requests",
            &self.llm_requests.snapshot(),
        );

        // LLM input tokens
        append_labeled_counter(
            &mut out,
            "adaclaw_llm_input_tokens_total",
            "Total LLM input tokens consumed",
            &self.llm_input_tokens.snapshot(),
        );

        // LLM output tokens
        append_labeled_counter(
            &mut out,
            "adaclaw_llm_output_tokens_total",
            "Total LLM output tokens generated",
            &self.llm_output_tokens.snapshot(),
        );

        // channel messages
        append_labeled_counter(
            &mut out,
            "adaclaw_channel_messages_total",
            "Total channel messages",
            &self.channel_messages.snapshot(),
        );

        // errors
        append_labeled_counter(
            &mut out,
            "adaclaw_errors_total",
            "Total errors by component",
            &self.errors.snapshot(),
        );

        out
    }
}

impl Default for PrometheusObserver {
    fn default() -> Self {
        Self::new()
    }
}

impl Observer for PrometheusObserver {
    fn record_event(&self, event: &ObserverEvent) {
        match event {
            ObserverEvent::AgentTurn { agent_id, provider, model } => {
                let key = format!(
                    r#"agent_id="{agent_id}",provider="{provider}",model="{model}""#
                );
                self.agent_turns.inc(&key);
            }
            ObserverEvent::AgentTurnEnd { agent_id, success, .. } => {
                if !success {
                    let key = format!(r#"agent_id="{agent_id}""#);
                    self.agent_turn_errors.inc(&key);
                }
            }
            ObserverEvent::ToolCall { tool, success, .. } => {
                let key = format!(r#"tool="{tool}",success="{success}""#);
                self.tool_calls.inc(&key);
            }
            ObserverEvent::LlmRequest { .. } => {
                // Counted on response instead, to avoid double-counting.
            }
            ObserverEvent::LlmResponse { provider, model, success, input_tokens, output_tokens, .. } => {
                let key = format!(r#"provider="{provider}",model="{model}",success="{success}""#);
                self.llm_requests.inc(&key);
                let tk_key = format!(r#"provider="{provider}",model="{model}""#);
                if let Some(n) = input_tokens {
                    self.llm_input_tokens.add(&tk_key, *n);
                }
                if let Some(n) = output_tokens {
                    self.llm_output_tokens.add(&tk_key, *n);
                }
            }
            ObserverEvent::ChannelMessage { channel, direction } => {
                let key = format!(r#"channel="{channel}",direction="{direction}""#);
                self.channel_messages.inc(&key);
            }
            ObserverEvent::HeartbeatTick => {
                self.heartbeat_ticks.fetch_add(1, Ordering::Relaxed);
            }
            ObserverEvent::Error { component, .. } => {
                let key = format!(r#"component="{component}""#);
                self.errors.inc(&key);
            }
        }
    }

    fn name(&self) -> &str {
        "prometheus"
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

// ── Formatting helpers ────────────────────────────────────────────────────────

fn append_counter(out: &mut String, name: &str, help: &str, labels: &[(&str, &str)], value: u64) {
    out.push_str(&format!("# HELP {name} {help}\n"));
    out.push_str(&format!("# TYPE {name} counter\n"));
    if labels.is_empty() {
        out.push_str(&format!("{name} {value}\n"));
    } else {
        let label_str = labels
            .iter()
            .map(|(k, v)| format!("{k}=\"{v}\""))
            .collect::<Vec<_>>()
            .join(",");
        out.push_str(&format!("{name}{{{label_str}}} {value}\n"));
    }
}

fn append_labeled_counter(out: &mut String, name: &str, help: &str, data: &HashMap<String, u64>) {
    if data.is_empty() {
        return;
    }
    out.push_str(&format!("# HELP {name} {help}\n"));
    out.push_str(&format!("# TYPE {name} counter\n"));
    // Sort for deterministic output
    let mut entries: Vec<_> = data.iter().collect();
    entries.sort_by_key(|(k, _)| k.as_str());
    for (labels, value) in entries {
        if labels.is_empty() {
            out.push_str(&format!("{name} {value}\n"));
        } else {
            out.push_str(&format!("{name}{{{labels}}} {value}\n"));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn prometheus_name() {
        assert_eq!(PrometheusObserver::new().name(), "prometheus");
    }

    #[test]
    fn heartbeat_increments() {
        let obs = PrometheusObserver::new();
        obs.record_event(&ObserverEvent::HeartbeatTick);
        obs.record_event(&ObserverEvent::HeartbeatTick);
        obs.record_event(&ObserverEvent::HeartbeatTick);
        let out = obs.encode();
        assert!(out.contains("adaclaw_heartbeat_ticks_total 3"), "output: {out}");
    }

    #[test]
    fn tool_calls_track_success_and_failure() {
        let obs = PrometheusObserver::new();
        obs.record_event(&ObserverEvent::ToolCall {
            tool: "shell".into(),
            duration: Duration::from_millis(10),
            success: true,
        });
        obs.record_event(&ObserverEvent::ToolCall {
            tool: "shell".into(),
            duration: Duration::from_millis(10),
            success: false,
        });
        let out = obs.encode();
        assert!(out.contains("adaclaw_tool_calls_total"));
        assert!(out.contains("shell"));
    }

    #[test]
    fn llm_response_tracks_tokens() {
        let obs = PrometheusObserver::new();
        obs.record_event(&ObserverEvent::LlmResponse {
            provider: "openai".into(),
            model: "gpt-4o".into(),
            duration: Duration::from_millis(500),
            success: true,
            input_tokens: Some(100),
            output_tokens: Some(50),
            error_message: None,
        });
        let out = obs.encode();
        assert!(out.contains("adaclaw_llm_requests_total"));
        assert!(out.contains("adaclaw_llm_input_tokens_total"));
        assert!(out.contains("adaclaw_llm_output_tokens_total"));
    }

    #[test]
    fn encode_produces_prometheus_text_format() {
        let obs = PrometheusObserver::new();
        obs.record_event(&ObserverEvent::HeartbeatTick);
        let out = obs.encode();
        assert!(out.contains("# HELP adaclaw_heartbeat_ticks_total"));
        assert!(out.contains("# TYPE adaclaw_heartbeat_ticks_total counter"));
    }
}
