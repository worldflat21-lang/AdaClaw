//! `MessageTool` — allows a sub-agent to deliver a message directly to the user.
//!
//! Designed for the Heartbeat **long-task** pattern: a sub-agent that is spawned
//! autonomously (not in response to a live user message) needs a way to push its
//! results or progress updates back to the user without going through the main
//! agent dispatch loop.
//!
//! The tool wraps an `Arc<AppMessageBus>` and calls `bus.send_outbound()` with
//! the provided content.  The `channel` and `session_id` parameters are optional;
//! when omitted, the defaults supplied at construction time are used (the
//! originating channel/session of the heartbeat event that triggered the task).
//!
//! ## Usage by a sub-agent
//!
//! ```json
//! {
//!   "name": "message",
//!   "arguments": {
//!     "content": "✅ Task complete: found 3 high-priority emails."
//!   }
//! }
//! ```
//!
//! The tool also accepts optional `channel` and `session_id` overrides, useful
//! when a sub-agent needs to fan-out results to multiple destinations.
//!
//! ## Security
//!
//! Credential patterns are scrubbed from `content` before sending, matching the
//! same policy applied to all other outbound messages in the agent dispatch loop.

use adaclaw_core::channel::{MessageContent, OutboundMessage};
use adaclaw_core::tool::{Tool, ToolResult, ToolSpec};
use anyhow::Result;
use async_trait::async_trait;
use serde_json::{Value, json};
use std::sync::Arc;
use uuid::Uuid;

use crate::bus::queue::AppMessageBus;

// ── MessageTool ───────────────────────────────────────────────────────────────

/// A tool that lets a sub-agent send a message directly to the user via the
/// outbound message bus, bypassing the normal agent-response pipeline.
///
/// Injected by `HeartbeatScheduler` when spawning a sub-agent for a long task.
/// Not included in `adaclaw_tools::registry::all_tools()` — it requires a live
/// `AppMessageBus` reference and is registered at runtime per-agent-spawn.
pub struct MessageTool {
    bus: Arc<AppMessageBus>,
    /// Default target channel (used when the agent omits the `channel` argument).
    default_channel: String,
    /// Default target session ID (used when the agent omits `session_id`).
    default_session_id: String,
}

impl MessageTool {
    /// Create a `MessageTool` bound to the given bus and default routing target.
    ///
    /// `default_channel` and `default_session_id` are the originating
    /// channel/session of the heartbeat event that triggered the long task; they
    /// ensure the sub-agent's output reaches the right user even when the agent
    /// doesn't explicitly specify routing.
    pub fn new(
        bus: Arc<AppMessageBus>,
        default_channel: impl Into<String>,
        default_session_id: impl Into<String>,
    ) -> Self {
        Self {
            bus,
            default_channel: default_channel.into(),
            default_session_id: default_session_id.into(),
        }
    }
}

#[async_trait]
impl Tool for MessageTool {
    fn name(&self) -> &str {
        "message"
    }

    fn description(&self) -> &str {
        "Send a message directly to the user. \
         Use this tool to deliver task results, progress updates, or notifications. \
         The message is sent immediately via the configured channel. \
         Always use this tool to report your final result for long-running tasks."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "required": ["content"],
            "properties": {
                "content": {
                    "type": "string",
                    "description": "The message content to send to the user. \
                                    May contain Markdown formatting."
                },
                "channel": {
                    "type": "string",
                    "description": "Override the target channel identifier \
                                    (optional; defaults to the originating channel)"
                },
                "session_id": {
                    "type": "string",
                    "description": "Override the target session ID \
                                    (optional; defaults to the originating session)"
                }
            }
        })
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: self.name().to_string(),
            description: self.description().to_string(),
            parameters: self.parameters_schema(),
        }
    }

    async fn execute(&self, args: Value) -> Result<ToolResult> {
        let content = match args.get("content").and_then(|v| v.as_str()) {
            Some(c) => c.to_string(),
            None => {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some("Missing required argument: 'content'".to_string()),
                });
            }
        };

        let channel = args
            .get("channel")
            .and_then(|v| v.as_str())
            .unwrap_or(&self.default_channel)
            .to_string();

        let session_id = args
            .get("session_id")
            .and_then(|v| v.as_str())
            .unwrap_or(&self.default_session_id)
            .to_string();

        // Scrub credentials before sending (matches agent dispatch loop policy)
        let safe_content = adaclaw_security::scrub::scrub_credentials(&content);

        let out = OutboundMessage {
            id: Uuid::new_v4(),
            target_channel: channel.clone(),
            target_session_id: session_id.clone(),
            content: MessageContent::Text(safe_content),
            reply_to: None,
        };

        match self.bus.send_outbound(out) {
            Ok(()) => Ok(ToolResult {
                success: true,
                output: format!("Message sent to {}:{}", channel, session_id),
                error: None,
            }),
            Err(e) => Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!("Failed to send message: {}", e)),
            }),
        }
    }
}

// ── tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn make_tool() -> MessageTool {
        // Use tokio mpsc + broadcast channels to construct a real AppMessageBus.
        // We only care that `send_outbound` doesn't panic; we don't need to
        // subscribe to the outbound side in these tests.
        let (inbound_tx, _) = tokio::sync::mpsc::channel(16);
        let (outbound_tx, _) = tokio::sync::broadcast::channel(16);
        let bus = Arc::new(AppMessageBus::new(inbound_tx, outbound_tx, vec![]));
        MessageTool::new(bus, "cli", "test-session")
    }

    #[test]
    fn test_name_and_description() {
        let tool = make_tool();
        assert_eq!(tool.name(), "message");
        assert!(!tool.description().is_empty());
    }

    #[test]
    fn test_spec_parameters_schema_has_content() {
        let tool = make_tool();
        let schema = tool.parameters_schema();
        assert!(
            schema["properties"]["content"].is_object(),
            "schema must have 'content' property"
        );
        let required = schema["required"].as_array().unwrap();
        assert!(
            required.iter().any(|v| v.as_str() == Some("content")),
            "'content' must be required"
        );
    }

    #[tokio::test]
    async fn test_execute_missing_content_returns_error() {
        let tool = make_tool();
        let result = tool.execute(json!({})).await.unwrap();
        assert!(!result.success);
        assert!(result.error.is_some());
        assert!(result.error.unwrap().contains("content"));
    }

    #[tokio::test]
    async fn test_execute_sends_to_default_channel() {
        let (inbound_tx, _) = tokio::sync::mpsc::channel(16);
        let (outbound_tx, mut rx) = tokio::sync::broadcast::channel(16);
        let bus = Arc::new(AppMessageBus::new(inbound_tx, outbound_tx, vec![]));
        let tool = MessageTool::new(Arc::clone(&bus), "telegram", "chat-42");

        let result = tool
            .execute(json!({"content": "hello from sub-agent"}))
            .await
            .unwrap();

        assert!(result.success, "execute should succeed");

        // Verify the outbound message was actually published to the bus
        let msg = rx
            .try_recv()
            .expect("outbound message should be on the bus");
        assert_eq!(msg.target_channel, "telegram");
        assert_eq!(msg.target_session_id, "chat-42");
        match msg.content {
            MessageContent::Text(t) => assert_eq!(t, "hello from sub-agent"),
            _ => panic!("expected Text content"),
        }
    }

    #[tokio::test]
    async fn test_execute_respects_channel_override() {
        let (inbound_tx, _) = tokio::sync::mpsc::channel(16);
        let (outbound_tx, mut rx) = tokio::sync::broadcast::channel(16);
        let bus = Arc::new(AppMessageBus::new(inbound_tx, outbound_tx, vec![]));
        let tool = MessageTool::new(Arc::clone(&bus), "default-channel", "default-session");

        let result = tool
            .execute(json!({
                "content": "override test",
                "channel": "discord",
                "session_id": "guild-123"
            }))
            .await
            .unwrap();

        assert!(result.success);
        let msg = rx
            .try_recv()
            .expect("outbound message should be on the bus");
        assert_eq!(msg.target_channel, "discord");
        assert_eq!(msg.target_session_id, "guild-123");
    }
}
