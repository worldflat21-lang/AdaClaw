//! Log-based observer — forwards all events to the `tracing` crate.

use super::{Observer, ObserverEvent};
use std::any::Any;

/// Emits structured log lines for every observer event using `tracing`.
pub struct LogObserver;

impl LogObserver {
    pub fn new() -> Self {
        Self
    }
}

impl Default for LogObserver {
    fn default() -> Self {
        Self::new()
    }
}

impl Observer for LogObserver {
    fn record_event(&self, event: &ObserverEvent) {
        match event {
            ObserverEvent::AgentTurn {
                agent_id,
                provider,
                model,
            } => {
                tracing::debug!(
                    agent_id = %agent_id,
                    provider = %provider,
                    model = %model,
                    "agent turn started"
                );
            }
            ObserverEvent::AgentTurnEnd {
                agent_id,
                provider,
                model,
                duration,
                success,
            } => {
                if *success {
                    tracing::debug!(
                        agent_id = %agent_id,
                        provider = %provider,
                        model = %model,
                        duration_ms = %duration.as_millis(),
                        "agent turn completed"
                    );
                } else {
                    tracing::warn!(
                        agent_id = %agent_id,
                        provider = %provider,
                        model = %model,
                        duration_ms = %duration.as_millis(),
                        "agent turn failed"
                    );
                }
            }
            ObserverEvent::LlmRequest { provider, model } => {
                tracing::debug!(provider = %provider, model = %model, "llm request");
            }
            ObserverEvent::LlmResponse {
                provider,
                model,
                duration,
                success,
                input_tokens,
                output_tokens,
                error_message,
            } => {
                if *success {
                    tracing::debug!(
                        provider = %provider,
                        model = %model,
                        duration_ms = %duration.as_millis(),
                        input_tokens = ?input_tokens,
                        output_tokens = ?output_tokens,
                        "llm response ok"
                    );
                } else {
                    tracing::warn!(
                        provider = %provider,
                        model = %model,
                        duration_ms = %duration.as_millis(),
                        error = ?error_message,
                        "llm response error"
                    );
                }
            }
            ObserverEvent::ToolCall {
                tool,
                duration,
                success,
            } => {
                if *success {
                    tracing::debug!(
                        tool = %tool,
                        duration_ms = %duration.as_millis(),
                        "tool call ok"
                    );
                } else {
                    tracing::warn!(
                        tool = %tool,
                        duration_ms = %duration.as_millis(),
                        "tool call failed"
                    );
                }
            }
            ObserverEvent::ChannelMessage { channel, direction } => {
                tracing::debug!(channel = %channel, direction = %direction, "channel message");
            }
            ObserverEvent::HeartbeatTick => {
                tracing::trace!("heartbeat tick");
            }
            ObserverEvent::Error { component, message } => {
                tracing::error!(component = %component, "observer error: {}", message);
            }
        }
    }

    fn name(&self) -> &str {
        "log"
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn log_observer_name() {
        assert_eq!(LogObserver::new().name(), "log");
    }

    #[test]
    fn log_observer_does_not_panic() {
        let obs = LogObserver::new();
        obs.record_event(&ObserverEvent::AgentTurn {
            agent_id: "a".into(),
            provider: "openai".into(),
            model: "gpt-4o".into(),
        });
        obs.record_event(&ObserverEvent::ToolCall {
            tool: "shell".into(),
            duration: Duration::from_secs(1),
            success: false,
        });
        obs.record_event(&ObserverEvent::HeartbeatTick);
        obs.record_event(&ObserverEvent::Error {
            component: "provider".into(),
            message: "timeout".into(),
        });
    }
}
