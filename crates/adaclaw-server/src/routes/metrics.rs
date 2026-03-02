//! GET /metrics — Prometheus text format metrics endpoint.
//!
//! Returns the current Prometheus metrics if the global observer is a
//! `PrometheusObserver`. Otherwise returns an informational message.
//!
//! # Security note
//! This endpoint is only bound to `127.0.0.1` by default and is not
//! authenticated. If you expose AdaClaw publicly, protect `/metrics`
//! with a reverse proxy (nginx/Cloudflare Access) or VPN.

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};

pub async fn metrics() -> Response {
    // The global observer is managed by the main binary, not this crate.
    // We use a static accessor pattern via a channel set during daemon startup.
    // For now, return the metrics from the thread-local Prometheus state.
    // The daemon populates METRICS_TEXT via set_metrics_encoder().
    let text = METRICS_ENCODER
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .map(|f| f())
        .unwrap_or_else(|| {
            "# AdaClaw metrics not yet initialized\n\
             # Set observability.backend = \"prometheus\" in config.toml\n"
                .to_string()
        });

    (
        StatusCode::OK,
        [(
            axum::http::header::CONTENT_TYPE,
            "text/plain; version=0.0.4; charset=utf-8",
        )],
        text,
    )
        .into_response()
}

// ── Global metrics encoder ────────────────────────────────────────────────────

use std::sync::RwLock;

/// A thread-safe function pointer that returns Prometheus text.
type MetricsEncoder = Box<dyn Fn() -> String + Send + Sync + 'static>;

static METRICS_ENCODER: RwLock<Option<MetricsEncoder>> = RwLock::new(None);

/// Register a Prometheus encoder function.
/// Call this from `daemon/run.rs` after initializing the PrometheusObserver.
pub fn set_metrics_encoder<F>(f: F)
where
    F: Fn() -> String + Send + Sync + 'static,
{
    let mut guard = METRICS_ENCODER.write().unwrap_or_else(|e| e.into_inner());
    *guard = Some(Box::new(f));
}
