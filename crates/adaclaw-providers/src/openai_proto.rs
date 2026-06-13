//! Shared OpenAI Chat Completions wire-format helpers.
//!
//! Both the canonical OpenAI provider ([`crate::openai`]) and the table-driven
//! OpenAI-compatible providers ([`crate::openai_compat`]) speak the same wire
//! protocol for messages, the `tools` array, and `tool_calls` parsing.  Keeping
//! that format in one place means native tool calling is implemented — and
//! tested — exactly once, and every OpenAI-compatible vendor inherits it.

use adaclaw_core::provider::{ChatRequest, ToolCall, Usage};
use adaclaw_core::tool::ToolSpec;
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
}
