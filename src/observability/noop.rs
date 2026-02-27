//! Zero-overhead no-op observer — all calls compile to nothing.

use super::{Observer, ObserverEvent};
use std::any::Any;

pub struct NoopObserver;

impl Observer for NoopObserver {
    #[inline(always)]
    fn record_event(&self, _event: &ObserverEvent) {}

    fn name(&self) -> &str {
        "noop"
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
    fn noop_name() {
        assert_eq!(NoopObserver.name(), "noop");
    }

    #[test]
    fn noop_does_not_panic_on_any_event() {
        let obs = NoopObserver;
        obs.record_event(&ObserverEvent::HeartbeatTick);
        obs.record_event(&ObserverEvent::AgentTurn {
            agent_id: "assistant".into(),
            provider: "openai".into(),
            model: "gpt-4o".into(),
        });
        obs.record_event(&ObserverEvent::ToolCall {
            tool: "shell".into(),
            duration: Duration::from_millis(10),
            success: true,
        });
        obs.record_event(&ObserverEvent::Error {
            component: "test".into(),
            message: "boom".into(),
        });
        obs.flush(); // must not panic
    }
}
