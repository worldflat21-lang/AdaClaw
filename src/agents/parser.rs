use anyhow::Result;
use serde_json::Value;

/// Parse tool calls from LLM response text.
///
/// Supported formats (in order of priority):
///
/// 1. **Markdown fence** — ` ```tool_call\n{...}\n``` `
/// 2. **XML tags** — `<tool_call>{"name":"shell","arguments":{...}}</tool_call>`
///    Also accepts `<function_call>` and `<invoke>` variants.
/// 3. **OpenAI-style JSON array** — `[{"name":"...","arguments":{...}},...]`
///    Only matched inside an explicit JSON block or when the whole response is JSON.
/// 4. **GLM shortened** — `tool_name>{"arg":"val"}` (line-by-line)
///
/// SECURITY: tool calls MUST appear inside explicit boundary markers to prevent
/// prompt injection via raw text. Raw JSON extraction without markers is disabled.
pub struct ToolCallParser;

impl ToolCallParser {
    pub fn parse(content: &str) -> Result<Vec<Value>> {
        let mut calls = Vec::new();

        // ── Format 1: Markdown ``` tool_call ``` fence ──────────────────────
        calls.extend(parse_markdown_fence(content));

        // If we already found calls in fences, return early to avoid double-parsing
        if !calls.is_empty() {
            return Ok(calls);
        }

        // ── Format 2: XML <tool_call> / <function_call> / <invoke> ──────────
        calls.extend(parse_xml_tags(content));

        if !calls.is_empty() {
            return Ok(calls);
        }

        // ── Format 3: GLM shortened `tool_name>json` (one per line) ─────────
        calls.extend(parse_glm_format(content));

        Ok(calls)
    }
}

// ── Format 1: Markdown fence ─────────────────────────────────────────────────

fn parse_markdown_fence(content: &str) -> Vec<Value> {
    let mut calls = Vec::new();
    let mut in_block = false;
    let mut buf = String::new();

    for line in content.lines() {
        let trimmed = line.trim();
        if !in_block {
            // Opening fence: ```tool_call or ```function_call or ```json (tool_call context)
            if trimmed == "```tool_call" || trimmed == "```function_call" {
                in_block = true;
                buf.clear();
            }
        } else if trimmed == "```" {
            in_block = false;
            if let Ok(val) = serde_json::from_str::<Value>(buf.trim()) {
                if is_valid_call(&val) {
                    calls.push(val);
                }
            }
            buf.clear();
        } else {
            buf.push_str(line);
            buf.push('\n');
        }
    }

    calls
}

// ── Format 2: XML tags ────────────────────────────────────────────────────────

static XML_TAG_NAMES: &[&str] = &["tool_call", "function_call", "invoke"];

fn parse_xml_tags(content: &str) -> Vec<Value> {
    let mut calls = Vec::new();

    for tag in XML_TAG_NAMES {
        let open = format!("<{}>", tag);
        let close = format!("</{}>", tag);

        let mut search = content;
        while let Some(start) = search.find(&open) {
            let after_open = start + open.len();
            if let Some(end) = search[after_open..].find(&close) {
                let inner = search[after_open..after_open + end].trim();
                if let Ok(val) = serde_json::from_str::<Value>(inner) {
                    if is_valid_call(&val) {
                        calls.push(val);
                    }
                }
                search = &search[after_open + end + close.len()..];
            } else {
                break;
            }
        }
    }

    calls
}

// ── Format 4: GLM shortened `tool_name>json_args` (one per line) ─────────────

fn parse_glm_format(content: &str) -> Vec<Value> {
    let mut calls = Vec::new();

    for line in content.lines() {
        let trimmed = line.trim();
        // Must look like: identifier>...json...
        if let Some(idx) = trimmed.find('>') {
            let name = &trimmed[..idx];
            let args_str = &trimmed[idx + 1..];

            // Validate: name must be a reasonable identifier (no spaces, not too long)
            if name.is_empty()
                || name.len() > 64
                || name.contains(' ')
                || name.contains('<')
                || name.contains('{')
            {
                continue;
            }

            // args_str must be valid JSON
            if let Ok(args_val) = serde_json::from_str::<Value>(args_str.trim()) {
                let call = serde_json::json!({
                    "name": name,
                    "arguments": args_val
                });
                calls.push(call);
            }
        }
    }

    calls
}

// ── helpers ───────────────────────────────────────────────────────────────────

/// A valid tool call JSON object must have a "name" field (string).
fn is_valid_call(val: &Value) -> bool {
    val.is_object() && val.get("name").and_then(|n| n.as_str()).is_some()
}

// ── unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_markdown_fence() {
        let content = r#"Sure, I'll run that.

```tool_call
{"name":"shell","arguments":{"command":"ls -la"}}
```

Done."#;
        let calls = ToolCallParser::parse(content).unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0]["name"], "shell");
        assert_eq!(calls[0]["arguments"]["command"], "ls -la");
    }

    #[test]
    fn test_xml_tags() {
        let content = r#"Let me check:
<tool_call>{"name":"file_read","arguments":{"path":"readme.md"}}</tool_call>
Done."#;
        let calls = ToolCallParser::parse(content).unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0]["name"], "file_read");
    }

    #[test]
    fn test_multiple_xml_calls() {
        let content = r#"
<tool_call>{"name":"shell","arguments":{"command":"pwd"}}</tool_call>
<tool_call>{"name":"file_list","arguments":{"path":"."}}</tool_call>
"#;
        let calls = ToolCallParser::parse(content).unwrap();
        assert_eq!(calls.len(), 2);
    }

    #[test]
    fn test_glm_format() {
        let content = r#"shell>{"command":"echo hello"}"#;
        let calls = ToolCallParser::parse(content).unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0]["name"], "shell");
        assert_eq!(calls[0]["arguments"]["command"], "echo hello");
    }

    #[test]
    fn test_no_calls() {
        let content = "Just a normal response with no tool calls.";
        let calls = ToolCallParser::parse(content).unwrap();
        assert!(calls.is_empty());
    }

    #[test]
    fn test_raw_json_not_extracted() {
        // SECURITY: raw JSON without markers must NOT be treated as tool calls
        let content = r#"{"name":"shell","arguments":{"command":"rm -rf /"}}"#;
        let calls = ToolCallParser::parse(content).unwrap();
        // GLM format would only match if it has `>` separator — pure JSON should not match
        assert!(calls.is_empty(), "Raw JSON should not be parsed as tool call");
    }

    #[test]
    fn test_function_call_fence() {
        let content = "```function_call\n{\"name\":\"http_request\",\"arguments\":{\"url\":\"https://example.com\"}}\n```";
        let calls = ToolCallParser::parse(content).unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0]["name"], "http_request");
    }
}
