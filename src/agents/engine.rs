use adaclaw_core::memory::{Category, Memory, RecallScope};
use adaclaw_core::provider::{ChatMessage, ChatRequest, Provider};
use adaclaw_core::tool::Tool;
use anyhow::{anyhow, Result};
use futures_util::future::join_all;
use std::collections::HashSet;
use std::sync::Arc;
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
    pub fn new(role: impl Into<String>, content: impl Into<String>, topic_id: impl Into<String>) -> Self {
        Self {
            role: role.into(),
            content: content.into(),
            topic_id: topic_id.into(),
            hidden: false,
        }
    }

    /// Convert to a `ChatMessage` for sending to the LLM.
    pub fn to_chat_message(&self) -> ChatMessage {
        ChatMessage {
            role: self.role.clone(),
            content: self.content.clone(),
        }
    }
}

// ── AgentEngine ───────────────────────────────────────────────────────────────

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
        }
    }

    /// Attach a memory backend for conversation indexing.
    pub fn with_memory(mut self, memory: Arc<dyn Memory>, session_id: impl Into<String>) -> Self {
        self.memory = Some(memory);
        self.session_id = session_id.into();
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
        self.run_tool_loop_with_options(provider, tools, input, model, temp, None, None)
            .await
    }

    /// Run the tool-call loop with full options.
    pub async fn run_tool_loop_with_options(
        &self,
        provider: &dyn Provider,
        tools: &[Box<dyn Tool>],
        input: &str,
        model: &str,
        temp: f64,
        system: Option<&str>,
        max_iterations: Option<usize>,
    ) -> Result<String> {
        let max_iter = max_iterations.unwrap_or(DEFAULT_MAX_ITERATIONS);

        // ── Step 1: Determine recall scope ────────────────────────────────────
        //
        // Two-tier clean intent detection:
        //   Tier 1 — Keyword fast-path (free, < 1 µs)
        //   Tier 2 — LLM intent check (only when topic drift is detected and
        //            tier 1 didn't match; typically rare, so low aggregate cost)
        let scope = if detect_clean_intent(input) {
            // Tier 1 matched — user clearly wants clean-slate thinking
            debug!(input, "Clean intent detected via keyword — using RecallScope::Clean");
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
                debug!(input, "Clean intent detected via LLM — using RecallScope::Clean");
                RecallScope::Clean
            } else {
                if let adaclaw_memory::topic::TopicSwitchResult::Switched { ref new_topic_id } = switch_result {
                    debug!(new_topic_id, "Topic switch detected — pruning old context");
                    self.prune_history_for_topic_switch(new_topic_id);
                }
                switch_result.to_recall_scope()
            }
        };

        let current_topic = self.topic_manager.current_topic_id();

        // ── Step 2: Build visible messages from history + new user message ────
        let mut messages: Vec<ChatMessage> = self.visible_messages();
        messages.push(ChatMessage {
            role: "user".to_string(),
            content: input.to_string(),
        });

        // Add user message to persistent history
        self.push_history(MessageEntry::new("user", input, &current_topic));

        for iteration in 0..max_iter {
            crate::agents::compact::trim_history(&mut messages);

            let req = ChatRequest {
                messages: &messages,
                system,
            };

            let response = provider.chat(req, model, temp).await?;
            debug!(
                iteration,
                response_len = response.content.len(),
                "Agent got LLM response"
            );

            messages.push(ChatMessage {
                role: "assistant".to_string(),
                content: response.content.clone(),
            });

            let parsed_calls = crate::agents::parser::ToolCallParser::parse(&response.content)?;

            if parsed_calls.is_empty() {
                // No more tool calls — turn complete
                let scrubbed = adaclaw_security::scrub::scrub_credentials(&response.content);

                // Add assistant reply to persistent history
                self.push_history(MessageEntry::new("assistant", &scrubbed, &current_topic));

                // Index this conversation turn into memory
                self.index_conversation(input, &scrubbed, &current_topic, &scope).await;

                return Ok(scrubbed);
            }

            // De-duplicate tool calls
            let mut dedup = HashSet::<String>::new();
            let mut tasks: Vec<(&Box<dyn Tool>, serde_json::Value)> = Vec::new();

            for call in &parsed_calls {
                let call_str = call.to_string();
                if !dedup.insert(call_str) {
                    warn!("Duplicate tool call skipped: {}", call["name"]);
                    continue;
                }

                let name = match call.get("name").and_then(|n| n.as_str()) {
                    Some(n) => n,
                    None => {
                        warn!("Tool call missing 'name' field, skipping");
                        continue;
                    }
                };

                if let Some(tool) = tools.iter().find(|t| t.name() == name) {
                    let args = call
                        .get("arguments")
                        .cloned()
                        .unwrap_or(serde_json::Value::Object(Default::default()));
                    tasks.push((tool, args));
                } else {
                    warn!("Unknown tool '{}' requested, skipping", name);
                    messages.push(ChatMessage {
                        role: "tool".to_string(),
                        content: format!("Error: tool '{}' not found", name),
                    });
                }
            }

            let futures = tasks.iter().map(|(tool, args)| {
                let name = tool.name().to_string();
                let fut = tool.execute(args.clone());
                async move { (name, fut.await) }
            });

            let results = join_all(futures).await;

            for (name, result) in results {
                let (content, success) = match result {
                    Ok(res) => {
                        let out = if res.success {
                            res.output
                        } else {
                            format!(
                                "Error: {}",
                                res.error.unwrap_or_else(|| "unknown error".to_string())
                            )
                        };
                        (out, res.success)
                    }
                    Err(e) => (format!("Error executing tool: {}", e), false),
                };

                debug!(tool = %name, success, "Tool call completed");

                messages.push(ChatMessage {
                    role: "tool".to_string(),
                    content: format!("[{}]: {}", name, content),
                });
            }
        }

        Err(anyhow!(
            "Exceeded maximum tool call iterations ({}). Possible infinite loop.",
            max_iter
        ))
    }

    // ── History management ────────────────────────────────────────────────────

    fn push_history(&self, entry: MessageEntry) {
        self.history.lock().unwrap().push(entry);
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

        if let Err(e) = memory
            .store(
                &key,
                &content,
                Category::Conversation,
                Some(&self.session_id),
                Some(topic_id),
            )
            .await
        {
            warn!(key, error = %e, "Failed to index conversation turn");
        } else {
            debug!(key, topic_id, "Conversation turn indexed");
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
        "不要旧记忆", "不要之前的记忆", "忘掉之前", "忘记之前",
        "不要被之前", "不要用之前", "不用之前", "不要之前的上下文",
        "不要用旧", "清空背景", "全新思考", "不受之前影响",
        "当作第一次", "就当没聊过", "不用管之前",
        "不要历史记录", "清除上下文", "清空记忆",
        // English — natural expressions
        "forget previous", "ignore history", "ignore previous",
        "clean slate", "fresh start", "no memory",
        "without context", "without history", "without prior",
        "don't use previous", "don't use history", "don't use old",
        "as if we never", "as if it's the first",
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

fn truncate(s: &str, max_chars: usize) -> &str {
    if s.len() <= max_chars {
        return s;
    }
    let mut idx = max_chars;
    while !s.is_char_boundary(idx) {
        idx -= 1;
    }
    &s[..idx]
}

// ── unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

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
        assert!(!history[2].hidden, "current topic message should be visible");
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
}
