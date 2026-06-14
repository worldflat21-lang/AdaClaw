//! Shared OpenAI Chat Completions wire-format helpers.
//!
//! Both the canonical OpenAI provider ([`crate::openai`]) and the table-driven
//! OpenAI-compatible providers ([`crate::openai_compat`]) speak the same wire
//! protocol for messages, the `tools` array, and `tool_calls` parsing.  Keeping
//! that format in one place means native tool calling is implemented — and
//! tested — exactly once, and every OpenAI-compatible vendor inherits it.

use adaclaw_core::provider::{
    ChatRequest, ChatResponse, ChatStream, StreamChunk, ToolCall, Usage,
};
use adaclaw_core::tool::ToolSpec;
use anyhow::Result;
use serde_json::{Value, json};

/// Build the `messages` array, faithfully re-encoding assistant tool-call turns
/// and tool-result turns so multi-turn native tool calling round-trips.
///
/// - A message with `tool_call_id = Some(id)` becomes a `{"role":"tool", ...}`
///   result message (OpenAI links it to the call by id).
/// - A message carrying `tool_calls` becomes an assistant message that echoes
///   those calls (OpenAI requires the original calls to be present on the
///   follow-up request, next to the matching tool results).
/// - Everything else is a plain `{"role", "content"}` message.
pub fn build_messages(req: &ChatRequest<'_>) -> Vec<Value> {
    let mut msgs = Vec::new();
    if let Some(sys) = req.system {
        msgs.push(json!({"role": "system", "content": sys}));
    }
    for m in req.messages {
        if let Some(tcid) = &m.tool_call_id {
            msgs.push(json!({
                "role": "tool",
                "tool_call_id": tcid,
                "content": m.content,
            }));
        } else if !m.tool_calls.is_empty() {
            let calls: Vec<Value> = m
                .tool_calls
                .iter()
                .map(|c| {
                    json!({
                        "id": c.id,
                        "type": "function",
                        "function": {
                            "name": c.name,
                            // OpenAI expects arguments as a JSON-encoded string.
                            "arguments": c.arguments.to_string(),
                        },
                    })
                })
                .collect();
            msgs.push(json!({
                "role": "assistant",
                // content may be empty when the model only called tools.
                "content": if m.content.is_empty() {
                    Value::Null
                } else {
                    Value::String(m.content.clone())
                },
                "tool_calls": calls,
            }));
        } else if !m.images.is_empty() {
            // Multimodal user turn: content becomes an array of text + image parts.
            let mut parts: Vec<Value> = Vec::new();
            if !m.content.is_empty() {
                parts.push(json!({"type": "text", "text": m.content}));
            }
            for img in &m.images {
                parts.push(json!({
                    "type": "image_url",
                    "image_url": {
                        "url": format!("data:{};base64,{}", img.media_type, img.data_base64),
                    },
                }));
            }
            msgs.push(json!({"role": m.role, "content": parts}));
        } else {
            msgs.push(json!({"role": m.role, "content": m.content}));
        }
    }
    msgs
}

/// Convert tool specs into the OpenAI `tools` array.
pub fn build_tools(tools: &[ToolSpec]) -> Vec<Value> {
    tools
        .iter()
        .map(|t| {
            json!({
                "type": "function",
                "function": {
                    "name": t.name,
                    "description": t.description,
                    "parameters": t.parameters,
                },
            })
        })
        .collect()
}

/// Parse `tool_calls` out of an OpenAI `choices[0].message` object.
///
/// OpenAI returns `function.arguments` as a JSON-encoded **string**; we decode
/// it back into a `Value` so the engine can hand it straight to the tool.  If
/// decoding fails (malformed model output) we keep the raw value rather than
/// dropping the call.
pub fn parse_tool_calls(message: &Value) -> Vec<ToolCall> {
    message["tool_calls"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|tc| {
                    let func = &tc["function"];
                    let name = func["name"].as_str()?.to_string();
                    let arguments = func["arguments"]
                        .as_str()
                        .and_then(|s| serde_json::from_str::<Value>(s).ok())
                        .unwrap_or_else(|| func["arguments"].clone());
                    Some(ToolCall {
                        id: tc["id"].as_str().unwrap_or("").to_string(),
                        name,
                        arguments,
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Parse the `usage` object from an OpenAI response root, if present.
pub fn parse_usage(data: &Value) -> Option<Usage> {
    let u = &data["usage"];
    if !u.is_object() {
        return None;
    }
    Some(Usage {
        prompt_tokens: u["prompt_tokens"].as_u64().unwrap_or(0) as u32,
        completion_tokens: u["completion_tokens"].as_u64().unwrap_or(0) as u32,
        total_tokens: u["total_tokens"].as_u64().unwrap_or(0) as u32,
    })
}

// ── Streaming (Server-Sent Events) ───────────────────────────────────────────

/// Accumulates one tool call across streamed deltas. OpenAI sends a tool call's
/// `id`/`name` once and its `arguments` JSON in fragments, keyed by `index`.
#[derive(Default, Clone)]
struct ToolCallBuilder {
    id: String,
    name: String,
    arguments: String,
}

/// Assembles a streamed OpenAI chat completion from its `data:` chunks.
///
/// [`push`](Self::push) is fed each chunk's JSON and returns the text delta in
/// that chunk (if any) for live display; [`finish`](Self::finish) produces the
/// complete [`ChatResponse`] (content + assembled tool calls + usage).
#[derive(Default)]
pub struct OpenAiStreamAssembler {
    content: String,
    tool_calls: Vec<ToolCallBuilder>,
    usage: Option<Usage>,
}

impl OpenAiStreamAssembler {
    /// Process one SSE chunk; returns the text delta it contributed, if any.
    pub fn push(&mut self, chunk: &Value) -> Option<String> {
        // usage arrives in a final, choices-empty chunk (stream_options.include_usage)
        if let Some(u) = parse_usage(chunk) {
            self.usage = Some(u);
        }
        let delta = &chunk["choices"][0]["delta"];

        // Tool-call fragments, accumulated by index.
        if let Some(tcs) = delta["tool_calls"].as_array() {
            for tc in tcs {
                let idx = tc["index"].as_u64().unwrap_or(0) as usize;
                if self.tool_calls.len() <= idx {
                    self.tool_calls.resize(idx + 1, ToolCallBuilder::default());
                }
                let b = &mut self.tool_calls[idx];
                if let Some(id) = tc["id"].as_str().filter(|s| !s.is_empty()) {
                    b.id = id.to_string();
                }
                let f = &tc["function"];
                if let Some(name) = f["name"].as_str().filter(|s| !s.is_empty()) {
                    b.name = name.to_string();
                }
                if let Some(args) = f["arguments"].as_str() {
                    b.arguments.push_str(args);
                }
            }
        }

        match delta["content"].as_str() {
            Some(text) if !text.is_empty() => {
                self.content.push_str(text);
                Some(text.to_string())
            }
            _ => None,
        }
    }

    /// Produce the final response once the stream ends.
    pub fn finish(self) -> ChatResponse {
        let tool_calls = self
            .tool_calls
            .into_iter()
            .filter(|b| !b.name.is_empty())
            .map(|b| ToolCall {
                id: b.id,
                name: b.name,
                arguments: serde_json::from_str(&b.arguments).unwrap_or(Value::Null),
            })
            .collect();
        ChatResponse {
            content: self.content,
            reasoning_content: None,
            tool_calls,
            usage: self.usage,
        }
    }
}

/// Pop one complete line (terminated by `\n`) from `buf`, without the trailing
/// newline/CR. Returns `None` when no complete line is buffered yet — the
/// partial remainder stays in `buf` for the next read. Generic SSE framing,
/// shared with the Anthropic streaming parser.
pub(crate) fn take_sse_line(buf: &mut String) -> Option<String> {
    let pos = buf.find('\n')?;
    let line = buf[..pos].trim_end_matches('\r').to_string();
    buf.drain(..=pos);
    Some(line)
}

/// Issue a streaming OpenAI-compatible chat request and return a stream of
/// [`StreamChunk`]s. `body` must already include `"stream": true`.
///
/// HTTP/auth errors surface as the returned `Result`'s `Err` (so the engine's
/// context-window retry still works); a mid-stream transport error surfaces as
/// an `Err` item inside the stream.
pub async fn stream_chat(
    client: &reqwest::Client,
    url: String,
    bearer: Option<String>,
    body: Value,
) -> Result<ChatStream> {
    use futures_util::StreamExt;

    let mut builder = client.post(&url).json(&body);
    if let Some(b) = &bearer {
        builder = builder.header("Authorization", format!("Bearer {}", b));
    }
    let resp = builder.send().await?;

    if !resp.status().is_success() {
        let status = resp.status().as_u16();
        let retry_after = resp
            .headers()
            .get("retry-after")
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.parse::<u64>().ok());
        let text = resp.text().await.unwrap_or_default();
        return Err(anyhow::Error::new(crate::error::ProviderError::from_status(
            status,
            &text,
            retry_after,
        )));
    }

    let s = async_stream::stream! {
        let byte_stream = resp.bytes_stream();
        futures_util::pin_mut!(byte_stream);
        let mut buf = String::new();
        let mut asm = OpenAiStreamAssembler::default();

        'outer: while let Some(chunk) = byte_stream.next().await {
            let chunk = match chunk {
                Ok(c) => c,
                Err(e) => {
                    yield Err(anyhow::Error::new(e));
                    return;
                }
            };
            buf.push_str(&String::from_utf8_lossy(&chunk));
            while let Some(line) = take_sse_line(&mut buf) {
                let line = line.trim();
                let data = match line.strip_prefix("data:") {
                    Some(d) => d.trim(),
                    None => continue, // skip event:/id:/comments/blank lines
                };
                if data == "[DONE]" {
                    break 'outer;
                }
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

#[cfg(test)]
mod tests {
    use super::*;
    use adaclaw_core::provider::ChatMessage;

    #[test]
    fn build_messages_encodes_tool_result_turn() {
        let history = vec![ChatMessage::tool_result("call_1", "the result")];
        let req = ChatRequest {
            messages: &history,
            system: None,
        };
        let msgs = build_messages(&req);
        assert_eq!(msgs[0]["role"], "tool");
        assert_eq!(msgs[0]["tool_call_id"], "call_1");
        assert_eq!(msgs[0]["content"], "the result");
    }

    #[test]
    fn build_messages_echoes_assistant_tool_calls() {
        let calls = vec![ToolCall {
            id: "call_1".to_string(),
            name: "shell".to_string(),
            arguments: json!({"command": "ls"}),
        }];
        let history = vec![ChatMessage::assistant_tool_calls("", calls)];
        let req = ChatRequest {
            messages: &history,
            system: None,
        };
        let msgs = build_messages(&req);
        assert_eq!(msgs[0]["role"], "assistant");
        assert_eq!(msgs[0]["content"], Value::Null);
        let tc = &msgs[0]["tool_calls"][0];
        assert_eq!(tc["id"], "call_1");
        assert_eq!(tc["type"], "function");
        assert_eq!(tc["function"]["name"], "shell");
        // arguments must be a JSON-encoded string, not an object.
        assert_eq!(tc["function"]["arguments"], "{\"command\":\"ls\"}");
    }

    #[test]
    fn build_messages_encodes_image_as_data_url() {
        use adaclaw_core::provider::ImageData;
        let history = vec![ChatMessage::user_with_images(
            "what is this?",
            vec![ImageData {
                media_type: "image/png".to_string(),
                data_base64: "QUJD".to_string(),
            }],
        )];
        let req = ChatRequest {
            messages: &history,
            system: None,
        };
        let msgs = build_messages(&req);
        assert_eq!(msgs[0]["role"], "user");
        // content is an array: text part then image_url part.
        assert_eq!(msgs[0]["content"][0]["type"], "text");
        assert_eq!(msgs[0]["content"][0]["text"], "what is this?");
        assert_eq!(msgs[0]["content"][1]["type"], "image_url");
        assert_eq!(
            msgs[0]["content"][1]["image_url"]["url"],
            "data:image/png;base64,QUJD"
        );
    }

    #[test]
    fn build_messages_plain_turn_unchanged() {
        let history = vec![ChatMessage::new("user", "hi")];
        let req = ChatRequest {
            messages: &history,
            system: Some("be nice"),
        };
        let msgs = build_messages(&req);
        assert_eq!(msgs[0]["role"], "system");
        assert_eq!(msgs[1]["role"], "user");
        assert_eq!(msgs[1]["content"], "hi");
    }

    #[test]
    fn build_tools_shapes_function_schema() {
        let specs = vec![ToolSpec {
            name: "shell".to_string(),
            description: "run a command".to_string(),
            parameters: json!({"type": "object"}),
        }];
        let tools = build_tools(&specs);
        assert_eq!(tools[0]["type"], "function");
        assert_eq!(tools[0]["function"]["name"], "shell");
        assert_eq!(tools[0]["function"]["description"], "run a command");
        assert_eq!(tools[0]["function"]["parameters"]["type"], "object");
    }

    #[test]
    fn parse_tool_calls_decodes_arguments_string() {
        let message = json!({
            "tool_calls": [{
                "id": "call_abc",
                "type": "function",
                "function": {"name": "shell", "arguments": "{\"command\":\"ls -la\"}"}
            }]
        });
        let calls = parse_tool_calls(&message);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].id, "call_abc");
        assert_eq!(calls[0].name, "shell");
        assert_eq!(calls[0].arguments["command"], "ls -la");
    }

    #[test]
    fn parse_tool_calls_empty_when_absent() {
        let message = json!({"content": "just text"});
        assert!(parse_tool_calls(&message).is_empty());
    }

    #[test]
    fn parse_usage_reads_token_counts() {
        let data = json!({"usage": {"prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15}});
        let u = parse_usage(&data).unwrap();
        assert_eq!(u.prompt_tokens, 10);
        assert_eq!(u.completion_tokens, 5);
        assert_eq!(u.total_tokens, 15);
    }

    #[test]
    fn parse_usage_none_when_absent() {
        assert!(parse_usage(&json!({"choices": []})).is_none());
    }

    // ── streaming ─────────────────────────────────────────────────────────────

    #[test]
    fn take_sse_line_splits_and_keeps_partial() {
        let mut buf = String::from("data: a\r\ndata: b\npart");
        assert_eq!(take_sse_line(&mut buf).as_deref(), Some("data: a"));
        assert_eq!(take_sse_line(&mut buf).as_deref(), Some("data: b"));
        // No trailing newline → partial remains for the next read.
        assert_eq!(take_sse_line(&mut buf), None);
        assert_eq!(buf, "part");
    }

    #[test]
    fn assembler_collects_text_deltas() {
        let mut asm = OpenAiStreamAssembler::default();
        let d1 = asm.push(&json!({"choices":[{"delta":{"content":"Hel"}}]}));
        let d2 = asm.push(&json!({"choices":[{"delta":{"content":"lo"}}]}));
        assert_eq!(d1.as_deref(), Some("Hel"));
        assert_eq!(d2.as_deref(), Some("lo"));
        let resp = asm.finish();
        assert_eq!(resp.content, "Hello");
        assert!(resp.tool_calls.is_empty());
    }

    #[test]
    fn assembler_reassembles_tool_call_across_fragments() {
        let mut asm = OpenAiStreamAssembler::default();
        // id + name arrive first, arguments stream in fragments.
        asm.push(&json!({"choices":[{"delta":{"tool_calls":[
            {"index":0,"id":"call_1","function":{"name":"shell","arguments":"{\"comm"}}
        ]}}]}));
        asm.push(&json!({"choices":[{"delta":{"tool_calls":[
            {"index":0,"function":{"arguments":"and\":\"ls\"}"}}
        ]}}]}));
        // usage in a final chunk
        asm.push(&json!({"choices":[],"usage":{"prompt_tokens":3,"completion_tokens":4,"total_tokens":7}}));
        let resp = asm.finish();
        assert_eq!(resp.tool_calls.len(), 1);
        assert_eq!(resp.tool_calls[0].id, "call_1");
        assert_eq!(resp.tool_calls[0].name, "shell");
        assert_eq!(resp.tool_calls[0].arguments["command"], "ls");
        assert_eq!(resp.usage.unwrap().total_tokens, 7);
    }
}
