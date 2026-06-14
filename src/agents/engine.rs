use adaclaw_core::memory::{Category, Memory, RecallScope};
use adaclaw_core::provider::{ChatMessage, ChatRequest, ChatResponse, Provider};
use adaclaw_core::tool::Tool;
use adaclaw_memory::session_store::SessionStore;
use anyhow::{Result, anyhow};
use futures_util::future::join_all;
use std::collections::HashSet;
use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::{debug, warn};

/// Maximum tool-call iterations per conversation turn to prevent infinite loops.
const DEFAULT_MAX_ITERATIONS: usize = 10;

/// Minimum reply length to bother indexing (skip "ok", "sure", etc.).
const MIN_INDEX_LEN: usize = 40;

// ── MessageEntry ──────────────────────────────────────────────────────────────

/// An entry in the conversation history, extended with topic metadata.
///
/// When a topic switch occurs, messages belonging to the old topic are marked
/// `hidden = true` so they are not sent to the LLM.  They are never deleted —
/// they can be restored when the user switches back to the original topic.
#[derive(Debug, Clone)]
pub struct MessageEntry {
    pub role: String,
    pub content: String,
    /// The topic this message belongs to.
    pub topic_id: String,
    /// When `true`, this message is not included in the LLM request.
    /// Hidden messages are preserved for potential topic restoration.
    pub hidden: bool,
}

impl MessageEntry {
    pub fn new(
        role: impl Into<String>,
        content: impl Into<String>,
        topic_id: impl Into<String>,
    ) -> Self {
        Self {
            role: role.into(),
            content: content.into(),
            topic_id: topic_id.into(),
            hidden: false,
        }
    }

    /// Convert to a `ChatMessage` for sending to the LLM.
    pub fn to_chat_message(&self) -> ChatMessage {
        ChatMessage::new(self.role.clone(), self.content.clone())
    }
}

// ── AgentEngine ───────────────────────────────────────────────────────────────

/// Maximum number of messages to restore from SQLite on session resumption.
///
/// Matches `HARD_MAX_HISTORY` in `compact.rs` — no point restoring more than
/// we would keep in memory.
const SESSION_RESTORE_LIMIT: usize = 60;

pub struct AgentEngine {
    /// Optional memory backend for conversation indexing.
    memory: Option<Arc<dyn Memory>>,
    /// Session ID passed to memory operations.  Defaults to "default".
    session_id: String,
    /// Topic manager for automatic topic detection and switching.
    /// Uses interior mutability so `AgentEngine` remains usable via `&self`.
    topic_manager: adaclaw_memory::topic::TopicManager,
    /// Full conversation history across all turns (includes hidden entries).
    /// Arc<Mutex<...>> so it can be shared across async calls if needed.
    history: std::sync::Mutex<Vec<MessageEntry>>,
    /// Optional session store for durable conversation history persistence.
    ///
    /// When set:
    /// - `push_history()` asynchronously appends each message to SQLite.
    /// - `with_session_store()` pre-populates `history` from SQLite on first load.
    /// - After `rolling_compact` produces a summary, the store is compacted so
    ///   the next process restart only needs to load the summary + recent tail.
    session_store: Option<Arc<SessionStore>>,
}

impl Default for AgentEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl AgentEngine {
    pub fn new() -> Self {
        Self {
            memory: None,
            session_id: "default".to_string(),
            topic_manager: adaclaw_memory::topic::TopicManager::new(),
            history: std::sync::Mutex::new(vec![]),
            session_store: None,
        }
    }

    /// Attach a memory backend for conversation indexing.
    pub fn with_memory(mut self, memory: Arc<dyn Memory>, session_id: impl Into<String>) -> Self {
        self.memory = Some(memory);
        self.session_id = session_id.into();
        self
    }

    /// Attach a `SessionStore` for durable conversation history persistence.
    ///
    /// Must be called **after** `with_memory()` so that `self.session_id` is
    /// already set to the correct value.
    ///
    /// On attachment, existing history for this session is loaded from SQLite
    /// and pre-populated into `self.history`.  This is the "记忆续传" (memory
    /// resumption) path — a process restart will restore the conversation
    /// exactly where it left off (up to `SESSION_RESTORE_LIMIT` messages).
    ///
    /// If the most recent stored entry is a `[Conversation summary]` (written
    /// by a previous `rolling_compact` pass), it is restored as a `system`
    /// role entry, giving the LLM the same compressed context it had before
    /// the restart.
    pub fn with_session_store(mut self, store: Arc<SessionStore>) -> Self {
        match store.load_sync(&self.session_id, SESSION_RESTORE_LIMIT) {
            Ok(entries) if !entries.is_empty() => {
                let mut history = self.history.lock().unwrap();
                for entry in &entries {
                    history.push(MessageEntry {
                        role: entry.role.clone(),
                        content: entry.content.clone(),
                        topic_id: "default".to_string(),
                        hidden: false,
                    });
                }
                debug!(
                    session_id = %self.session_id,
                    restored = entries.len(),
                    "Session history restored from SQLite"
                );
            }
            Ok(_) => {
                // No prior history — new session, that's fine.
            }
            Err(e) => {
                warn!(
                    session_id = %self.session_id,
                    error = %e,
                    "Failed to restore session history from SQLite, starting fresh"
                );
            }
        }
        self.session_store = Some(store);
        self
    }

    // ── Public entry points ───────────────────────────────────────────────────

    /// Run the tool-call loop for a single user message.
    ///
    /// Automatically:
    /// 1. Detects clean intent ("不要旧记忆" etc.) → uses `RecallScope::Clean`
    /// 2. Detects topic switches via `TopicManager` → adjusts `RecallScope`
    /// 3. Prunes hidden messages from the LLM context on topic switch
    /// 4. Writes conversation to memory with the current `topic_id`
    pub async fn run_tool_loop(
        &self,
        provider: &dyn Provider,
        tools: &[Box<dyn Tool>],
        input: &str,
        model: &str,
        temp: f64,
    ) -> Result<String> {
        self.run_tool_loop_with_options(provider, tools, input, model, temp, None, None, &[], None)
            .await
    }

    /// Run the tool-call loop, streaming assistant text deltas to `deltas` as
    /// they are generated, and returning the final assembled text.
    ///
    /// Streaming uses [`Provider::chat_stream`]; providers without real token
    /// streaming fall back to a single delta (the whole message). Note: the
    /// Tier-2 reflection self-check is **skipped** when streaming (it would
    /// retroactively rewrite text already shown to the user), and forwarded
    /// deltas are raw — the final recorded message is still credential-scrubbed.
    #[allow(clippy::too_many_arguments)]
    pub async fn run_tool_loop_streaming(
        &self,
        provider: &dyn Provider,
        tools: &[Box<dyn Tool>],
        input: &str,
        model: &str,
        temp: f64,
        system: Option<&str>,
        max_iterations: Option<usize>,
        images: &[adaclaw_core::provider::ImageData],
        deltas: mpsc::Sender<String>,
    ) -> Result<String> {
        self.run_tool_loop_with_options(
            provider,
            tools,
            input,
            model,
            temp,
            system,
            max_iterations,
            images,
            Some(&deltas),
        )
        .await
    }

    /// Run the tool-call loop with full options.
    #[allow(clippy::too_many_arguments)]
    pub async fn run_tool_loop_with_options(
        &self,
        provider: &dyn Provider,
        tools: &[Box<dyn Tool>],
        input: &str,
        model: &str,
        temp: f64,
        system: Option<&str>,
        max_iterations: Option<usize>,
        images: &[adaclaw_core::provider::ImageData],
        deltas: Option<&mpsc::Sender<String>>,
    ) -> Result<String> {
        let max_iter = max_iterations.unwrap_or(DEFAULT_MAX_ITERATIONS);

        // Tracks whether any tool in this turn returned an error.
        // Used by the Tier-1 reflection heuristic to decide whether a self-check
        // is warranted after the agent produces its final response.
        let mut had_tool_error = false;

        // ── Step 1: Determine recall scope ────────────────────────────────────
        //
        // Two-tier clean intent detection:
        //   Tier 1 — Keyword fast-path (free, < 1 µs)
        //   Tier 2 — LLM intent check (only when topic drift is detected and
        //            tier 1 didn't match; typically rare, so low aggregate cost)
        let scope = if detect_clean_intent(input) {
            // Tier 1 matched — user clearly wants clean-slate thinking
            debug!(
                input,
                "Clean intent detected via keyword — using RecallScope::Clean"
            );
            RecallScope::Clean
        } else {
            // Automatic topic detection (embedding or keyword, no extra LLM call)
            let switch_result = self
                .topic_manager
                .check_and_switch(input, None)
                .await
                .unwrap_or(adaclaw_memory::topic::TopicSwitchResult::SameTopic);

            // When topic drift is detected, ask the LLM whether the user's phrasing
            // also implies "don't use prior memory" (catches tier-1 misses like
            // "就当没见过我一样帮我想想" or "pretend this is our first chat").
            let drift_with_clean_intent = match &switch_result {
                adaclaw_memory::topic::TopicSwitchResult::Switched { .. }
                | adaclaw_memory::topic::TopicSwitchResult::PartialDrift => {
                    detect_clean_intent_llm(provider, model, input).await
                }
                _ => false,
            };

            if drift_with_clean_intent {
                debug!(
                    input,
                    "Clean intent detected via LLM — using RecallScope::Clean"
                );
                RecallScope::Clean
            } else {
                if let adaclaw_memory::topic::TopicSwitchResult::Switched { ref new_topic_id } =
                    switch_result
                {
                    debug!(new_topic_id, "Topic switch detected — pruning old context");
                    self.prune_history_for_topic_switch(new_topic_id);
                }
                switch_result.to_recall_scope()
            }
        };

        let current_topic = self.topic_manager.current_topic_id();

        // ── Step 2: Build visible messages from history + new user message ────
        let mut messages: Vec<ChatMessage> = self.visible_messages();
        // Attach images to the user turn (caller is responsible for only passing
        // images to vision-capable providers). Images live only in this turn's
        // in-memory context — they are not persisted to durable history below.
        let user_msg = if images.is_empty() {
            ChatMessage::new("user", input)
        } else {
            ChatMessage::user_with_images(input, images.to_vec())
        };
        messages.push(user_msg);

        // Add user message to persistent history (text only — images are
        // per-turn context, never written to the durable store).
        self.push_history(MessageEntry::new("user", input, &current_topic));

        // Tool specs for native tool calling. Providers that advertise native
        // support receive these as the API `tools` array; providers that don't
        // ignore them (the default `chat_with_tools` falls back to `chat`) and
        // the engine drives tools via text parsing instead. The catalog is also
        // omitted from the system prompt for native providers (see
        // `AgentInstance::build_system_prompt`) to avoid double-advertising.
        let tool_specs: Vec<adaclaw_core::tool::ToolSpec> =
            tools.iter().map(|t| t.spec()).collect();

        // Prompt tokens reported by the previous LLM response, used to drive
        // token-aware compaction (more accurate than a message-count heuristic
        // when a few messages are very large). `None` until the first response,
        // or for providers that don't report usage.
        let mut last_prompt_tokens: Option<u32> = None;

        for iteration in 0..max_iter {
            // Auto-compact history: rolling LLM summarisation when above threshold
            // (ROLLING_THRESHOLD=30), then hard-trim as safety net (HARD_MAX=60).
            // Falls back to hard-trim gracefully if the LLM summary call fails.
            // Reference: picoclaw maybeSummarize + zeroclaw auto_compact_history.
            //
            // ── Compaction → SessionStore sync ─────────────────────────────────
            // Detect whether a NEW summary is produced by rolling_compact so we
            // can persist it to SQLite.  The marker is `messages[1]` starting with
            // `[Conversation summary]`.  We snapshot the state *before* to tell
            // apart a freshly-written summary from one that was already there.
            let had_summary_before = messages
                .get(1)
                .is_some_and(|m| m.content.starts_with("[Conversation summary]"));

            if let Err(e) = crate::agents::compact::auto_compact_history(
                &mut messages,
                provider,
                model,
                last_prompt_tokens,
            )
            .await
            {
                warn!(error = %e, "auto_compact_history failed, applying hard trim");
                crate::agents::compact::trim_history(&mut messages);
            }

            // If a new summary was just inserted, persist it to the SessionStore so
            // the next process restart can restore the compressed context directly.
            let has_new_summary = !had_summary_before
                && messages
                    .get(1)
                    .is_some_and(|m| m.content.starts_with("[Conversation summary]"));

            if has_new_summary && let Some(store) = &self.session_store {
                let summary = messages[1].content.clone();
                let store = Arc::clone(store);
                let session_id = self.session_id.clone();
                tokio::spawn(async move {
                    if let Err(e) = store.compact(&session_id, &summary).await {
                        warn!(
                            session_id = %session_id,
                            error = %e,
                            "Failed to persist compaction summary to SessionStore"
                        );
                    } else {
                        debug!(
                            session_id = %session_id,
                            "Compaction summary persisted to SessionStore"
                        );
                    }
                });
            }

            // Call LLM with retry on context-window errors (max 2 retries).
            // On each retry, force_compress_messages() drops the oldest 50% of the
            // conversation to recover space — matching picoclaw's forceCompression.
            let mut context_retry = 0u8;
            let response = loop {
                let req = ChatRequest {
                    messages: &messages,
                    system,
                };
                match stream_or_call(provider, req, &tool_specs, model, temp, deltas).await {
                    Ok(resp) => break Ok(resp),
                    Err(e) if detect_context_window_error(&e) && context_retry < 2 => {
                        warn!(
                            error = %e,
                            retry = context_retry + 1,
                            "Context window error detected — force-compressing history and retrying"
                        );
                        force_compress_messages(&mut messages);
                        context_retry += 1;
                    }
                    Err(e) => break Err(e),
                }
            }?;

            // Record token usage (if the provider reported it) to drive the next
            // iteration's token-aware compaction.
            last_prompt_tokens = response.usage.map(|u| u.prompt_tokens);
            debug!(
                iteration,
                response_len = response.content.len(),
                prompt_tokens = last_prompt_tokens.unwrap_or(0),
                completion_tokens = response.usage.map(|u| u.completion_tokens).unwrap_or(0),
                "Agent got LLM response"
            );

            // ── Record the assistant turn and collect tool calls ──────────────
            // Native path: the provider returned structured `tool_calls`. Record
            // the assistant turn *with* those calls so the follow-up request
            // round-trips correctly (OpenAI/Anthropic require the original calls
            // alongside their results), then run them.
            // Text path: no native calls — record plain content and parse the
            // text for `<tool_call>` blocks (works with non-native providers).
            let pending: Vec<PendingCall> = if !response.tool_calls.is_empty() {
                messages.push(ChatMessage::assistant_tool_calls(
                    response.content.clone(),
                    response.tool_calls.clone(),
                ));
                response
                    .tool_calls
                    .iter()
                    .map(|c| PendingCall {
                        id: Some(c.id.clone()),
                        name: c.name.clone(),
                        args: c.arguments.clone(),
                    })
                    .collect()
            } else {
                messages.push(ChatMessage::new("assistant", response.content.clone()));
                collect_text_tool_calls(&response.content)?
            };

            if pending.is_empty() {
                // No more tool calls — turn complete
                let scrubbed = adaclaw_security::scrub::scrub_credentials(&response.content);

                // ── Tiered Reflection ─────────────────────────────────────────
                //
                // Tier 1 (zero tokens): heuristic check — triggers when:
                //   a) ≥1 tool returned an error AND the response doesn't mention it
                //   b) The agent used ≥3 iterations (complex multi-step task)
                //   c) User explicitly asked to verify/confirm the result
                //
                // Tier 2 (one LLM yes/no + optional fix): only runs when Tier 1
                //   triggers. Bounded cost — at most 2 extra LLM calls per turn,
                //   and only for complex or error-laden tasks.
                //
                // 95%+ of ordinary single-step conversations pay zero extra cost.
                // Reflection is skipped while streaming: it would retroactively
                // rewrite text the user has already seen streamed.
                let final_response = if deltas.is_none()
                    && needs_reflection_tier1(input, &scrubbed, had_tool_error, iteration)
                {
                    debug!(
                        iteration,
                        had_tool_error, "Tier 1 reflection triggered — running LLM self-check"
                    );
                    tiered_reflect(provider, model, input, scrubbed, &messages).await
                } else {
                    scrubbed
                };

                // Add assistant reply to persistent history
                self.push_history(MessageEntry::new(
                    "assistant",
                    &final_response,
                    &current_topic,
                ));

                // Index this conversation turn into memory
                self.index_conversation(input, &final_response, &current_topic, &scope)
                    .await;

                return Ok(final_response);
            }

            // ── Execute the pending tool calls ─────────────────────────────────
            // Known tools run concurrently; an unknown tool name yields an error
            // result rather than aborting the turn. Results are stitched back in
            // `pending` order so each native call's result references its id.
            let mut futures = Vec::new();
            let mut slots: Vec<Option<(String, String)>> = vec![None; pending.len()];
            for (idx, pc) in pending.iter().enumerate() {
                match tools.iter().find(|t| t.name() == pc.name) {
                    Some(tool) => {
                        let name = pc.name.clone();
                        let fut = tool.execute(pc.args.clone());
                        futures.push(async move { (idx, name, fut.await) });
                    }
                    None => {
                        warn!("Unknown tool '{}' requested", pc.name);
                        had_tool_error = true;
                        slots[idx] = Some((
                            pc.name.clone(),
                            format!("Error: tool '{}' not found", pc.name),
                        ));
                    }
                }
            }

            for (idx, name, result) in join_all(futures).await {
                let (content, success) = match result {
                    Ok(res) => {
                        if res.success {
                            (res.output, true)
                        } else {
                            (
                                format!(
                                    "Error: {}",
                                    res.error.unwrap_or_else(|| "unknown error".to_string())
                                ),
                                false,
                            )
                        }
                    }
                    Err(e) => (format!("Error executing tool: {}", e), false),
                };
                if !success {
                    had_tool_error = true;
                }
                debug!(tool = %name, success, "Tool call completed");
                slots[idx] = Some((name, content));
            }

            // Push results in call order, using the native `tool_result` shape
            // (linked by id) for native calls or the text `[name]: …` shape for
            // text-parsed calls.
            for (pc, slot) in pending.iter().zip(slots.into_iter()) {
                if let Some((name, content)) = slot {
                    match &pc.id {
                        Some(id) => messages.push(ChatMessage::tool_result(id, content)),
                        None => messages
                            .push(ChatMessage::new("tool", format!("[{}]: {}", name, content))),
                    }
                }
            }
        }

        Err(anyhow!(
            "Exceeded maximum tool call iterations ({}). Possible infinite loop.",
            max_iter
        ))
    }

    // ── History management ────────────────────────────────────────────────────

    /// Append a message to the in-memory history and, if a `SessionStore` is
    /// attached, asynchronously persist it to SQLite (fire-and-forget).
    ///
    /// The write is performed via `tokio::spawn` so it never blocks the LLM
    /// call loop.  In the rare event that the spawn fails (e.g. Tokio runtime
    /// already shutting down), the message is still in memory for this run.
    fn push_history(&self, entry: MessageEntry) {
        self.history.lock().unwrap().push(entry.clone());

        // Persist to SessionStore asynchronously — same pattern as
        // `StateManager::update_last_active()` (fire-and-forget, WAL-safe).
        if let Some(store) = &self.session_store {
            let store = Arc::clone(store);
            let session_id = self.session_id.clone();
            let role = entry.role.clone();
            let content = entry.content.clone();
            tokio::spawn(async move {
                if let Err(e) = store.append(&session_id, &role, &content).await {
                    warn!(
                        session_id = %session_id,
                        role = %role,
                        error = %e,
                        "Failed to persist message to SessionStore"
                    );
                }
            });
        }
    }

    /// Get all non-hidden messages as `ChatMessage` for LLM consumption.
    fn visible_messages(&self) -> Vec<ChatMessage> {
        self.history
            .lock()
            .unwrap()
            .iter()
            .filter(|m| !m.hidden)
            .map(|m| m.to_chat_message())
            .collect()
    }

    /// When switching topics, hide messages that belong to the old topic.
    /// Does NOT delete them — they can be restored when switching back.
    fn prune_history_for_topic_switch(&self, new_topic_id: &str) {
        let mut history = self.history.lock().unwrap();
        for entry in history.iter_mut() {
            if entry.topic_id != new_topic_id {
                entry.hidden = true;
            }
        }
    }

    // ── Conversation indexing ─────────────────────────────────────────────────

    /// Store a brief index of this conversation turn into memory.
    ///
    /// When scope is `Clean`, we still write to memory (the user asked for
    /// clean *thinking*, not clean *recording*).
    async fn index_conversation(
        &self,
        user_input: &str,
        assistant_reply: &str,
        topic_id: &str,
        _scope: &RecallScope,
    ) {
        let memory = match self.memory.as_ref() {
            Some(m) => m,
            None => return,
        };

        if assistant_reply.len() < MIN_INDEX_LEN {
            return;
        }

        let input_snippet = truncate(user_input, 300);
        let reply_snippet = truncate(assistant_reply, 500);
        let content = format!("User: {}\nAssistant: {}", input_snippet, reply_snippet);

        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);

        let key = format!("conv:{}:{}", self.session_id, ts);

        match memory
            .store(
                &key,
                &content,
                Category::Conversation,
                Some(&self.session_id),
                Some(topic_id),
            )
            .await
        {
            Err(e) => {
                warn!(key, error = %e, "Failed to index conversation turn");
            }
            _ => {
                debug!(key, topic_id, "Conversation turn indexed");
            }
        }
    }
}

// ── Clean intent detection ────────────────────────────────────────────────────

/// Returns `true` if the user's message contains a natural-language expression
/// meaning "think without prior history / don't use old memory".
///
/// Detection is purely lexical — no LLM call required.  Silent: no user-facing
/// message is emitted when this triggers.
pub fn detect_clean_intent(message: &str) -> bool {
    let lower = message.to_lowercase();

    let patterns: &[&str] = &[
        // Chinese — common natural expressions
        "不要旧记忆",
        "不要之前的记忆",
        "忘掉之前",
        "忘记之前",
        "不要被之前",
        "不要用之前",
        "不用之前",
        "不要之前的上下文",
        "不要用旧",
        "清空背景",
        "全新思考",
        "不受之前影响",
        "当作第一次",
        "就当没聊过",
        "不用管之前",
        "不要历史记录",
        "清除上下文",
        "清空记忆",
        // English — natural expressions
        "forget previous",
        "ignore history",
        "ignore previous",
        "clean slate",
        "fresh start",
        "no memory",
        "without context",
        "without history",
        "without prior",
        "don't use previous",
        "don't use history",
        "don't use old",
        "as if we never",
        "as if it's the first",
        // Explicit command (least common, but should still work)
        "/fresh",
    ];

    patterns.iter().any(|p| lower.contains(p))
}

/// LLM-based clean intent detector — **Tier 2 fallback**.
///
/// Called only when the keyword fast-path (`detect_clean_intent`) returns
/// `false` **and** the `TopicManager` has detected topic drift.  This catches
/// natural expressions that no finite keyword list can cover, e.g.:
///
/// - "就当没见过我一样帮我想想"
/// - "pretend this is our first conversation"
/// - "抛开所有背景，从头分析这个问题"
///
/// Uses a **single yes/no LLM call** at temperature 0.0 (deterministic).
/// If the call fails for any reason, returns `false` (safe default = no clean
/// scope, avoids over-silencing memory).
async fn detect_clean_intent_llm(provider: &dyn Provider, model: &str, message: &str) -> bool {
    let system = "You are a binary classifier. \
                  Answer ONLY 'YES' or 'NO', nothing else. \
                  Determine whether the user's message implies they want the AI to \
                  respond WITHOUT referencing any prior conversation history or memory. \
                  Examples of YES: \
                  '就当没聊过，帮我想想', \
                  'pretend this is our first chat', \
                  '抛开所有背景，从零分析'. \
                  Examples of NO: \
                  '帮我看看这段代码', \
                  'what is 2+2', \
                  'write a poem about autumn'.";

    let prompt = format!(
        "Does this message imply the user wants a response WITHOUT prior history? Message: \"{}\"",
        message
    );

    match provider
        .chat_with_system(Some(system), &prompt, model, 0.0)
        .await
    {
        Ok(reply) => {
            let r = reply.trim().to_uppercase();
            let result = r.starts_with("YES");
            if result {
                tracing::debug!(message, "LLM classified clean intent as YES");
            }
            result
        }
        Err(e) => {
            // Non-fatal: if LLM call fails, default to no clean intent
            tracing::debug!(error = %e, "detect_clean_intent_llm failed, defaulting to false");
            false
        }
    }
}

// ── helpers ───────────────────────────────────────────────────────────────────

/// Truncate `s` to at most `max_bytes` **bytes**, returning a valid UTF-8 slice.
///
/// Note: the limit is in bytes, not Unicode scalar values (this is intentional —
/// the callers use conservative byte limits, and for typical LLM snippet indexing
/// (max 300–500 bytes) the difference is rarely significant in practice).
/// The function always returns a valid UTF-8 slice even when the byte limit falls
/// inside a multi-byte codepoint (it walks back to the nearest char boundary).
fn truncate(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    let mut idx = max_bytes;
    while !s.is_char_boundary(idx) {
        idx -= 1;
    }
    &s[..idx]
}

/// Detect whether an LLM error is due to context-window / token-limit overflow.
///
/// Covers error messages from OpenAI, Anthropic, Ollama, Groq, DeepSeek, and
/// other OpenAI-compatible endpoints.  The check is case-insensitive so it
/// handles variations in capitalisation across providers.
///
/// Reference: picoclaw `runLLMIteration` isContextError check (loop.go).
pub(crate) fn detect_context_window_error(err: &anyhow::Error) -> bool {
    let msg = err.to_string().to_lowercase();
    // OpenAI / compatible
    msg.contains("context_length_exceeded")
        || msg.contains("context window")
        || msg.contains("maximum context")
        || msg.contains("request_too_large")
        || msg.contains("request too large")
        // Anthropic
        || msg.contains("input is too long")
        || msg.contains("prompt is too long")
        // Generic token wording
        || (msg.contains("token") && (msg.contains("exceed") || msg.contains("limit")))
        || (msg.contains("context") && msg.contains("length"))
        // Zhipu/GLM ("InvalidParameter: Total tokens … exceed max")
        || msg.contains("invalidparameter")
        // Ollama
        || msg.contains("context length")
        // Groq
        || msg.contains("exceeds the model's context length")
}

/// Emergency compression: drop the **oldest 50%** of non-system conversation
/// messages.  Designed for use when the LLM returns a context-window error and
/// `auto_compact_history` (which requires a working LLM) has already been
/// tried.  This is a deterministic, zero-LLM-call fallback.
///
/// - Preserves `messages[0]` if it is the system prompt (role == "system").
/// - Preserves the most recent message (the current user turn or last tool result).
/// - Drops the **oldest half** of the middle conversation.
///
/// Reference: picoclaw `forceCompression` in loop.go.
pub(crate) fn force_compress_messages(messages: &mut Vec<ChatMessage>) {
    if messages.len() <= 4 {
        return;
    }

    let has_system = messages
        .first()
        .map(|m| m.role == "system")
        .unwrap_or(false);
    let conv_start = if has_system { 1 } else { 0 };

    // Conversation slice = everything except the system prompt and the last message.
    let conv_len = messages.len().saturating_sub(conv_start + 1);
    if conv_len == 0 {
        return;
    }

    let drop_count = conv_len / 2;
    if drop_count == 0 {
        return;
    }

    // Drop the oldest half of the conversation window.
    messages.drain(conv_start..conv_start + drop_count);

    debug!(
        dropped = drop_count,
        remaining = messages.len(),
        "force_compress_messages: dropped oldest half of conversation to recover context space"
    );
}

// ── Tiered Reflection ─────────────────────────────────────────────────────────

/// Tier-1 heuristic: decide whether the LLM self-check (Tier 2) is warranted.
///
/// This is a **zero-token** check — no LLM call is made.  It returns `true`
/// when at least one of the following conditions holds:
///
/// 1. **Unacknowledged tool error** — ≥1 tool returned an error AND the
///    agent's response does not contain any failure-acknowledgement signal.
///    Catches the common case where the model says "done!" despite a tool
///    failure it quietly ignored.
///
/// 2. **High iteration count** — the agent went through ≥3 tool-call
///    iterations, indicating a complex multi-step task where completeness
///    is harder to guarantee.
///
/// 3. **Explicit verification keywords** — the user's message contains
///    words like "confirm", "verify", "确认", "检查" etc., signalling that
///    correctness is especially important.
///
/// When this returns `false` (the common case for simple requests), the
/// agent response is returned immediately with zero additional cost.
pub(crate) fn needs_reflection_tier1(
    user_input: &str,
    response: &str,
    had_tool_error: bool,
    iterations_used: usize,
) -> bool {
    // Condition 1: Tool error present but response doesn't mention it
    if had_tool_error {
        let lower_resp = response.to_lowercase();
        let acknowledges_failure = lower_resp.contains("error")
            || lower_resp.contains("fail")
            || lower_resp.contains("错误")
            || lower_resp.contains("失败")
            || lower_resp.contains("unable")
            || lower_resp.contains("cannot")
            || lower_resp.contains("无法");
        if !acknowledges_failure {
            return true;
        }
    }

    // Condition 2: Complex task (many iterations)
    // iterations_used is the 0-based loop counter; value of 2 means the
    // 3rd iteration has completed (iterations 0, 1, 2).
    if iterations_used >= 2 {
        return true;
    }

    // Condition 3: User explicitly asked for verification
    let lower_input = user_input.to_lowercase();
    let verification_keywords: &[&str] = &[
        // Chinese
        "确认",
        "检查",
        "验证",
        "核实",
        "核查",
        "检验",
        "是否完成",
        "是否成功",
        "完成了吗",
        "有没有问题",
        // English
        "verify",
        "confirm",
        "make sure",
        "double check",
        "double-check",
        "check if",
        "check that",
        "ensure",
        "validate",
        "make certain",
    ];
    if verification_keywords
        .iter()
        .any(|kw| lower_input.contains(kw))
    {
        return true;
    }

    false
}

/// Tier-2 LLM self-check: ask the model whether its own response is complete,
/// and optionally request a corrective follow-up pass.
///
/// ## Cost model
///
/// | Step | When | Approx tokens |
/// |------|------|--------------|
/// | Yes/No completeness check | Always (Tier 1 triggered) | ~200–400 |
/// | Corrective pass | Only when model says "NO" | ~400–1000 |
///
/// Maximum additional cost per turn: ~2 LLM calls, ~1 400 extra tokens.
/// This is bounded and only incurred for genuinely complex / error-prone tasks.
///
/// ## Fallback
///
/// If either LLM call fails, the original `candidate_response` is returned
/// unchanged — the reflection system is entirely non-blocking.
async fn tiered_reflect(
    provider: &dyn Provider,
    model: &str,
    user_input: &str,
    candidate_response: String,
    messages: &[ChatMessage],
) -> String {
    // ── Step 1: Yes/No completeness check ─────────────────────────────────────
    let check_system = "You are a quality-checker for an AI assistant. \
                        Answer ONLY 'YES' or 'NO'. \
                        YES = the assistant's response fully and correctly addressed \
                        the user's request. \
                        NO  = the response is incomplete, incorrect, or missed part \
                        of the request.";

    let check_prompt = format!(
        "User request: \"{}\"\n\nAssistant response:\n{}\n\nWas the task fully completed?",
        // Clip snippets to control cost
        user_input.chars().take(400).collect::<String>(),
        candidate_response.chars().take(600).collect::<String>(),
    );

    let is_complete = match provider
        .chat_with_system(Some(check_system), &check_prompt, model, 0.0)
        .await
    {
        Ok(reply) => {
            let upper = reply.trim().to_uppercase();
            debug!(
                "Reflection Tier-2 self-check result: {}",
                &upper[..upper.len().min(10)]
            );
            upper.starts_with("YES")
        }
        Err(e) => {
            // Non-fatal: LLM unavailable → keep original response
            debug!(error = %e, "Reflection Tier-2 check failed — keeping original response");
            return candidate_response;
        }
    };

    if is_complete {
        debug!("Reflection Tier-2: response is complete, no correction needed");
        return candidate_response;
    }

    // ── Step 2: Corrective pass (only when model says "NO") ───────────────────
    debug!("Reflection Tier-2: response flagged as incomplete — requesting correction");

    // Build a correction prompt using the existing message history for context.
    // We append a meta-instruction rather than replacing the last assistant message,
    // so the model sees the full picture of what it already tried.
    let fix_prompt = format!(
        "Your previous response to the user's request may be incomplete or incorrect.\n\n\
         User request: \"{}\"\n\n\
         Your previous response:\n{}\n\n\
         Please provide a complete and correct response, addressing any gaps or errors.",
        user_input.chars().take(300).collect::<String>(),
        candidate_response.chars().take(500).collect::<String>(),
    );

    let mut fix_messages = messages.to_vec();
    fix_messages.push(ChatMessage::new("user", fix_prompt));

    let fix_req = ChatRequest {
        messages: &fix_messages,
        system: None,
    };

    match provider.chat(fix_req, model, 0.3).await {
        Ok(resp) => {
            let corrected = adaclaw_security::scrub::scrub_credentials(&resp.content);
            debug!(
                original_len = candidate_response.len(),
                corrected_len = corrected.len(),
                "Reflection Tier-2: correction obtained"
            );
            corrected
        }
        Err(e) => {
            debug!(error = %e, "Reflection Tier-2 correction failed — keeping original response");
            candidate_response
        }
    }
}

// ── Tool-call normalization ─────────────────────────────────────────────────

/// A tool call to execute this iteration, normalized from either the native
/// `tool_calls` response field or text-parsed `<tool_call>` blocks.
struct PendingCall {
    /// Native call id, used to link the `tool_result` back to its call. `None`
    /// for text-parsed calls (the text path uses `[name]: …` instead).
    id: Option<String>,
    name: String,
    args: serde_json::Value,
}

/// Run one LLM call, optionally streaming text deltas to `deltas`.
///
/// - `deltas == None`  → a normal [`Provider::chat_with_tools`] call.
/// - `deltas == Some`  → a [`Provider::chat_stream`] call; each `Delta` is
///   forwarded to the sink (best-effort — a dropped receiver just stops
///   forwarding) and the terminal `Done` chunk's [`ChatResponse`] is returned.
///
/// Either way the return value is the same `ChatResponse` the tool loop needs,
/// so the rest of the loop is identical for streaming and non-streaming.
async fn stream_or_call(
    provider: &dyn Provider,
    req: ChatRequest<'_>,
    tools: &[adaclaw_core::tool::ToolSpec],
    model: &str,
    temp: f64,
    deltas: Option<&mpsc::Sender<String>>,
) -> Result<ChatResponse> {
    use adaclaw_core::provider::StreamChunk;
    use futures_util::StreamExt;

    let sink = match deltas {
        None => return provider.chat_with_tools(req, tools, model, temp).await,
        Some(s) => s,
    };

    let mut stream = provider.chat_stream(req, tools, model, temp).await?;
    let mut final_resp: Option<ChatResponse> = None;
    while let Some(item) = stream.next().await {
        match item? {
            StreamChunk::Delta(text) => {
                // Best-effort: if the consumer hung up, keep draining so the
                // provider call still completes and history stays consistent.
                let _ = sink.send(text).await;
            }
            StreamChunk::Done(resp) => final_resp = Some(resp),
        }
    }
    final_resp.ok_or_else(|| anyhow!("stream ended without a Done chunk"))
}

/// Parse `<tool_call>` blocks out of assistant text and normalize them,
/// de-duplicating exact-duplicate calls (some models repeat a call in prose).
fn collect_text_tool_calls(content: &str) -> Result<Vec<PendingCall>> {
    let parsed = crate::agents::parser::ToolCallParser::parse(content)?;
    let mut dedup = HashSet::<String>::new();
    let mut out = Vec::new();
    for call in &parsed {
        if !dedup.insert(call.to_string()) {
            warn!("Duplicate tool call skipped: {}", call["name"]);
            continue;
        }
        match call.get("name").and_then(|n| n.as_str()) {
            Some(name) => out.push(PendingCall {
                id: None,
                name: name.to_string(),
                args: call
                    .get("arguments")
                    .cloned()
                    .unwrap_or_else(|| serde_json::Value::Object(Default::default())),
            }),
            None => warn!("Tool call missing 'name' field, skipping"),
        }
    }
    Ok(out)
}

// ── unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_collect_text_tool_calls_parses_and_dedups() {
        let content = "<tool_call>{\"name\":\"shell\",\"arguments\":{\"command\":\"ls\"}}</tool_call>\n\
                       <tool_call>{\"name\":\"shell\",\"arguments\":{\"command\":\"ls\"}}</tool_call>";
        let calls = collect_text_tool_calls(content).unwrap();
        assert_eq!(calls.len(), 1, "exact-duplicate text calls are deduped");
        assert_eq!(calls[0].name, "shell");
        assert_eq!(calls[0].args["command"], "ls");
        assert!(
            calls[0].id.is_none(),
            "text-parsed calls carry no native id"
        );
    }

    #[test]
    fn test_collect_text_tool_calls_empty_for_plain_text() {
        let calls = collect_text_tool_calls("just a normal answer, no tools").unwrap();
        assert!(calls.is_empty());
    }

    // ── native tool-calling end-to-end ────────────────────────────────────────

    /// A provider that, on its first `chat_with_tools` call, asks to run the
    /// `echo` tool, and on the second call asserts that the assistant tool-call
    /// turn and its `tool_result` were faithfully sent back, returning "done"
    /// only if the round-trip is intact.
    struct ScriptedProvider {
        calls: std::sync::atomic::AtomicUsize,
    }

    #[async_trait::async_trait]
    impl Provider for ScriptedProvider {
        fn name(&self) -> &str {
            "scripted"
        }
        fn capabilities(&self) -> adaclaw_core::provider::ProviderCapabilities {
            adaclaw_core::provider::ProviderCapabilities {
                native_tool_calling: true,
                vision: false,
                streaming: false,
            }
        }
        async fn chat(
            &self,
            _req: ChatRequest<'_>,
            _model: &str,
            _temp: f64,
        ) -> Result<adaclaw_core::provider::ChatResponse> {
            Ok(adaclaw_core::provider::ChatResponse::default())
        }
        async fn chat_with_tools(
            &self,
            req: ChatRequest<'_>,
            _tools: &[adaclaw_core::tool::ToolSpec],
            _model: &str,
            _temp: f64,
        ) -> Result<adaclaw_core::provider::ChatResponse> {
            use std::sync::atomic::Ordering;
            let n = self.calls.fetch_add(1, Ordering::SeqCst);
            if n == 0 {
                Ok(adaclaw_core::provider::ChatResponse {
                    tool_calls: vec![adaclaw_core::provider::ToolCall {
                        id: "call_1".to_string(),
                        name: "echo".to_string(),
                        arguments: serde_json::json!({"text": "hi"}),
                    }],
                    ..Default::default()
                })
            } else {
                let saw_assistant_call = req
                    .messages
                    .iter()
                    .any(|m| m.tool_calls.iter().any(|c| c.id == "call_1"));
                let saw_result = req.messages.iter().any(|m| {
                    m.tool_call_id.as_deref() == Some("call_1") && m.content.contains("echoed:hi")
                });
                Ok(adaclaw_core::provider::ChatResponse {
                    content: if saw_assistant_call && saw_result {
                        "done".to_string()
                    } else {
                        "MISSING_ROUNDTRIP".to_string()
                    },
                    ..Default::default()
                })
            }
        }
        async fn chat_with_system(
            &self,
            _s: Option<&str>,
            _m: &str,
            _mo: &str,
            _t: f64,
        ) -> Result<String> {
            Ok("NO".to_string())
        }
    }

    struct EchoTool;

    #[async_trait::async_trait]
    impl Tool for EchoTool {
        fn name(&self) -> &str {
            "echo"
        }
        fn description(&self) -> &str {
            "echo text"
        }
        fn parameters_schema(&self) -> serde_json::Value {
            serde_json::json!({"type": "object", "properties": {"text": {"type": "string"}}})
        }
        fn spec(&self) -> adaclaw_core::tool::ToolSpec {
            adaclaw_core::tool::ToolSpec {
                name: "echo".to_string(),
                description: "echo text".to_string(),
                parameters: self.parameters_schema(),
            }
        }
        async fn execute(&self, args: serde_json::Value) -> Result<adaclaw_core::tool::ToolResult> {
            Ok(adaclaw_core::tool::ToolResult {
                success: true,
                output: format!("echoed:{}", args["text"].as_str().unwrap_or("")),
                error: None,
            })
        }
    }

    #[tokio::test]
    async fn test_native_tool_loop_roundtrips_tool_result() {
        let engine = AgentEngine::new();
        let provider = ScriptedProvider {
            calls: std::sync::atomic::AtomicUsize::new(0),
        };
        let tools: Vec<Box<dyn Tool>> = vec![Box::new(EchoTool)];
        let out = engine
            .run_tool_loop(&provider, &tools, "please echo hi", "test-model", 0.0)
            .await
            .unwrap();
        assert_eq!(
            out, "done",
            "native tool_call + tool_result must round-trip back to the provider"
        );
    }

    // ── streaming ─────────────────────────────────────────────────────────────

    /// A provider whose `chat_stream` emits several text deltas then a Done.
    struct StreamingProvider;

    #[async_trait::async_trait]
    impl Provider for StreamingProvider {
        fn name(&self) -> &str {
            "streaming"
        }
        fn capabilities(&self) -> adaclaw_core::provider::ProviderCapabilities {
            adaclaw_core::provider::ProviderCapabilities {
                native_tool_calling: false,
                vision: false,
                streaming: true,
            }
        }
        async fn chat(
            &self,
            _req: ChatRequest<'_>,
            _model: &str,
            _temp: f64,
        ) -> Result<ChatResponse> {
            Ok(ChatResponse {
                content: "Hello, world".to_string(),
                ..Default::default()
            })
        }
        async fn chat_stream(
            &self,
            _req: ChatRequest<'_>,
            _tools: &[adaclaw_core::tool::ToolSpec],
            _model: &str,
            _temp: f64,
        ) -> Result<adaclaw_core::provider::ChatStream> {
            use adaclaw_core::provider::StreamChunk;
            let chunks: Vec<Result<StreamChunk>> = vec![
                Ok(StreamChunk::Delta("Hello".to_string())),
                Ok(StreamChunk::Delta(", ".to_string())),
                Ok(StreamChunk::Delta("world".to_string())),
                Ok(StreamChunk::Done(ChatResponse {
                    content: "Hello, world".to_string(),
                    ..Default::default()
                })),
            ];
            Ok(Box::pin(futures_util::stream::iter(chunks)))
        }
        async fn chat_with_system(
            &self,
            _s: Option<&str>,
            _m: &str,
            _mo: &str,
            _t: f64,
        ) -> Result<String> {
            Ok(String::new())
        }
    }

    #[tokio::test]
    async fn test_streaming_forwards_deltas_in_order() {
        let engine = AgentEngine::new();
        let provider = StreamingProvider;
        let tools: Vec<Box<dyn Tool>> = vec![];
        // Buffer larger than the number of deltas so the engine never blocks on
        // send() without a concurrent reader (we drain after it returns).
        let (tx, mut rx) = mpsc::channel::<String>(64);
        let out = engine
            .run_tool_loop_streaming(&provider, &tools, "hi", "m", 0.0, None, None, &[], tx)
            .await
            .unwrap();

        let mut got = Vec::new();
        while let Ok(d) = rx.try_recv() {
            got.push(d);
        }
        assert_eq!(
            got,
            vec!["Hello", ", ", "world"],
            "deltas forwarded in order"
        );
        assert_eq!(
            out, "Hello, world",
            "final text equals the concatenated deltas"
        );
    }

    #[test]
    fn test_detect_clean_intent_chinese() {
        assert!(detect_clean_intent("不要旧记忆，帮我想一首诗"));
        assert!(detect_clean_intent("忘掉之前的话，全新思考这个问题"));
        assert!(detect_clean_intent("清空背景，你觉得这个设计怎么样"));
    }

    #[test]
    fn test_detect_clean_intent_english() {
        assert!(detect_clean_intent("fresh start, help me with this"));
        assert!(detect_clean_intent("ignore history and answer this"));
        assert!(detect_clean_intent("/fresh help me write a poem"));
    }

    #[test]
    fn test_detect_clean_intent_false_positive_guard() {
        // Normal messages should NOT trigger clean intent
        assert!(!detect_clean_intent("帮我看看这段代码"));
        assert!(!detect_clean_intent("what is the weather today?"));
        assert!(!detect_clean_intent("write a poem about autumn"));
    }

    #[test]
    fn test_message_entry_hidden_default() {
        let entry = MessageEntry::new("user", "hello", "topic-1");
        assert!(!entry.hidden);
        assert_eq!(entry.topic_id, "topic-1");
    }

    #[test]
    fn test_prune_history_for_topic_switch() {
        let engine = AgentEngine::new();
        engine.push_history(MessageEntry::new("user", "rust question", "topic-rust"));
        engine.push_history(MessageEntry::new("assistant", "rust answer", "topic-rust"));
        engine.push_history(MessageEntry::new("user", "new topic", "topic-poem"));

        engine.prune_history_for_topic_switch("topic-poem");

        let history = engine.history.lock().unwrap();
        assert!(history[0].hidden, "old topic messages should be hidden");
        assert!(history[1].hidden, "old topic messages should be hidden");
        assert!(
            !history[2].hidden,
            "current topic message should be visible"
        );
    }

    #[test]
    fn test_visible_messages_excludes_hidden() {
        let engine = AgentEngine::new();
        let mut entry1 = MessageEntry::new("user", "hidden msg", "old-topic");
        entry1.hidden = true;
        engine.push_history(entry1);
        engine.push_history(MessageEntry::new("user", "visible msg", "new-topic"));

        let visible = engine.visible_messages();
        assert_eq!(visible.len(), 1);
        assert_eq!(visible[0].content, "visible msg");
    }

    // ── detect_context_window_error tests ─────────────────────────────────────

    #[test]
    fn test_detect_context_window_error_openai_patterns() {
        // OpenAI canonical code
        assert!(detect_context_window_error(&anyhow::anyhow!(
            "context_length_exceeded: your prompt is too long"
        )));
        // Common plain-English variant
        assert!(detect_context_window_error(&anyhow::anyhow!(
            "This model's context window is 4096 tokens"
        )));
        // Maximum context phrasing
        assert!(detect_context_window_error(&anyhow::anyhow!(
            "maximum context length exceeded"
        )));
        // Request-too-large code
        assert!(detect_context_window_error(&anyhow::anyhow!(
            "request_too_large"
        )));
        assert!(detect_context_window_error(&anyhow::anyhow!(
            "request too large for model"
        )));
    }

    #[test]
    fn test_detect_context_window_error_anthropic_patterns() {
        assert!(detect_context_window_error(&anyhow::anyhow!(
            "input is too long for this model"
        )));
        assert!(detect_context_window_error(&anyhow::anyhow!(
            "prompt is too long for claude"
        )));
    }

    #[test]
    fn test_detect_context_window_error_generic_token_patterns() {
        assert!(detect_context_window_error(&anyhow::anyhow!(
            "token limit exceeded in this request"
        )));
        assert!(detect_context_window_error(&anyhow::anyhow!(
            "1234 tokens exceed the context length"
        )));
        assert!(detect_context_window_error(&anyhow::anyhow!(
            "context length 4096 exceeded"
        )));
        // Groq-style
        assert!(detect_context_window_error(&anyhow::anyhow!(
            "request exceeds the model's context length"
        )));
    }

    #[test]
    fn test_detect_context_window_error_zhipu_pattern() {
        // GLM / Zhipu error format from picoclaw test suite
        assert!(detect_context_window_error(&anyhow::anyhow!(
            "InvalidParameter: Total tokens of image and text exceed max message tokens"
        )));
    }

    #[test]
    fn test_detect_context_window_error_false_positives() {
        // Normal errors must NOT be mistaken for context errors
        assert!(!detect_context_window_error(&anyhow::anyhow!(
            "Authentication failed: invalid API key"
        )));
        assert!(!detect_context_window_error(&anyhow::anyhow!(
            "rate limit exceeded: 429 too many requests"
        )));
        assert!(!detect_context_window_error(&anyhow::anyhow!(
            "network timeout after 30s"
        )));
        assert!(!detect_context_window_error(&anyhow::anyhow!(
            "internal server error 500"
        )));
    }

    // ── force_compress_messages tests ─────────────────────────────────────────

    fn make_messages(n: usize, with_system: bool) -> Vec<ChatMessage> {
        let mut msgs = Vec::new();
        if with_system {
            msgs.push(ChatMessage::new("system", "System prompt"));
        }
        for i in 0..n {
            msgs.push(ChatMessage::new(
                if i % 2 == 0 { "user" } else { "assistant" },
                format!("message {}", i),
            ));
        }
        msgs
    }

    #[test]
    fn test_force_compress_noop_when_short() {
        let mut msgs = make_messages(3, true);
        let original_len = msgs.len(); // system + 3 = 4
        force_compress_messages(&mut msgs);
        assert_eq!(
            msgs.len(),
            original_len,
            "should not compress when len <= 4"
        );
    }

    #[test]
    fn test_force_compress_drops_oldest_half_with_system() {
        // system + 10 conversation messages = 11 total
        let mut msgs = make_messages(10, true);
        let before = msgs.len();
        force_compress_messages(&mut msgs);

        // conv_len = 11 - 1(sys) - 1(last) = 9 → drop_count = 4
        let expected_dropped = (before - 1 - 1) / 2;
        assert_eq!(
            msgs.len(),
            before - expected_dropped,
            "should drop oldest half of conversation"
        );
    }

    #[test]
    fn test_force_compress_preserves_system_prompt() {
        let mut msgs = make_messages(10, true);
        force_compress_messages(&mut msgs);
        assert_eq!(msgs[0].role, "system");
        assert_eq!(msgs[0].content, "System prompt");
    }

    #[test]
    fn test_force_compress_preserves_last_message() {
        let mut msgs = make_messages(10, true);
        let last_content = msgs.last().unwrap().content.clone();
        force_compress_messages(&mut msgs);
        assert_eq!(
            msgs.last().unwrap().content,
            last_content,
            "most recent message must be preserved after compression"
        );
    }

    #[test]
    fn test_force_compress_without_system_prompt() {
        // 10 messages without a system prompt
        let mut msgs = make_messages(10, false);
        let before = msgs.len();
        let last_content = msgs.last().unwrap().content.clone();
        force_compress_messages(&mut msgs);

        // conv_len = 10 - 0(sys) - 1(last) = 9 → drop_count = 4
        let expected_dropped = (before - 1) / 2;
        assert_eq!(msgs.len(), before - expected_dropped);
        assert_eq!(
            msgs.last().unwrap().content,
            last_content,
            "last message preserved even without system prompt"
        );
    }

    #[test]
    fn test_force_compress_idempotent_on_tiny_history() {
        // Exactly 4 messages (threshold boundary) — should be a no-op
        let mut msgs = make_messages(3, true); // system + 3 = 4
        let before = msgs.len();
        force_compress_messages(&mut msgs);
        assert_eq!(msgs.len(), before, "len==4 is the no-op boundary");
    }

    // ── shell output truncation constant ──────────────────────────────────────

    #[test]
    fn test_max_output_chars_constant() {
        // Verify the constant is exported and matches our agreed value from
        // the comparison with picoclaw (10 000 chars, shell.go maxLen).
        assert_eq!(
            adaclaw_tools::shell::MAX_OUTPUT_CHARS,
            10_000,
            "ShellTool output ceiling must match picoclaw's 10 000-char limit"
        );
    }

    // ── truncate() helper (used for memory indexing snippets) ─────────────────

    #[test]
    fn test_truncate_helper_noop_when_within_limit() {
        // Strings shorter than the limit must be returned unchanged (same slice).
        assert_eq!(truncate("hello", 100), "hello");
        assert_eq!(truncate("", 10), "");
    }

    #[test]
    fn test_truncate_helper_ascii_exact_limit() {
        // `truncate` uses byte length as the limit.  For ASCII, bytes == chars.
        let s = "abcdefghij"; // 10 bytes
        assert_eq!(
            truncate(s, 10),
            "abcdefghij",
            "at-limit ASCII must be unchanged"
        );
        assert_eq!(
            truncate(s, 5),
            "abcde",
            "over-limit ASCII truncated at byte 5"
        );
    }

    #[test]
    fn test_truncate_helper_multibyte_does_not_panic() {
        // Each CJK char is 3 bytes.  truncate() must not panic and must return
        // valid UTF-8 even when the byte limit falls in the middle of a character.
        let s = "中文Rust"; // 中=3B, 文=3B, R=1, u=1, s=1, t=1 → 10 bytes total
        // Limit of 4 bytes falls mid-character (中=0-2, 文=3-5) — must walk back
        let result = truncate(s, 4);
        assert!(
            std::str::from_utf8(result.as_bytes()).is_ok(),
            "result must be valid UTF-8"
        );
        // The result must be a prefix of the original string
        assert!(
            s.starts_with(result),
            "result must be a valid prefix of the original"
        );
    }

    #[test]
    fn test_truncate_helper_multibyte_char_boundary_alignment() {
        // With limit=3, the function starts at byte 3 which IS a char boundary
        // (end of '中').  So the result must be "中".
        let s = "中文hello";
        let result = truncate(s, 3);
        assert_eq!(result, "中", "byte limit 3 = end of first CJK char");
    }

    // ── parallel tool execution — Err isolation ───────────────────────────────

    /// Documents the key safety property of `join_all` used in the tool execution
    /// loop: `join_all` always collects **all** results and never short-circuits
    /// when one future resolves to `Err`.  This means a tool returning `Err` does
    /// NOT prevent other concurrently-running tools from completing.
    ///
    /// The engine's result-processing loop then converts each `Err` to an error
    /// message (role="tool") so the LLM can see what failed and continue.
    ///
    /// ⚠️  Panic isolation caveat: a `panic!` inside a future passed to `join_all`
    /// WILL propagate through the `join_all` await and bring down the current
    /// agent turn.  Full panic isolation would require `tokio::task::spawn` (which
    /// captures panics as `JoinError::is_panic()`).  Tool authors should not panic
    /// and should instead return `Err(...)`.
    #[tokio::test]
    async fn test_join_all_collects_all_results_on_partial_failure() {
        use futures_util::future::join_all;

        // Simulate 3 concurrent tool executions: A succeeds, B fails, C succeeds.
        // join_all must return all 3 results (not short-circuit after B fails).
        let futures: Vec<_> = vec![
            futures_util::future::ready(Ok::<&str, &str>("tool_a: success")),
            futures_util::future::ready(Err::<&str, &str>("tool_b: connection refused")),
            futures_util::future::ready(Ok::<&str, &str>("tool_c: success")),
        ];

        let results = join_all(futures).await;

        assert_eq!(
            results.len(),
            3,
            "join_all must collect ALL results without short-circuiting"
        );
        assert!(results[0].is_ok(), "tool_a must succeed");
        assert!(results[1].is_err(), "tool_b must fail");
        assert!(
            results[2].is_ok(),
            "tool_c must succeed even though tool_b failed"
        );
        assert_eq!(results[0].unwrap(), "tool_a: success");
        assert_eq!(results[2].unwrap(), "tool_c: success");
    }

    #[tokio::test]
    async fn test_join_all_all_failing_tools_still_collects_all() {
        use futures_util::future::join_all;

        // When ALL tools fail, join_all must still return all errors
        // (not just the first one) so the engine can report them all.
        let futures: Vec<_> = vec![
            futures_util::future::ready(Err::<&str, &str>("tool_a: timeout")),
            futures_util::future::ready(Err::<&str, &str>("tool_b: permission denied")),
        ];

        let results = join_all(futures).await;

        assert_eq!(results.len(), 2, "all failure results must be collected");
        assert!(results.iter().all(|r| r.is_err()), "all must be Err");
        assert_eq!(results[0].unwrap_err(), "tool_a: timeout");
        assert_eq!(results[1].unwrap_err(), "tool_b: permission denied");
    }

    // ── dedup logic ───────────────────────────────────────────────────────────

    #[test]
    fn test_tool_call_dedup_key_is_full_json() {
        // The dedup key is the full JSON string of the tool call object.
        // Two calls with the same name but different arguments must NOT be deduped.
        use serde_json::json;

        let call_a = json!({"name": "shell", "arguments": {"command": "ls"}});
        let call_b = json!({"name": "shell", "arguments": {"command": "pwd"}});
        let call_c = json!({"name": "shell", "arguments": {"command": "ls"}}); // duplicate of a

        let mut dedup = std::collections::HashSet::<String>::new();
        assert!(
            dedup.insert(call_a.to_string()),
            "call_a must be inserted (first occurrence)"
        );
        assert!(
            dedup.insert(call_b.to_string()),
            "call_b must be inserted (different args)"
        );
        assert!(
            !dedup.insert(call_c.to_string()),
            "call_c must be rejected (exact duplicate of call_a)"
        );
        assert_eq!(dedup.len(), 2, "only 2 unique calls");
    }

    #[test]
    fn test_tool_call_dedup_same_name_different_tools_not_deduped() {
        // Two calls to different tools with identical-looking args must both be kept.
        use serde_json::json;

        let call_a = json!({"name": "file_read",  "arguments": {"path": "README.md"}});
        let call_b = json!({"name": "file_write", "arguments": {"path": "README.md"}});

        let mut dedup = std::collections::HashSet::<String>::new();
        assert!(dedup.insert(call_a.to_string()));
        assert!(
            dedup.insert(call_b.to_string()),
            "different tool names → not a duplicate"
        );
    }
}
