//! Runtime trace — structured JSONL event log for tool-call and LLM diagnostics.
//!
//! Events are written to a JSONL file (one JSON object per line).
//! Supports rolling mode (keep last N entries) and full mode (append forever).
//!
//! # Usage
//! ```rust,ignore
//! let tracer = RuntimeTracer::new(".adaclaw/runtime-trace.jsonl", 1000);
//! tracer.record("tool_call", Some("telegram"), None, None, Some(true), "shell ls /tmp");
//! ```

use serde::{Deserialize, Serialize};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use uuid::Uuid;

/// A single runtime trace event, written as one JSONL line.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeTraceEvent {
    pub id: String,
    pub timestamp: String,
    pub event_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub channel: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub success: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
}

impl RuntimeTraceEvent {
    pub fn new(event_type: &str) -> Self {
        Self {
            id: Uuid::new_v4().to_string(),
            timestamp: now_rfc3339(),
            event_type: event_type.to_string(),
            agent_id: None,
            channel: None,
            provider: None,
            model: None,
            success: None,
            message: None,
            duration_ms: None,
        }
    }
}

/// Writes runtime trace events to a JSONL file.
pub struct RuntimeTracer {
    path: PathBuf,
    max_entries: usize,
    write_lock: Mutex<()>,
}

impl RuntimeTracer {
    /// Create a new tracer that writes to `path`, keeping at most `max_entries` lines.
    /// Set `max_entries = 0` to disable rolling (keep all).
    pub fn new(path: impl Into<PathBuf>, max_entries: usize) -> Self {
        Self {
            path: path.into(),
            max_entries,
            write_lock: Mutex::new(()),
        }
    }

    /// Record a structured event.
    pub fn record(&self, event: &RuntimeTraceEvent) {
        let _guard = self.write_lock.lock().unwrap_or_else(|e| e.into_inner());

        if let Some(parent) = self.path.parent() {
            let _ = fs::create_dir_all(parent);
        }

        let line = match serde_json::to_string(event) {
            Ok(l) => l,
            Err(e) => {
                tracing::warn!("runtime_trace serialize error: {}", e);
                return;
            }
        };

        match OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
        {
            Ok(mut f) => {
                let _ = writeln!(f, "{}", line);
            }
            Err(e) => {
                tracing::warn!("runtime_trace write error: {}", e);
                return;
            }
        }

        if self.max_entries > 0 {
            let _ = self.trim_to_last();
        }
    }

    /// Convenience: record a simple event from individual fields.
    #[allow(clippy::too_many_arguments)]
    pub fn record_simple(
        &self,
        event_type: &str,
        agent_id: Option<&str>,
        channel: Option<&str>,
        provider: Option<&str>,
        model: Option<&str>,
        success: Option<bool>,
        message: Option<&str>,
        duration_ms: Option<u64>,
    ) {
        let mut ev = RuntimeTraceEvent::new(event_type);
        ev.agent_id = agent_id.map(str::to_string);
        ev.channel = channel.map(str::to_string);
        ev.provider = provider.map(str::to_string);
        ev.model = model.map(str::to_string);
        ev.success = success;
        ev.message = message.map(str::to_string);
        ev.duration_ms = duration_ms;
        self.record(&ev);
    }

    /// Load recent events from the trace file (most recent first).
    pub fn load_events(&self, limit: usize) -> Vec<RuntimeTraceEvent> {
        load_events_from_path(&self.path, limit, None, None)
    }

    fn trim_to_last(&self) -> std::io::Result<()> {
        let raw = fs::read_to_string(&self.path).unwrap_or_default();
        let lines: Vec<&str> = raw
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .collect();

        if lines.len() <= self.max_entries {
            return Ok(());
        }

        let keep_from = lines.len().saturating_sub(self.max_entries);
        let kept = lines[keep_from..].join("\n") + "\n";
        fs::write(&self.path, kept)
    }
}

/// Load trace events from a path, optionally filtered.
pub fn load_events_from_path(
    path: &Path,
    limit: usize,
    event_filter: Option<&str>,
    contains: Option<&str>,
) -> Vec<RuntimeTraceEvent> {
    if !path.exists() {
        return Vec::new();
    }

    let raw = match fs::read_to_string(path) {
        Ok(r) => r,
        Err(_) => return Vec::new(),
    };

    let mut events: Vec<RuntimeTraceEvent> = raw
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect();

    if let Some(filter) = event_filter {
        let f = filter.to_ascii_lowercase();
        events.retain(|e| e.event_type.to_ascii_lowercase() == f);
    }

    if let Some(needle) = contains {
        let needle_lc = needle.to_ascii_lowercase();
        events.retain(|e| {
            let haystack = format!(
                "{} {} {} {} {}",
                e.event_type,
                e.message.as_deref().unwrap_or_default(),
                e.provider.as_deref().unwrap_or_default(),
                e.model.as_deref().unwrap_or_default(),
                e.agent_id.as_deref().unwrap_or_default(),
            );
            haystack.to_ascii_lowercase().contains(&needle_lc)
        });
    }

    let total = events.len();
    if total > limit && limit > 0 {
        let keep_from = total - limit;
        events = events.split_off(keep_from);
    }

    events.reverse(); // most recent first
    events
}

fn now_rfc3339() -> String {
    // Use SystemTime to avoid pulling in chrono just for timestamps.
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    // Format as ISO-8601 UTC approximation (no sub-second precision needed here).
    let s = secs % 60;
    let m = (secs / 60) % 60;
    let h = (secs / 3600) % 24;
    let days = secs / 86400;
    // Approximate date (good enough for diagnostics logging).
    let year = 1970 + days / 365;
    let day_of_year = days % 365;
    let month = (day_of_year / 30).min(11) + 1;
    let day = (day_of_year % 30) + 1;
    format!("{year:04}-{month:02}-{day:02}T{h:02}:{m:02}:{s:02}Z")
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn record_and_load() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("trace.jsonl");
        let tracer = RuntimeTracer::new(&path, 100);

        tracer.record_simple("tool_call", Some("assistant"), None, None, None, Some(true), Some("shell ls"), None);
        tracer.record_simple("llm_request", Some("assistant"), None, Some("openai"), Some("gpt-4o"), Some(true), None, Some(250));

        let events = tracer.load_events(10);
        assert_eq!(events.len(), 2);
        // Most recent first
        assert_eq!(events[0].event_type, "llm_request");
        assert_eq!(events[1].event_type, "tool_call");
    }

    #[test]
    fn rolling_keeps_last_n_entries() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("rolling.jsonl");
        let tracer = RuntimeTracer::new(&path, 3);

        for i in 0..6 {
            tracer.record_simple(
                "heartbeat", None, None, None, None, None,
                Some(&format!("tick-{i}")), None,
            );
        }

        let events = tracer.load_events(100);
        assert_eq!(events.len(), 3, "rolling should keep last 3 entries, got: {}", events.len());
    }

    #[test]
    fn load_nonexistent_path_returns_empty() {
        let events = load_events_from_path(
            std::path::Path::new("/nonexistent/path/trace.jsonl"),
            100, None, None,
        );
        assert!(events.is_empty());
    }
}
