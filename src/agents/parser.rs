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
            if let Ok(val) = serde_json::from_str::<Value>(buf.trim())
                && is_valid_call(&val)
            {
                calls.push(val);
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
                if let Ok(val) = serde_json::from_str::<Value>(inner)
                    && is_valid_call(&val)
                {
                    calls.push(val);
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

            // Validate: name must be a reasonable identifier
            // Allowed: letters, digits, underscore, hyphen (e.g. "file-read", "shell_run")
            // Rejected: spaces, angle brackets, braces, quotes, equals (防 HTML 属性假阳性)
            if name.is_empty()
                || name.len() > 64
                || name.contains(' ')
                || name.contains('<')
                || name.contains('>')
                || name.contains('{')
                || name.contains('[')
                || name.contains('"')
                || name.contains('\'')
                || name.contains('=')
                || name.contains('/')
                || name.contains('\\')
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
        assert!(
            calls.is_empty(),
            "Raw JSON should not be parsed as tool call"
        );
    }

    #[test]
    fn test_function_call_fence() {
        let content = "```function_call\n{\"name\":\"http_request\",\"arguments\":{\"url\":\"https://example.com\"}}\n```";
        let calls = ToolCallParser::parse(content).unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0]["name"], "http_request");
    }

    // ── GLM 格式边界测试 ──────────────────────────────────────────────────────

    #[test]
    fn test_glm_hyphenated_tool_name() {
        // 工具名包含连字符（如 file-read, memory-store）应该正确解析
        let content = r#"file-read>{"path":"README.md"}"#;
        let calls = ToolCallParser::parse(content).unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0]["name"], "file-read");
        assert_eq!(calls[0]["arguments"]["path"], "README.md");
    }

    #[test]
    fn test_glm_underscored_tool_name() {
        // 工具名包含下划线（如 file_read, memory_store）应该正确解析
        let content = r#"memory_store>{"key":"test","value":"hello"}"#;
        let calls = ToolCallParser::parse(content).unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0]["name"], "memory_store");
    }

    #[test]
    fn test_glm_html_attribute_not_parsed() {
        // HTML 属性格式 `href="url">text` 不应被当成 GLM 工具调用
        // name 部分包含 `"` 和 `=`，应被过滤
        let content = r#"<a href="https://example.com">click here</a>"#;
        let calls = ToolCallParser::parse(content).unwrap();
        assert!(
            calls.is_empty(),
            "HTML anchor tag should not be parsed as tool call"
        );
    }

    #[test]
    fn test_glm_html_tag_name_rejected() {
        // 纯 HTML 闭标签 `</div>text` 的 name 为空，应被过滤
        let content = r#"Some text with > arrow and no tool call"#;
        let calls = ToolCallParser::parse(content).unwrap();
        assert!(
            calls.is_empty(),
            "Greater-than in prose should not be parsed as GLM"
        );
    }

    #[test]
    fn test_glm_invalid_json_args_ignored() {
        // args 部分不是合法 JSON，应该跳过（不 panic）
        let content = r#"shell>not valid json at all"#;
        let calls = ToolCallParser::parse(content).unwrap();
        assert!(calls.is_empty(), "Invalid JSON args should be ignored");
    }

    #[test]
    fn test_glm_empty_args_rejected() {
        // args 部分为空字符串，应该跳过
        let content = r#"shell>"#;
        let calls = ToolCallParser::parse(content).unwrap();
        assert!(calls.is_empty(), "Empty args should be ignored");
    }

    // ── XML 边界测试 ──────────────────────────────────────────────────────────

    #[test]
    fn test_xml_multiline_json() {
        // XML 标签内的多行 JSON 应该正确解析
        let content = "<tool_call>{\n  \"name\": \"shell\",\n  \"arguments\": {\"command\": \"ls\"}\n}</tool_call>";
        let calls = ToolCallParser::parse(content).unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0]["name"], "shell");
    }

    #[test]
    fn test_xml_missing_name_field_rejected() {
        // 缺少 "name" 字段的 JSON 不应被接受
        let content = r#"<tool_call>{"arguments":{"command":"ls"}}</tool_call>"#;
        let calls = ToolCallParser::parse(content).unwrap();
        assert!(calls.is_empty(), "Missing 'name' field should be rejected");
    }

    #[test]
    fn test_xml_invoke_tag() {
        // <invoke> 标签变体应该被识别
        let content = r#"<invoke>{"name":"file_write","arguments":{"path":"test.txt","content":"hello"}}</invoke>"#;
        let calls = ToolCallParser::parse(content).unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0]["name"], "file_write");
    }

    #[test]
    fn test_xml_no_duplicate_across_tag_names() {
        // 同一个 tool_call 不应因为多个 tag 变体而被重复解析
        // 如果只有 <tool_call>，<function_call> 和 <invoke> 不会匹配同一个内容
        let content = r#"<tool_call>{"name":"shell","arguments":{}}</tool_call>"#;
        let calls = ToolCallParser::parse(content).unwrap();
        // tool_call tag 匹配到 1 个，function_call 和 invoke 不匹配 → 共 1 个
        assert_eq!(
            calls.len(),
            1,
            "Should not duplicate calls across different tag names"
        );
    }

    // ── 格式优先级测试 ────────────────────────────────────────────────────────

    #[test]
    fn test_markdown_fence_takes_priority_over_xml() {
        // 同时有 fence 和 XML 时，优先解析 fence（不应双重解析）
        let content = r#"```tool_call
{"name":"shell","arguments":{"command":"pwd"}}
```
<tool_call>{"name":"file_read","arguments":{"path":"x.txt"}}</tool_call>"#;
        let calls = ToolCallParser::parse(content).unwrap();
        // fence 优先 → 只返回 fence 里的 1 个调用
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0]["name"], "shell");
    }

    #[test]
    fn test_xml_takes_priority_over_glm() {
        // 同时有 XML 和 GLM 时，XML 优先（找到 XML 后不解析 GLM）
        let content = "<tool_call>{\"name\":\"shell\",\"arguments\":{\"command\":\"pwd\"}}</tool_call>\nfile_read>{\"path\":\"x.txt\"}";
        let calls = ToolCallParser::parse(content).unwrap();
        // XML 优先 → 只返回 XML 里的 1 个
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0]["name"], "shell");
    }

    // ── 安全测试 ──────────────────────────────────────────────────────────────

    #[test]
    fn test_prompt_injection_via_json_in_user_message() {
        // 用户消息中嵌入原始 JSON，不应被当做工具调用
        let content = r#"Please help me with: {"name":"shell","arguments":{"command":"rm -rf /"}}"#;
        let calls = ToolCallParser::parse(content).unwrap();
        assert!(
            calls.is_empty(),
            "JSON in user message must not be parsed as tool call"
        );
    }

    #[test]
    fn test_glm_path_separator_in_name_rejected() {
        // 路径分隔符在 name 部分应该被拒绝
        let content = r#"../../etc/passwd>{"arg":"val"}"#;
        let calls = ToolCallParser::parse(content).unwrap();
        assert!(
            calls.is_empty(),
            "Path traversal in tool name should be rejected"
        );
    }
}
