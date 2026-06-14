//! System-prompt assembly for the agent harness.
//!
//! Before this module existed, the only `system` content sent to the LLM was the
//! operator-supplied `system_extra` (default `None`).  The model was therefore
//! never told **what tools exist**, **how to call them**, who it is, or which
//! skills are available — so tool calling could only work if an operator manually
//! transcribed every tool schema and the call format into their config.
//!
//! `build_system_prompt` closes that gap.  It assembles, in order:
//!
//!   1. **Identity**       — `IDENTITY.md` (or the built-in default)
//!   2. **Tool protocol**  — how to emit a tool call (matches [`crate::agents::parser`])
//!   3. **Tool catalog**   — every tool's name, description, and JSON parameter schema
//!   4. **Skills**         — the `<available_skills>` block from `workspace/skills/`
//!   5. **Operator extra** — the `system_extra` string from config, appended last
//!
//! The rendering functions are pure (no file IO) so they can be unit-tested; the
//! file-backed pieces (identity, skills) are loaded by
//! [`crate::agents::instance::AgentInstance::build_system_prompt`].

use adaclaw_core::tool::Tool;

/// Instructions that teach the model the tool-call wire format.
///
/// The format described here MUST stay in lock-step with what
/// [`crate::agents::parser::ToolCallParser`] accepts.  We document the
/// `<tool_call>` XML form as canonical because it is unambiguous, compact, and
/// survives surrounding prose better than a bare fenced block.
const TOOL_PROTOCOL: &str = r#"## Using tools

You can call tools to gather information or take actions. To call a tool, emit a
tool call wrapped in `<tool_call>` tags containing a JSON object with a `name`
and an `arguments` object:

<tool_call>{"name": "<tool_name>", "arguments": {"<param>": "<value>"}}</tool_call>

Rules:
- Emit a `<tool_call>` block only for a tool listed in "Available tools" below.
- `arguments` must match that tool's parameter schema exactly (valid JSON).
- You may emit several `<tool_call>` blocks in one turn to run tools in parallel.
- After emitting tool calls, STOP and wait. Each result is returned to you on the
  next turn as `[<tool_name>]: <result>`.
- When you have everything you need and the task is done, reply to the user in
  plain language with NO `<tool_call>` tags."#;

/// Render the "Available tools" catalog from a set of tools.
///
/// Returns an empty string when `tools` is empty, so the caller can append it
/// unconditionally without producing a dangling header.
pub fn render_tool_catalog(tools: &[Box<dyn Tool>]) -> String {
    if tools.is_empty() {
        return String::new();
    }

    let mut out = String::from("## Available tools\n\n");
    for tool in tools {
        out.push_str(&format!("### {}\n", tool.name()));
        let desc = tool.description().trim();
        if !desc.is_empty() {
            out.push_str(desc);
            out.push('\n');
        }
        // Compact (single-line) JSON keeps the prompt small; the schema is for the
        // model to read, not for humans, so pretty-printing wastes tokens.
        let schema = tool.parameters_schema();
        let schema_str = serde_json::to_string(&schema).unwrap_or_else(|_| "{}".to_string());
        out.push_str("Parameters (JSON schema): ");
        out.push_str(&schema_str);
        out.push_str("\n\n");
    }
    out
}

/// Assemble the full system prompt from its already-rendered parts.
///
/// Each section is optional: empty inputs are skipped so the result never
/// contains a header with no body or runs of blank lines.  Sections are joined
/// with a blank line between them.
pub fn assemble_system_prompt(
    identity: &str,
    tool_catalog: &str,
    skills_section: &str,
    system_extra: Option<&str>,
) -> String {
    let mut sections: Vec<&str> = Vec::new();

    let identity = identity.trim();
    if !identity.is_empty() {
        sections.push(identity);
    }

    // The protocol instructions are only meaningful when at least one tool is
    // advertised — otherwise we are teaching a format the model can never use.
    let tool_catalog = tool_catalog.trim();
    if !tool_catalog.is_empty() {
        sections.push(TOOL_PROTOCOL);
        sections.push(tool_catalog);
    }

    let skills_section = skills_section.trim();
    if !skills_section.is_empty() {
        sections.push(skills_section);
    }

    let extra = system_extra.map(str::trim).unwrap_or("");
    if !extra.is_empty() {
        sections.push(extra);
    }

    sections.join("\n\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use adaclaw_core::tool::Tool;

    /// A minimal tool used only to exercise catalog rendering.
    struct FakeTool {
        name: &'static str,
        desc: &'static str,
    }

    #[async_trait::async_trait]
    impl Tool for FakeTool {
        fn name(&self) -> &str {
            self.name
        }
        fn description(&self) -> &str {
            self.desc
        }
        fn parameters_schema(&self) -> serde_json::Value {
            serde_json::json!({
                "type": "object",
                "properties": {"path": {"type": "string"}},
                "required": ["path"]
            })
        }
        fn spec(&self) -> adaclaw_core::tool::ToolSpec {
            adaclaw_core::tool::ToolSpec {
                name: self.name.to_string(),
                description: self.desc.to_string(),
                parameters: self.parameters_schema(),
            }
        }
        async fn execute(
            &self,
            _args: serde_json::Value,
        ) -> anyhow::Result<adaclaw_core::tool::ToolResult> {
            Ok(adaclaw_core::tool::ToolResult {
                success: true,
                output: String::new(),
                error: None,
            })
        }
    }

    fn fake_tools() -> Vec<Box<dyn Tool>> {
        vec![
            Box::new(FakeTool {
                name: "file_read",
                desc: "Read a file from disk.",
            }),
            Box::new(FakeTool {
                name: "shell",
                desc: "Run a shell command.",
            }),
        ]
    }

    #[test]
    fn catalog_lists_every_tool_with_schema() {
        let catalog = render_tool_catalog(&fake_tools());
        assert!(catalog.contains("### file_read"));
        assert!(catalog.contains("Read a file from disk."));
        assert!(catalog.contains("### shell"));
        // Schema is rendered as compact JSON.
        assert!(catalog.contains("\"required\":[\"path\"]"));
    }

    #[test]
    fn catalog_empty_when_no_tools() {
        let tools: Vec<Box<dyn Tool>> = vec![];
        assert!(render_tool_catalog(&tools).is_empty());
    }

    #[test]
    fn assemble_includes_protocol_only_when_tools_present() {
        let catalog = render_tool_catalog(&fake_tools());
        let with = assemble_system_prompt("You are Ada.", &catalog, "", None);
        assert!(with.contains("You are Ada."));
        assert!(with.contains("## Using tools"));
        assert!(with.contains("## Available tools"));

        // No tools → no protocol section (we don't teach an unusable format).
        let without = assemble_system_prompt("You are Ada.", "", "", None);
        assert!(without.contains("You are Ada."));
        assert!(!without.contains("## Using tools"));
    }

    #[test]
    fn assemble_appends_system_extra_last() {
        let catalog = render_tool_catalog(&fake_tools());
        let out = assemble_system_prompt("Identity here.", &catalog, "", Some("Be terse."));
        let extra_pos = out.find("Be terse.").unwrap();
        let identity_pos = out.find("Identity here.").unwrap();
        assert!(extra_pos > identity_pos, "system_extra must come last");
    }

    #[test]
    fn assemble_skips_all_empty_sections() {
        assert_eq!(assemble_system_prompt("", "", "", None), "");
        assert_eq!(assemble_system_prompt("  ", "", "", Some("  ")), "");
    }

    #[test]
    fn assemble_includes_skills_section() {
        let out = assemble_system_prompt(
            "Identity.",
            "",
            "## Available Skills\n<available_skills></available_skills>",
            None,
        );
        assert!(out.contains("## Available Skills"));
    }
}
