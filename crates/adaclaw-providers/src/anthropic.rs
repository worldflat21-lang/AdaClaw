use crate::error::ProviderError;
use crate::registry::ProviderSpec;
use adaclaw_core::provider::{
    ChatMessage, ChatRequest, ChatResponse, ChatStream, Provider, ProviderCapabilities,
    StreamChunk, ToolCall, Usage,
};
use adaclaw_core::tool::ToolSpec;
use anyhow::Result;
use async_trait::async_trait;
use reqwest::Client;
use secrecy::{ExposeSecret, Secret};
use serde_json::Value;

const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";
const ANTHROPIC_VERSION: &str = "2023-06-01";
/// Default `max_tokens` for Anthropic requests.
///
/// Raised from 4096 to 8192 — Claude 3.5 Sonnet and newer models support at
/// least 8192 output tokens.  This value is used when no `max_tokens` override
/// is provided via `ProviderConfig`.
///
/// Configurable per-provider via `config.toml`:
/// ```toml
/// [providers.anthropic]
/// max_tokens = 16384
/// ```
const DEFAULT_MAX_TOKENS: u32 = 8192;

pub struct AnthropicProvider {
    /// Phase 14-P1-2: API key wrapped in `Secret<String>`.
    key: Option<Secret<String>>,
    base_url: String,
    client: Client,
    /// Maximum output tokens per request.
    /// Defaults to `DEFAULT_MAX_TOKENS`; overrideable via `ProviderConfig`.
    max_tokens: u32,
}

impl AnthropicProvider {
    pub fn new(key: Option<&str>, url: Option<&str>, max_tokens: Option<u32>) -> Self {
        Self {
            key: key.map(|s| Secret::new(s.to_string())),
            base_url: url
                .unwrap_or(DEFAULT_BASE_URL)
                .trim_end_matches('/')
                .to_string(),
            client: Client::new(),
            max_tokens: max_tokens.unwrap_or(DEFAULT_MAX_TOKENS),
        }
    }

    /// Build the Anthropic `messages` array.
    ///
    /// Anthropic differs from OpenAI in two ways that matter for tool calling:
    ///   * The system prompt is a top-level field, never a message — filtered here.
    ///   * Tool interactions use **content blocks**: an assistant `tool_use`
    ///     block, answered by a `tool_result` block in a **user** turn. Anthropic
    ///     wants all results for one assistant turn coalesced into a single user
    ///     message, so consecutive tool-result `ChatMessage`s are merged.
    fn build_messages(req: &ChatRequest<'_>) -> Vec<Value> {
        let mut out: Vec<Value> = Vec::new();
        let mut pending_results: Vec<Value> = Vec::new();

        for m in req.messages.iter().filter(|m| m.role != "system") {
            if let Some(tcid) = &m.tool_call_id {
                pending_results.push(serde_json::json!({
                    "type": "tool_result",
                    "tool_use_id": tcid,
                    "content": m.content,
                }));
                continue;
            }

            // A non-result message ends any run of pending tool results.
            if !pending_results.is_empty() {
                out.push(serde_json::json!({
                    "role": "user",
                    "content": std::mem::take(&mut pending_results),
                }));
            }

            if !m.tool_calls.is_empty() {
                let mut blocks: Vec<Value> = Vec::new();
                if !m.content.is_empty() {
                    blocks.push(serde_json::json!({"type": "text", "text": m.content}));
                }
                for c in &m.tool_calls {
                    blocks.push(serde_json::json!({
                        "type": "tool_use",
                        "id": c.id,
                        "name": c.name,
                        "input": c.arguments,
                    }));
                }
                out.push(serde_json::json!({"role": "assistant", "content": blocks}));
            } else if !m.images.is_empty() {
                // Multimodal user turn: text + base64 image source blocks.
                let mut blocks: Vec<Value> = Vec::new();
                if !m.content.is_empty() {
                    blocks.push(serde_json::json!({"type": "text", "text": m.content}));
                }
                for img in &m.images {
                    blocks.push(serde_json::json!({
                        "type": "image",
                        "source": {
                            "type": "base64",
                            "media_type": img.media_type,
                            "data": img.data_base64,
                        },
                    }));
                }
                out.push(serde_json::json!({"role": m.role, "content": blocks}));
            } else {
                out.push(serde_json::json!({"role": m.role, "content": m.content}));
            }
        }

        if !pending_results.is_empty() {
            out.push(serde_json::json!({
                "role": "user",
                "content": pending_results,
            }));
        }

        out
    }

    /// Compute the effective top-level `system` string.
    ///
    /// Anthropic has no `system` role inside the messages array (those are
    /// filtered out by [`Self::build_messages`]), so any in-conversation
    /// `system` message — notably the rolling-summary message that
    /// `compact.rs` inserts as role `"system"` — would be silently dropped and
    /// never reach the model.  Here we fold that content into the top-level
    /// `system` field so the summary (and any other system note) is preserved.
    fn fold_system(req_system: Option<&str>, messages: &[ChatMessage]) -> Option<String> {
        let in_msg: Vec<&str> = messages
            .iter()
            .filter(|m| m.role == "system")
            .map(|m| m.content.as_str())
            .collect();
        match (req_system, in_msg.is_empty()) {
            (sys, true) => sys.map(str::to_string),
            (Some(sys), false) => Some(format!("{}\n\n{}", sys, in_msg.join("\n\n"))),
            (None, false) => Some(in_msg.join("\n\n")),
        }
    }

    /// Convert tool specs into the Anthropic `tools` array.
    fn build_tools(tools: &[ToolSpec]) -> Vec<Value> {
        tools
            .iter()
            .map(|t| {
                serde_json::json!({
                    "name": t.name,
                    "description": t.description,
                    "input_schema": t.parameters,
                })
            })
            .collect()
    }

    /// Shared request path for [`Provider::chat`] and
    /// [`Provider::chat_with_tools`].
    async fn chat_inner(
        &self,
        req: ChatRequest<'_>,
        tools: &[ToolSpec],
        model: &str,
        temp: f64,
    ) -> Result<ChatResponse> {
        let messages = Self::build_messages(&req);

        let mut body = serde_json::json!({
            "model": model,
            "max_tokens": self.max_tokens,
            "messages": messages,
            "temperature": temp,
        });

        // system prompt (+ any in-conversation system messages) goes top-level
        if let Some(sys) = Self::fold_system(req.system, req.messages) {
            body["system"] = Value::String(sys);
        }
        if !tools.is_empty() {
            body["tools"] = Value::Array(Self::build_tools(tools));
        }

        let key = self
            .key
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Anthropic API key not set"))?;

        let resp = self
            .client
            .post(format!("{}/v1/messages", self.base_url))
            .header("x-api-key", key.expose_secret())
            .header("anthropic-version", ANTHROPIC_VERSION)
            .json(&body)
            .send()
            .await?;

        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            // 在消费 body 之前先提取 Retry-After 头
            let retry_after = resp
                .headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .and_then(|s| s.parse::<u64>().ok());
            let text = resp.text().await.unwrap_or_default();
            return Err(anyhow::Error::new(ProviderError::from_status(
                status,
                &text,
                retry_after,
            )));
        }

        let data: Value = resp.json().await?;
        // Anthropic response content is an array of blocks: text and tool_use.
        let empty = Vec::new();
        let blocks = data["content"].as_array().unwrap_or(&empty);

        let mut content = String::new();
        let mut tool_calls = Vec::new();
        for block in blocks {
            match block["type"].as_str() {
                Some("text") => {
                    if let Some(t) = block["text"].as_str() {
                        content.push_str(t);
                    }
                }
                Some("tool_use") => {
                    tool_calls.push(ToolCall {
                        id: block["id"].as_str().unwrap_or("").to_string(),
                        name: block["name"].as_str().unwrap_or("").to_string(),
                        arguments: block["input"].clone(),
                    });
                }
                _ => {}
            }
        }

        // Anthropic usage: { "input_tokens": N, "output_tokens": M }
        let usage = {
            let u = &data["usage"];
            if u.is_object() {
                let prompt = u["input_tokens"].as_u64().unwrap_or(0) as u32;
                let completion = u["output_tokens"].as_u64().unwrap_or(0) as u32;
                Some(Usage {
                    prompt_tokens: prompt,
                    completion_tokens: completion,
                    total_tokens: prompt + completion,
                })
            } else {
                None
            }
        };

        Ok(ChatResponse {
            content,
            reasoning_content: None,
            tool_calls,
            usage,
        })
    }

    /// Streaming variant of [`Self::chat_inner`]: POST with `stream: true` and
    /// parse Anthropic's SSE events into [`StreamChunk`]s.
    async fn stream_messages(&self, body: Value) -> Result<ChatStream> {
        use futures_util::StreamExt;

        let key = self
            .key
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Anthropic API key not set"))?;

        let resp = self
            .client
            .post(format!("{}/v1/messages", self.base_url))
            .header("x-api-key", key.expose_secret())
            .header("anthropic-version", ANTHROPIC_VERSION)
            .json(&body)
            .send()
            .await?;

        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let retry_after = resp
                .headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .and_then(|s| s.parse::<u64>().ok());
            let text = resp.text().await.unwrap_or_default();
            return Err(anyhow::Error::new(ProviderError::from_status(
                status,
                &text,
                retry_after,
            )));
        }

        let s = async_stream::stream! {
            let byte_stream = resp.bytes_stream();
            futures_util::pin_mut!(byte_stream);
            let mut buf = String::new();
            let mut asm = AnthropicStreamAssembler::default();

            while let Some(chunk) = byte_stream.next().await {
                let chunk = match chunk {
                    Ok(c) => c,
                    Err(e) => {
                        yield Err(anyhow::Error::new(e));
                        return;
                    }
                };
                buf.push_str(&String::from_utf8_lossy(&chunk));
                while let Some(line) = crate::openai_proto::take_sse_line(&mut buf) {
                    let line = line.trim();
                    // Anthropic events carry their type in the JSON; the `event:`
                    // line is redundant, so we only parse `data:` payloads.
                    let data = match line.strip_prefix("data:") {
                        Some(d) => d.trim(),
                        None => continue,
                    };
                    if let Ok(json) = serde_json::from_str::<Value>(data)
                        && let Some(delta) = asm.push(&json)
                    {
                        yield Ok(StreamChunk::Delta(delta));
                    }
                }
            }
            yield Ok(StreamChunk::Done(asm.finish()));
        };

        Ok(Box::pin(s))
    }
}

#[async_trait]
impl Provider for AnthropicProvider {
    fn name(&self) -> &str {
        "anthropic"
    }

    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            native_tool_calling: true,
            vision: true,
            streaming: true,
        }
    }

    async fn chat(&self, req: ChatRequest<'_>, model: &str, temp: f64) -> Result<ChatResponse> {
        self.chat_inner(req, &[], model, temp).await
    }

    async fn chat_with_tools(
        &self,
        req: ChatRequest<'_>,
        tools: &[ToolSpec],
        model: &str,
        temp: f64,
    ) -> Result<ChatResponse> {
        self.chat_inner(req, tools, model, temp).await
    }

    async fn chat_stream(
        &self,
        req: ChatRequest<'_>,
        tools: &[ToolSpec],
        model: &str,
        temp: f64,
    ) -> Result<ChatStream> {
        let messages = Self::build_messages(&req);
        let mut body = serde_json::json!({
            "model": model,
            "max_tokens": self.max_tokens,
            "messages": messages,
            "temperature": temp,
            "stream": true,
        });
        if let Some(sys) = Self::fold_system(req.system, req.messages) {
            body["system"] = Value::String(sys);
        }
        if !tools.is_empty() {
            body["tools"] = Value::Array(Self::build_tools(tools));
        }
        self.stream_messages(body).await
    }

    async fn chat_with_system(
        &self,
        system: Option<&str>,
        msg: &str,
        model: &str,
        temp: f64,
    ) -> Result<String> {
        let messages = vec![ChatMessage::new("user", msg)];
        let req = ChatRequest {
            messages: &messages,
            system,
        };
        Ok(self.chat(req, model, temp).await?.content)
    }
}

// ── Streaming assembler ──────────────────────────────────────────────────────

/// A content block being assembled from the stream. Text blocks stream straight
/// into the response content; tool-use blocks accumulate `partial_json`
/// fragments (keyed by content-block index) into the call's arguments.
#[derive(Default, Clone)]
struct AnthBlock {
    is_tool: bool,
    id: String,
    name: String,
    json: String,
}

/// Assembles a streamed Anthropic message from its SSE events.
///
/// Dispatches on the event's `type`: `message_start` (input tokens),
/// `content_block_start` (opens a text or tool_use block), `content_block_delta`
/// (`text_delta` → content, `input_json_delta` → tool args), `message_delta`
/// (output tokens). `push` returns the text delta for an event, if any.
#[derive(Default)]
pub struct AnthropicStreamAssembler {
    content: String,
    blocks: Vec<AnthBlock>,
    input_tokens: u32,
    output_tokens: u32,
}

impl AnthropicStreamAssembler {
    pub fn push(&mut self, evt: &Value) -> Option<String> {
        match evt["type"].as_str() {
            Some("message_start") => {
                self.input_tokens = evt["message"]["usage"]["input_tokens"]
                    .as_u64()
                    .unwrap_or(0) as u32;
                None
            }
            Some("content_block_start") => {
                let idx = evt["index"].as_u64().unwrap_or(0) as usize;
                if self.blocks.len() <= idx {
                    self.blocks.resize(idx + 1, AnthBlock::default());
                }
                let cb = &evt["content_block"];
                if cb["type"] == "tool_use" {
                    self.blocks[idx] = AnthBlock {
                        is_tool: true,
                        id: cb["id"].as_str().unwrap_or("").to_string(),
                        name: cb["name"].as_str().unwrap_or("").to_string(),
                        json: String::new(),
                    };
                }
                None
            }
            Some("content_block_delta") => {
                let d = &evt["delta"];
                match d["type"].as_str() {
                    Some("text_delta") => match d["text"].as_str() {
                        Some(t) if !t.is_empty() => {
                            self.content.push_str(t);
                            Some(t.to_string())
                        }
                        _ => None,
                    },
                    Some("input_json_delta") => {
                        let idx = evt["index"].as_u64().unwrap_or(0) as usize;
                        if let (Some(b), Some(frag)) =
                            (self.blocks.get_mut(idx), d["partial_json"].as_str())
                        {
                            b.json.push_str(frag);
                        }
                        None
                    }
                    _ => None,
                }
            }
            Some("message_delta") => {
                if let Some(o) = evt["usage"]["output_tokens"].as_u64() {
                    self.output_tokens = o as u32;
                }
                None
            }
            _ => None, // content_block_stop, message_stop, ping
        }
    }

    pub fn finish(self) -> ChatResponse {
        let tool_calls = self
            .blocks
            .into_iter()
            .filter(|b| b.is_tool && !b.name.is_empty())
            .map(|b| ToolCall {
                id: b.id,
                name: b.name,
                arguments: serde_json::from_str(&b.json).unwrap_or(Value::Null),
            })
            .collect();
        let usage = if self.input_tokens > 0 || self.output_tokens > 0 {
            Some(Usage {
                prompt_tokens: self.input_tokens,
                completion_tokens: self.output_tokens,
                total_tokens: self.input_tokens + self.output_tokens,
            })
        } else {
            None
        };
        ChatResponse {
            content: self.content,
            reasoning_content: None,
            tool_calls,
            usage,
        }
    }
}

pub fn spec() -> ProviderSpec {
    ProviderSpec {
        name: "anthropic",
        aliases: &["claude", "claude-3", "claude-3-5-sonnet", "claude-sonnet-4"],
        local: false,
        capabilities: ProviderCapabilities {
            native_tool_calling: true,
            vision: true,
            streaming: true,
        },
        factory: Box::new(|key, url| Box::new(AnthropicProvider::new(key, url, None))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use adaclaw_core::provider::ToolCall;

    #[test]
    fn build_messages_encodes_tool_use_and_coalesces_results() {
        let calls = vec![
            ToolCall {
                id: "t1".into(),
                name: "shell".into(),
                arguments: serde_json::json!({"command": "ls"}),
            },
            ToolCall {
                id: "t2".into(),
                name: "echo".into(),
                arguments: serde_json::json!({"text": "hi"}),
            },
        ];
        let history = vec![
            ChatMessage::new("user", "do it"),
            ChatMessage::assistant_tool_calls("working", calls),
            ChatMessage::tool_result("t1", "files..."),
            ChatMessage::tool_result("t2", "hi"),
        ];
        let req = ChatRequest {
            messages: &history,
            system: None,
        };
        let msgs = AnthropicProvider::build_messages(&req);
        // user, assistant(tool_use x2), then a SINGLE user turn with both results.
        assert_eq!(msgs.len(), 3);
        assert_eq!(msgs[0]["role"], "user");
        assert_eq!(msgs[1]["role"], "assistant");
        assert_eq!(msgs[1]["content"][0]["type"], "text");
        assert_eq!(msgs[1]["content"][1]["type"], "tool_use");
        assert_eq!(msgs[1]["content"][1]["id"], "t1");
        assert_eq!(msgs[1]["content"][1]["input"]["command"], "ls");
        // Two tool results coalesced into one user message.
        assert_eq!(msgs[2]["role"], "user");
        assert_eq!(msgs[2]["content"][0]["type"], "tool_result");
        assert_eq!(msgs[2]["content"][0]["tool_use_id"], "t1");
        assert_eq!(msgs[2]["content"][1]["tool_use_id"], "t2");
    }

    #[test]
    fn build_messages_encodes_image_as_base64_source() {
        use adaclaw_core::provider::ImageData;
        let history = vec![ChatMessage::user_with_images(
            "describe",
            vec![ImageData {
                media_type: "image/jpeg".to_string(),
                data_base64: "QUJD".to_string(),
            }],
        )];
        let req = ChatRequest {
            messages: &history,
            system: None,
        };
        let msgs = AnthropicProvider::build_messages(&req);
        assert_eq!(msgs[0]["role"], "user");
        assert_eq!(msgs[0]["content"][0]["type"], "text");
        assert_eq!(msgs[0]["content"][0]["text"], "describe");
        assert_eq!(msgs[0]["content"][1]["type"], "image");
        assert_eq!(msgs[0]["content"][1]["source"]["type"], "base64");
        assert_eq!(msgs[0]["content"][1]["source"]["media_type"], "image/jpeg");
        assert_eq!(msgs[0]["content"][1]["source"]["data"], "QUJD");
    }

    #[test]
    fn build_messages_filters_system_role() {
        let history = vec![
            ChatMessage::new("system", "[Conversation summary]: earlier stuff"),
            ChatMessage::new("user", "hi"),
        ];
        let req = ChatRequest {
            messages: &history,
            system: None,
        };
        let msgs = AnthropicProvider::build_messages(&req);
        assert_eq!(msgs.len(), 1, "system-role messages are not sent inline");
        assert_eq!(msgs[0]["role"], "user");
    }

    #[test]
    fn fold_system_preserves_in_conversation_summary() {
        let history = vec![
            ChatMessage::new("system", "[Conversation summary]: earlier stuff"),
            ChatMessage::new("user", "hi"),
        ];
        // No top-level system → the summary becomes the system prompt.
        let folded = AnthropicProvider::fold_system(None, &history).unwrap();
        assert!(folded.contains("[Conversation summary]: earlier stuff"));

        // Top-level system present → both are combined, base first.
        let folded2 = AnthropicProvider::fold_system(Some("You are Ada."), &history).unwrap();
        assert!(folded2.starts_with("You are Ada."));
        assert!(folded2.contains("[Conversation summary]"));
    }

    #[test]
    fn fold_system_none_when_nothing() {
        let history = vec![ChatMessage::new("user", "hi")];
        assert!(AnthropicProvider::fold_system(None, &history).is_none());
    }

    // ── streaming assembler ───────────────────────────────────────────────────

    #[test]
    fn stream_assembler_text_tool_and_usage() {
        let mut asm = AnthropicStreamAssembler::default();
        asm.push(&serde_json::json!({"type":"message_start","message":{"usage":{"input_tokens":5,"output_tokens":1}}}));
        asm.push(&serde_json::json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}));
        let d1 = asm.push(&serde_json::json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hel"}}));
        let d2 = asm.push(&serde_json::json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"lo"}}));
        assert_eq!(d1.as_deref(), Some("Hel"));
        assert_eq!(d2.as_deref(), Some("lo"));
        // tool_use block at index 1, args streamed as partial_json fragments
        asm.push(&serde_json::json!({"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"toolu_1","name":"shell"}}));
        asm.push(&serde_json::json!({"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"cmd\":"}}));
        asm.push(&serde_json::json!({"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"\"ls\"}"}}));
        asm.push(&serde_json::json!({"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":7}}));

        let resp = asm.finish();
        assert_eq!(resp.content, "Hello");
        assert_eq!(resp.tool_calls.len(), 1);
        assert_eq!(resp.tool_calls[0].id, "toolu_1");
        assert_eq!(resp.tool_calls[0].name, "shell");
        assert_eq!(resp.tool_calls[0].arguments["cmd"], "ls");
        let u = resp.usage.unwrap();
        assert_eq!(u.prompt_tokens, 5);
        assert_eq!(u.completion_tokens, 7);
        assert_eq!(u.total_tokens, 12);
    }
}
