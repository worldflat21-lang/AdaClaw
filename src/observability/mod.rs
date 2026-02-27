//! Observability subsystem — Prometheus metrics, structured tracing, runtime trace.
//!
//! # Backends
//! - `noop`       — zero overhead, all calls compile away
//! - `log`        — emit events via `tracing`
//! - `prometheus` — expose `/metrics` in Prometheus text format
//!
//! # Usage
//! ```rust,ignore
//! use crate::observability::{create_observer, ObserverEvent};
//! let obs = create_observer("prometheus");
//! obs.record_event(&ObserverEvent::AgentTurn { agent_id: "assistant".into() });
//! ```

pub mod log;
pub mod noop;
pub mod prometheus;
pub mod trace;

pub use log::LogObserver;
pub use noop::NoopObserver;
pub use prometheus::PrometheusObserver;
pub use trace::{RuntimeTraceEvent, RuntimeTracer};

use std::time::Duration;

// ── Observer trait ────────────────────────────────────────────────────────────

/// Discrete lifecycle events emitted by the AdaClaw agent runtime.
#[derive(Debug, Clone)]
pub enum ObserverEvent {
    /// Agent turn started (one user message → one agent response cycle).
    AgentTurn {
        agent_id: String,
        provider: String,
        model: String,
    },
    /// Agent turn completed (success or failure).
    AgentTurnEnd {
        agent_id: String,
        provider: String,
        model: String,
        duration: Duration,
        success: bool,
    },
    /// LLM provider request started.
    LlmRequest {
        provider: String,
        model: String,
    },
    /// LLM provider response received.
    LlmResponse {
        provider: String,
        model: String,
        duration: Duration,
        success: bool,
        input_tokens: Option<u64>,
        output_tokens: Option<u64>,
        error_message: Option<String>,
    },
    /// Tool call executed.
    ToolCall {
        tool: String,
        duration: Duration,
        success: bool,
    },
    /// Inbound / outbound channel message.
    ChannelMessage {
        channel: String,
        direction: String, // "inbound" | "outbound"
    },
    /// Daemon heartbeat tick.
    HeartbeatTick,
    /// Error in a named component.
    Error {
        component: String,
        message: String,
    },
}

/// Core observability trait.
///
/// All methods are synchronous and must not block. Implementations should
/// buffer internally and flush asynchronously when needed.
pub trait Observer: Send + Sync + 'static {
    fn record_event(&self, event: &ObserverEvent);
    fn flush(&self) {}
    fn name(&self) -> &str;
    fn as_any(&self) -> &dyn std::any::Any;
}

// ── Shared global observer ────────────────────────────────────────────────────

use std::sync::{Arc, OnceLock};

static GLOBAL_OBSERVER: OnceLock<Arc<dyn Observer>> = OnceLock::new();

/// Initialize the global observer (call once at daemon startup).
pub fn init_global(observer: Arc<dyn Observer>) {
    let _ = GLOBAL_OBSERVER.set(observer);
}

/// Record an event on the global observer (no-op if not initialized).
pub fn record(event: ObserverEvent) {
    if let Some(obs) = GLOBAL_OBSERVER.get() {
        obs.record_event(&event);
    }
}

// ── Factory ───────────────────────────────────────────────────────────────────

/// Create an observer from a string backend name.
/// Supported: `"noop"`, `"log"`, `"prometheus"`, `"none"`.
pub fn create_observer(backend: &str) -> Arc<dyn Observer> {
    match backend.trim().to_ascii_lowercase().as_str() {
        "log" | "logging" => Arc::new(LogObserver::new()),
        "prometheus" | "prom" => Arc::new(PrometheusObserver::new()),
        _ => Arc::new(NoopObserver),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn factory_noop() {
        let obs = create_observer("noop");
        assert_eq!(obs.name(), "noop");
    }

    #[test]
    fn factory_log() {
        let obs = create_observer("log");
        assert_eq!(obs.name(), "log");
    }

    #[test]
    fn factory_prometheus() {
        let obs = create_observer("prometheus");
        assert_eq!(obs.name(), "prometheus");
    }

    #[test]
    fn factory_unknown_falls_back_to_noop() {
        let obs = create_observer("xyzzy_unknown");
        assert_eq!(obs.name(), "noop");
    }

    #[test]
    fn record_before_init_is_noop() {
        // Should not panic even if global observer is not set
        record(ObserverEvent::HeartbeatTick);
    }
}
