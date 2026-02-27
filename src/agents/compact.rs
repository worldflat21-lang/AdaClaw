/// History compaction — "Congee" rolling-summary strategy.
///
/// ## Strategy
///
/// Instead of a hard truncation (which silently discards context) or a single
/// bulk summarisation (which compresses too much at once), we use a *rolling
/// window* approach:
///
/// 1. When `history.len() >= ROLLING_THRESHOLD`, take the **oldest** N messages
///    (everything before the rolling window, but always keep the very first user
///    message intact at index 0) and compress them with a single LLM call.
/// 2. The LLM summary is inserted at index 1 as a `[system]` message so that
///    subsequent turns can still reference it.
/// 3. Only the last `ROLLING_KEEP` messages stay as verbatim history.
/// 4. If the LLM summarisation fails, we fall back to a hard trim so the loop
///    can always continue.
///
/// This is inspired by the "Congee" compaction module in openclaw, adapted to
/// our async Provider trait.
///
/// ## Interaction with topic-aware history
///
/// `AgentEngine` maintains a persistent `history: Vec<MessageEntry>` that
/// includes **hidden** messages (messages from previous topics that have been
/// pruned from the LLM context on topic switch).
///
/// The `messages: Vec<ChatMessage>` passed to these compaction functions is
/// the **already-filtered** visible subset — hidden messages are excluded
/// before compaction runs.  This means:
///
/// - Compaction **never touches hidden messages**.  They remain intact in
///   `AgentEngine::history` for potential topic restoration.
/// - Summaries produced here only cover the current topic's visible context.
/// - If the user switches back to an older topic, its hidden messages are
///   restored as-is, without any compaction-induced data loss.
use adaclaw_core::provider::{ChatMessage, Provider};
use anyhow::Result;

// ── Tuning constants ──────────────────────────────────────────────────────────

/// Absolute ceiling: if history exceeds this, hard-trim unconditionally.
const HARD_MAX_HISTORY: usize = 60;

/// Start a rolling compaction pass when history reaches this length.
const ROLLING_THRESHOLD: usize = 30;

/// How many recent messages to keep verbatim after compaction.
const ROLLING_KEEP: usize = 15;

/// Hard-trim fallback: keep this many recent messages (+ message[0]).
const HARD_KEEP_RECENT: usize = 20;

// ── Public API ────────────────────────────────────────────────────────────────

/// Unconditional hard-trim: keep `history[0]` + the most recent `HARD_KEEP_RECENT`
/// messages. O(n) drain, no LLM call.  Used as a fast path and fallback.
pub fn trim_history(history: &mut Vec<ChatMessage>) {
    if history.len() > HARD_MAX_HISTORY {
        let drain_end = history.len() - HARD_KEEP_RECENT;
        if drain_end > 1 {
            history.drain(1..drain_end);
        }
    }
}

/// Rolling compaction: summarise the *oldest* portion of the history with an
/// LLM call, replacing it with a single summary message.
///
/// Only triggers when `history.len() >= ROLLING_THRESHOLD`.
/// Falls back to `trim_history` if the LLM call fails.
///
/// # Parameters
/// - `history`  — the mutable conversation history (modified in-place)
/// - `provider` — any `Provider` impl used for the summary LLM call
/// - `model`    — model name to use (usually the agent's own model)
pub async fn rolling_compact(
    history: &mut Vec<ChatMessage>,
    provider: &dyn Provider,
    model: &str,
) -> Result<()> {
    if history.len() < ROLLING_THRESHOLD {
        return Ok(());
    }

    // Identify the slice to summarise:
    //   history[0]          → first user message  (always preserved verbatim)
    //   history[1..sum_end] → oldest messages     (to be summarised)
    //   history[sum_end..]  → recent tail         (kept verbatim)
    let sum_end = history.len().saturating_sub(ROLLING_KEEP).max(2);

    if sum_end <= 1 {
        // Nothing to summarise (history is already small enough after saturation)
        return Ok(());
    }

    let to_summarise = &history[1..sum_end];

    // Skip if all "old" messages are already a previous summary (avoid re-summarising
    // a summary on every single turn when the history stays at the threshold).
    if to_summarise.len() == 1 && to_summarise[0].content.starts_with("[Conversation summary]") {
        return Ok(());
    }

    let text_to_summarise: String = to_summarise
        .iter()
        .map(|m| format!("[{}]: {}", m.role, m.content))
        .collect::<Vec<_>>()
        .join("\n");

    let prompt = format!(
        "Summarise the following conversation history concisely. \
         Preserve all key facts, decisions, tool results, and context \
         that may be needed later. Be brief but complete.\n\n{}",
        text_to_summarise
    );

    match provider
        .chat_with_system(
            Some(
                "You are a precise conversation archivist. \
                 Produce a dense, structured summary that captures everything important.",
            ),
            &prompt,
            model,
            0.2, // low temperature for deterministic summaries
        )
        .await
    {
        Ok(summary) => {
            let before = history.len();
            history.drain(1..sum_end);
            history.insert(
                1,
                ChatMessage {
                    role: "system".to_string(),
                    content: format!("[Conversation summary]: {}", summary),
                },
            );
            tracing::debug!(
                before,
                after = history.len(),
                summarised = sum_end - 1,
                "rolling_compact: history compacted"
            );
        }
        Err(e) => {
            tracing::warn!(
                error = %e,
                "rolling_compact: LLM summarisation failed, falling back to hard trim"
            );
            trim_history(history);
        }
    }

    Ok(())
}

/// Convenience wrapper: call `rolling_compact` first; if history still exceeds
/// `HARD_MAX_HISTORY` after that (shouldn't normally happen), hard-trim as a
/// last-resort safety net.
pub async fn auto_compact_history(
    history: &mut Vec<ChatMessage>,
    provider: &dyn Provider,
    model: &str,
) -> Result<()> {
    rolling_compact(history, provider, model).await?;
    // Safety net: ensure we never exceed the absolute hard limit.
    if history.len() > HARD_MAX_HISTORY {
        trim_history(history);
    }
    Ok(())
}

// ── unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn make_history(n: usize) -> Vec<ChatMessage> {
        (0..n)
            .map(|i| ChatMessage {
                role: if i % 2 == 0 {
                    "user".to_string()
                } else {
                    "assistant".to_string()
                },
                content: format!("message {}", i),
            })
            .collect()
    }

    #[test]
    fn test_trim_noop_when_small() {
        let mut h = make_history(10);
        trim_history(&mut h);
        assert_eq!(h.len(), 10, "small history should not be trimmed");
    }

    #[test]
    fn test_trim_fires_when_large() {
        let mut h = make_history(HARD_MAX_HISTORY + 5);
        trim_history(&mut h);
        assert!(
            h.len() <= HARD_MAX_HISTORY,
            "trim must reduce history to <= HARD_MAX_HISTORY"
        );
    }

    #[test]
    fn test_trim_preserves_first_message() {
        let mut h = make_history(HARD_MAX_HISTORY + 5);
        trim_history(&mut h);
        assert_eq!(h[0].content, "message 0", "first message must be preserved");
    }

    #[test]
    fn test_trim_preserves_recent_tail() {
        let n = HARD_MAX_HISTORY + 5;
        let mut h = make_history(n);
        let last = h.last().unwrap().content.clone();
        trim_history(&mut h);
        assert_eq!(
            h.last().unwrap().content,
            last,
            "most recent message must survive trim"
        );
    }

    #[tokio::test]
    async fn test_rolling_compact_noop_when_small() {
        // Use a NopProvider that panics if called — compact shouldn't touch a short history.
        struct PanicProvider;
        #[async_trait::async_trait]
        impl adaclaw_core::provider::Provider for PanicProvider {
            fn name(&self) -> &str { "panic" }
            fn capabilities(&self) -> adaclaw_core::provider::ProviderCapabilities {
                adaclaw_core::provider::ProviderCapabilities {
                    native_tool_calling: false,
                    vision: false,
                    streaming: false,
                }
            }
            async fn chat(
                &self,
                _req: adaclaw_core::provider::ChatRequest<'_>,
                _model: &str,
                _temp: f64,
            ) -> anyhow::Result<adaclaw_core::provider::ChatResponse> {
                panic!("should not be called")
            }
            async fn chat_with_system(
                &self,
                _system: Option<&str>,
                _msg: &str,
                _model: &str,
                _temp: f64,
            ) -> anyhow::Result<String> {
                panic!("should not be called")
            }
        }

        let mut h = make_history(ROLLING_THRESHOLD - 1);
        let p = PanicProvider;
        // Must not call provider
        rolling_compact(&mut h, &p, "any-model").await.unwrap();
        assert_eq!(h.len(), ROLLING_THRESHOLD - 1);
    }

    #[test]
    fn test_hard_trim_safety_net() {
        let mut h = make_history(HARD_MAX_HISTORY + 10);
        trim_history(&mut h);
        // After hard trim: message[0] + HARD_KEEP_RECENT most recent
        assert!(h.len() <= HARD_MAX_HISTORY);
        assert_eq!(h[0].content, "message 0");
    }
}
