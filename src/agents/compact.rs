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

/// Prompt-token level at which we start a rolling compaction even if the
/// message *count* is still below `ROLLING_THRESHOLD`.
///
/// This catches token-dense conversations (e.g. a handful of large tool
/// outputs) that the message-count heuristic alone would miss — a single 30k
/// token tool result in message 5 would otherwise never trigger compaction.
/// The value is sourced from the provider's reported `usage.prompt_tokens`, so
/// it reflects the *actual* context size rather than an estimate.  It is a
/// conservative default; deriving it from each model's context window is a
/// natural future refinement.
const ROLLING_TOKEN_THRESHOLD: u32 = 24_000;

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
/// Triggers when `history.len() >= ROLLING_THRESHOLD` **or** the last reported
/// `prompt_tokens` reaches `ROLLING_TOKEN_THRESHOLD`.
/// Falls back to `trim_history` if the LLM call fails.
///
/// # Parameters
/// - `history`       — the mutable conversation history (modified in-place)
/// - `provider`      — any `Provider` impl used for the summary LLM call
/// - `model`         — model name to use (usually the agent's own model)
/// - `prompt_tokens` — prompt tokens of the most recent request, if the
///   provider reported usage; drives token-based triggering
pub async fn rolling_compact(
    history: &mut Vec<ChatMessage>,
    provider: &dyn Provider,
    model: &str,
    prompt_tokens: Option<u32>,
) -> Result<()> {
    let over_count = history.len() >= ROLLING_THRESHOLD;
    let over_tokens = prompt_tokens.is_some_and(|t| t >= ROLLING_TOKEN_THRESHOLD);
    if !over_count && !over_tokens {
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
    if to_summarise.len() == 1
        && to_summarise[0]
            .content
            .starts_with("[Conversation summary]")
    {
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
                ChatMessage::new("system", format!("[Conversation summary]: {}", summary)),
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
    prompt_tokens: Option<u32>,
) -> Result<()> {
    rolling_compact(history, provider, model, prompt_tokens).await?;
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
            .map(|i| {
                ChatMessage::new(
                    if i % 2 == 0 { "user" } else { "assistant" },
                    format!("message {}", i),
                )
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

    // ── Shared mock provider helpers ──────────────────────────────────────────

    struct PanicProvider;
    #[async_trait::async_trait]
    impl adaclaw_core::provider::Provider for PanicProvider {
        fn name(&self) -> &str {
            "panic"
        }
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
            panic!("chat() should not be called in compact tests")
        }
        async fn chat_with_system(
            &self,
            _system: Option<&str>,
            _msg: &str,
            _model: &str,
            _temp: f64,
        ) -> anyhow::Result<String> {
            panic!("chat_with_system() should not be called")
        }
    }

    /// Provider that always succeeds and returns a fixed summary string.
    struct SummaryProvider;
    #[async_trait::async_trait]
    impl adaclaw_core::provider::Provider for SummaryProvider {
        fn name(&self) -> &str {
            "summary"
        }
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
            panic!("chat() should not be called in compact tests")
        }
        async fn chat_with_system(
            &self,
            _system: Option<&str>,
            _msg: &str,
            _model: &str,
            _temp: f64,
        ) -> anyhow::Result<String> {
            Ok(
                "Key facts: user asked about Rust ownership, assistant explained borrowing rules."
                    .to_string(),
            )
        }
    }

    /// Provider that always fails — simulates LLM unavailability.
    struct FailProvider;
    #[async_trait::async_trait]
    impl adaclaw_core::provider::Provider for FailProvider {
        fn name(&self) -> &str {
            "fail"
        }
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
            panic!("chat() should not be called in compact tests")
        }
        async fn chat_with_system(
            &self,
            _system: Option<&str>,
            _msg: &str,
            _model: &str,
            _temp: f64,
        ) -> anyhow::Result<String> {
            Err(anyhow::anyhow!(
                "LLM service unavailable: connection refused"
            ))
        }
    }

    // ── rolling_compact: noop path ────────────────────────────────────────────

    #[tokio::test]
    async fn test_rolling_compact_noop_when_small() {
        // PanicProvider must NOT be called — history is below ROLLING_THRESHOLD.
        let mut h = make_history(ROLLING_THRESHOLD - 1);
        rolling_compact(&mut h, &PanicProvider, "any-model", None)
            .await
            .unwrap();
        assert_eq!(
            h.len(),
            ROLLING_THRESHOLD - 1,
            "short history must be unchanged"
        );
    }

    // ── rolling_compact: success path ─────────────────────────────────────────

    #[tokio::test]
    async fn test_rolling_compact_calls_llm_and_inserts_summary() {
        // Verify that rolling_compact:
        //   1. Calls the LLM with the old messages
        //   2. Inserts the summary at index 1
        //   3. Preserves history[0] (first message) and the ROLLING_KEEP recent tail
        let mut h = make_history(ROLLING_THRESHOLD + 5);
        let original_len = h.len();
        let first_content = h[0].content.clone();
        let last_content = h.last().unwrap().content.clone();

        rolling_compact(&mut h, &SummaryProvider, "gpt-4", None)
            .await
            .unwrap();

        assert!(
            h.len() < original_len,
            "compacted history must be shorter than original ({} vs {})",
            h.len(),
            original_len
        );
        assert_eq!(
            h[0].content, first_content,
            "first message must be preserved verbatim"
        );
        assert_eq!(
            h.last().unwrap().content,
            last_content,
            "most recent message must be in the kept tail"
        );
        // Summary inserted at index 1
        assert!(
            h[1].content.starts_with("[Conversation summary]"),
            "summary must be inserted at index 1; got: {:?}",
            h[1].content
        );
        assert!(
            h[1].content.contains("Rust ownership"),
            "summary content must be from provider response"
        );
        // After compaction, recent tail = ROLLING_KEEP messages plus history[0] + summary
        assert!(
            h.len() <= ROLLING_KEEP + 2,
            "post-compaction length must be <= ROLLING_KEEP + 2 (first + summary + tail); got {}",
            h.len()
        );
    }

    #[tokio::test]
    async fn test_rolling_compact_result_stays_below_rolling_threshold() {
        // After a successful compaction the history should drop below ROLLING_THRESHOLD
        // so a second immediate call is a no-op (no infinite LLM-call loop).
        let mut h = make_history(ROLLING_THRESHOLD + 5);

        rolling_compact(&mut h, &SummaryProvider, "gpt-4", None)
            .await
            .unwrap();
        let after_first = h.len();

        // Second call must be a no-op (PanicProvider would panic if called).
        rolling_compact(&mut h, &PanicProvider, "gpt-4", None)
            .await
            .unwrap();

        assert_eq!(
            h.len(),
            after_first,
            "second call must be a no-op — history is still below ROLLING_THRESHOLD"
        );
    }

    // ── rolling_compact: LLM failure → graceful hard-trim fallback ───────────

    #[tokio::test]
    async fn test_rolling_compact_llm_failure_falls_back_to_hard_trim() {
        // When the LLM summarisation call fails, rolling_compact must NOT return Err
        // (it catches the failure internally and applies hard trim as a safety net).
        let mut h = make_history(ROLLING_THRESHOLD + 5);
        let first_content = h[0].content.clone();
        let last_content = h.last().unwrap().content.clone();

        // Should complete without propagating the LLM error
        rolling_compact(&mut h, &FailProvider, "model", None)
            .await
            .unwrap();

        // Hard trim was applied — history is within HARD_MAX_HISTORY
        assert!(
            h.len() <= HARD_MAX_HISTORY,
            "hard trim must be applied after LLM failure; len={}",
            h.len()
        );
        assert_eq!(
            h[0].content, first_content,
            "first message must survive the fallback trim"
        );
        assert_eq!(
            h.last().unwrap().content,
            last_content,
            "most recent message must survive the fallback trim"
        );
        // The history must not contain a "[Conversation summary]" entry (LLM failed)
        let has_summary = h
            .iter()
            .any(|m| m.content.starts_with("[Conversation summary]"));
        assert!(!has_summary, "no summary must be inserted when LLM fails");
    }

    // ── rolling_compact: token-based trigger ─────────────────────────────────

    #[tokio::test]
    async fn test_rolling_compact_triggers_on_high_prompt_tokens() {
        // History is *below* the message-count threshold (20 < ROLLING_THRESHOLD
        // = 30), so the count-based trigger would not fire — but high reported
        // prompt_tokens must.
        let mut h = make_history(20);
        rolling_compact(&mut h, &SummaryProvider, "gpt-4", Some(25_000))
            .await
            .unwrap();
        assert!(
            h[1].content.starts_with("[Conversation summary]"),
            "high prompt_tokens must trigger compaction even below the message count"
        );
    }

    #[tokio::test]
    async fn test_rolling_compact_noop_when_tokens_below_threshold() {
        // Below both triggers → PanicProvider must NOT be called.
        let mut h = make_history(20);
        rolling_compact(&mut h, &PanicProvider, "gpt-4", Some(1_000))
            .await
            .unwrap();
        assert_eq!(h.len(), 20, "below both triggers must be a no-op");
    }

    // ── auto_compact_history ─────────────────────────────────────────────────

    #[tokio::test]
    async fn test_auto_compact_history_noop_below_threshold() {
        let mut h = make_history(ROLLING_THRESHOLD - 1);
        // PanicProvider must not be called
        auto_compact_history(&mut h, &PanicProvider, "model", None)
            .await
            .unwrap();
        assert_eq!(
            h.len(),
            ROLLING_THRESHOLD - 1,
            "auto_compact must be a no-op when below threshold"
        );
    }

    #[tokio::test]
    async fn test_auto_compact_history_compacts_and_applies_safety_net() {
        // Verify the two-stage flow:
        //   1. rolling_compact reduces history (via SummaryProvider)
        //   2. safety net trim fires if somehow still over HARD_MAX_HISTORY
        //      (not realistic after rolling_compact, but the net must not break things)
        let mut h = make_history(ROLLING_THRESHOLD + 20);
        let first_content = h[0].content.clone();
        let last_content = h.last().unwrap().content.clone();

        auto_compact_history(&mut h, &SummaryProvider, "model", None)
            .await
            .unwrap();

        assert!(
            h.len() <= HARD_MAX_HISTORY,
            "must be within hard max after auto_compact"
        );
        assert_eq!(h[0].content, first_content, "first message preserved");
        assert_eq!(
            h.last().unwrap().content,
            last_content,
            "last message preserved"
        );
    }

    #[tokio::test]
    async fn test_auto_compact_history_failure_still_within_bounds() {
        // Even when LLM fails and rolling_compact falls back to trim, auto_compact_history
        // must succeed (Ok) and leave history within HARD_MAX_HISTORY.
        let mut h = make_history(ROLLING_THRESHOLD + 20);
        auto_compact_history(&mut h, &FailProvider, "model", None)
            .await
            .unwrap();
        assert!(
            h.len() <= HARD_MAX_HISTORY,
            "history must be within hard max even after LLM failure; len={}",
            h.len()
        );
    }

    // ── hard trim edge cases ──────────────────────────────────────────────────

    #[test]
    fn test_hard_trim_safety_net() {
        let mut h = make_history(HARD_MAX_HISTORY + 10);
        trim_history(&mut h);
        // After hard trim: message[0] + HARD_KEEP_RECENT most recent
        assert!(h.len() <= HARD_MAX_HISTORY);
        assert_eq!(h[0].content, "message 0");
    }

    #[test]
    fn test_hard_trim_at_exact_boundary() {
        // Exactly HARD_MAX_HISTORY messages: trim should be a no-op.
        let mut h = make_history(HARD_MAX_HISTORY);
        let len_before = h.len();
        trim_history(&mut h);
        assert_eq!(
            h.len(),
            len_before,
            "trim at exact HARD_MAX_HISTORY must be a no-op"
        );
    }
}
