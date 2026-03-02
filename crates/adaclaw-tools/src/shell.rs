use adaclaw_core::tool::{Tool, ToolResult, ToolSpec};
use anyhow::{Result, anyhow};
use async_trait::async_trait;
use serde_json::Value;
use std::path::{Path, PathBuf};
use tokio::process::Command;

/// Maximum combined output length (chars) returned to the LLM.
/// Prevents a single tool call from consuming the entire context window.
/// Matches picoclaw's 10 000-char ceiling (shell.go `maxLen = 10000`).
pub const MAX_OUTPUT_CHARS: usize = 10_000;

/// Truncate combined shell output to `MAX_OUTPUT_CHARS` Unicode characters.
///
/// When truncation occurs, appends a human-readable notice so the LLM knows
/// output was cut and can ask for the rest if needed.
///
/// Counting by **Unicode scalar values** (not bytes) ensures that multi-byte
/// characters (CJK, emoji) are handled correctly — the same approach used by
/// our Telegram message splitter.
pub(crate) fn truncate_output(raw: &str) -> String {
    let char_count = raw.chars().count();
    if char_count <= MAX_OUTPUT_CHARS {
        return raw.to_string();
    }
    let truncated: String = raw.chars().take(MAX_OUTPUT_CHARS).collect();
    format!(
        "{}\n... [output truncated: showing {}/{} chars]",
        truncated, MAX_OUTPUT_CHARS, char_count
    )
}

/// Resolve the workspace root:
/// 1. `ADACLAW_WORKSPACE` env var
/// 2. `./workspace` relative to current dir
pub fn workspace_root() -> PathBuf {
    if let Ok(p) = std::env::var("ADACLAW_WORKSPACE") {
        let pb = PathBuf::from(p);
        if pb.exists() {
            return pb.canonicalize().unwrap_or(pb);
        }
        return pb;
    }
    let default = std::env::current_dir()
        .unwrap_or_else(|_| PathBuf::from("."))
        .join("workspace");
    if default.exists() {
        default.canonicalize().unwrap_or(default)
    } else {
        default
    }
}

/// Lexically normalise a path (resolve `.` and `..`) without requiring it to exist.
pub fn normalize_path(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::ParentDir => {
                out.pop();
            }
            std::path::Component::CurDir => {}
            c => out.push(c),
        }
    }
    out
}

/// Ensure `user_path` stays within `workspace`.
/// Returns the absolute path if safe, or an error if it escapes.
pub fn safe_path(workspace: &Path, user_path: &str) -> Result<PathBuf> {
    // Reject null bytes
    if user_path.contains('\0') {
        return Err(anyhow!("Path contains null byte"));
    }

    let raw = workspace.join(user_path);
    let normalized = normalize_path(&raw);

    // For existing paths, canonicalize catches symlink attacks
    let canonical = if normalized.exists() {
        normalized.canonicalize()?
    } else {
        normalized.clone()
    };

    // Workspace may not exist yet; normalize it too
    let ws_normalized = if workspace.exists() {
        workspace.canonicalize()?
    } else {
        normalize_path(workspace)
    };

    if !canonical.starts_with(&ws_normalized) {
        return Err(anyhow!(
            "Path escape detected: '{}' is outside workspace",
            user_path
        ));
    }

    // Reject known system directories even if inside workspace somehow
    #[cfg(unix)]
    {
        let blocked = ["/etc", "/bin", "/sbin", "/usr/bin", "/proc", "/sys", "/dev"];
        let path_str = canonical.to_string_lossy();
        for b in &blocked {
            if path_str.starts_with(b) {
                return Err(anyhow!("Access to system directory denied: {}", user_path));
            }
        }
    }
    #[cfg(windows)]
    {
        let blocked = [
            "C:\\Windows",
            "C:\\Program Files",
            "C:\\Program Files (x86)",
        ];
        let path_str = canonical.to_string_lossy().to_lowercase();
        for b in &blocked {
            if path_str.starts_with(&b.to_lowercase()) {
                return Err(anyhow!("Access to system directory denied: {}", user_path));
            }
        }
    }

    Ok(canonical)
}

// ── ShellTool ─────────────────────────────────────────────────────────────────

pub struct ShellTool {
    workspace: PathBuf,
}

impl Default for ShellTool {
    fn default() -> Self {
        Self::new()
    }
}

impl ShellTool {
    pub fn new() -> Self {
        Self {
            workspace: workspace_root(),
        }
    }

    pub fn with_workspace(workspace: PathBuf) -> Self {
        Self { workspace }
    }
}

#[async_trait]
impl Tool for ShellTool {
    fn name(&self) -> &str {
        "shell"
    }

    fn description(&self) -> &str {
        "Execute a shell command in the workspace directory. Returns stdout and stderr."
    }

    fn parameters_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "command": {
                    "type": "string",
                    "description": "Shell command to execute"
                },
                "timeout_secs": {
                    "type": "integer",
                    "description": "Optional timeout in seconds (default: 30)",
                    "default": 30
                }
            },
            "required": ["command"]
        })
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: self.name().to_string(),
            description: self.description().to_string(),
            parameters: self.parameters_schema(),
        }
    }

    async fn execute(&self, args: Value) -> Result<ToolResult> {
        let command = args["command"]
            .as_str()
            .ok_or_else(|| anyhow!("Missing 'command' argument"))?;

        let timeout_secs = args["timeout_secs"].as_u64().unwrap_or(30);

        // Ensure workspace directory exists
        if !self.workspace.exists() {
            tokio::fs::create_dir_all(&self.workspace).await?;
        }

        #[cfg(unix)]
        let mut cmd = {
            let mut c = Command::new("sh");
            c.arg("-c").arg(command);
            c
        };

        #[cfg(windows)]
        let mut cmd = {
            let mut c = Command::new("cmd");
            c.args(["/C", command]);
            c
        };

        cmd.current_dir(&self.workspace);

        let result =
            tokio::time::timeout(std::time::Duration::from_secs(timeout_secs), cmd.output()).await;

        match result {
            Ok(Ok(output)) => {
                let stdout = String::from_utf8_lossy(&output.stdout).to_string();
                let stderr = String::from_utf8_lossy(&output.stderr).to_string();

                let raw_combined = if stderr.is_empty() {
                    stdout.clone()
                } else if stdout.is_empty() {
                    format!("[stderr] {}", stderr)
                } else {
                    format!("{}\n[stderr] {}", stdout, stderr)
                };

                // Truncate to prevent a single tool call from flooding the LLM context.
                // Reference: picoclaw shell.go maxLen=10000, zeroclaw MAX_OUTPUT_BYTES=1MB.
                let combined = truncate_output(&raw_combined);

                Ok(ToolResult {
                    success: output.status.success(),
                    output: combined,
                    error: if output.status.success() {
                        None
                    } else {
                        Some(format!("Exit code: {}", output.status.code().unwrap_or(-1)))
                    },
                })
            }
            Ok(Err(e)) => Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!("Failed to spawn process: {}", e)),
            }),
            Err(_) => Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!("Command timed out after {}s", timeout_secs)),
            }),
        }
    }
}

// ── unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── truncate_output ───────────────────────────────────────────────────────

    #[test]
    fn test_truncate_output_no_change_when_within_limit() {
        let input = "hello world".to_string();
        let result = truncate_output(&input);
        assert_eq!(
            result, input,
            "output within limit must be returned unchanged"
        );
    }

    #[test]
    fn test_truncate_output_exact_limit_no_change() {
        // Exactly MAX_OUTPUT_CHARS characters — must NOT be truncated.
        let input: String = "x".repeat(MAX_OUTPUT_CHARS);
        let result = truncate_output(&input);
        assert_eq!(
            result, input,
            "output at exactly MAX_OUTPUT_CHARS must not be truncated"
        );
    }

    #[test]
    fn test_truncate_output_one_over_limit_is_truncated() {
        // MAX_OUTPUT_CHARS + 1 characters — must be truncated.
        let input: String = "x".repeat(MAX_OUTPUT_CHARS + 1);
        let result = truncate_output(&input);

        // The returned string starts with exactly MAX_OUTPUT_CHARS 'x' chars
        let kept: String = result.chars().take(MAX_OUTPUT_CHARS).collect();
        assert_eq!(kept, "x".repeat(MAX_OUTPUT_CHARS));

        // The truncation notice must be present
        assert!(
            result.contains("[output truncated:"),
            "truncation notice must be appended; got: {:?}",
            &result[MAX_OUTPUT_CHARS..]
        );
    }

    #[test]
    fn test_truncate_output_large_input() {
        // 50 000 ASCII characters — well above the limit.
        let input: String = "a".repeat(50_000);
        let result = truncate_output(&input);

        // Total char count in result = MAX_OUTPUT_CHARS + newline + notice
        assert!(result.starts_with(&"a".repeat(MAX_OUTPUT_CHARS)));
        assert!(result.contains("showing 10000/50000 chars"));
    }

    #[test]
    fn test_truncate_output_counts_unicode_chars_not_bytes() {
        // Each CJK character is 3 bytes but 1 Unicode scalar value.
        // MAX_OUTPUT_CHARS CJK chars must NOT be truncated (char count == limit).
        let cjk_char = '中'; // 3 bytes
        let input: String = std::iter::repeat(cjk_char).take(MAX_OUTPUT_CHARS).collect();
        assert_eq!(
            input.len(),
            MAX_OUTPUT_CHARS * 3,
            "sanity: CJK chars are 3 bytes each"
        );

        let result = truncate_output(&input);
        // Must not be truncated (char count == MAX_OUTPUT_CHARS, not byte count)
        assert_eq!(
            result, input,
            "exactly MAX_OUTPUT_CHARS CJK characters must not be truncated"
        );
    }

    #[test]
    fn test_truncate_output_unicode_overflow_truncated_correctly() {
        // MAX_OUTPUT_CHARS + 5 CJK characters — must be truncated at the right char boundary.
        let cjk_char = '字';
        let input: String = std::iter::repeat(cjk_char)
            .take(MAX_OUTPUT_CHARS + 5)
            .collect();
        let result = truncate_output(&input);

        // The kept portion must be valid UTF-8 (no split in the middle of a char)
        let kept_portion: String = result.chars().take(MAX_OUTPUT_CHARS).collect();
        assert_eq!(kept_portion.chars().count(), MAX_OUTPUT_CHARS);
        // All kept chars must be the same CJK character
        assert!(kept_portion.chars().all(|c| c == cjk_char));
        assert!(result.contains(&format!(
            "showing {}/{} chars",
            MAX_OUTPUT_CHARS,
            MAX_OUTPUT_CHARS + 5
        )));
    }

    #[test]
    fn test_truncate_output_empty_input() {
        let result = truncate_output("");
        assert_eq!(result, "", "empty input must be returned as empty string");
    }

    #[test]
    fn test_truncate_output_notice_format() {
        // Verify the truncation notice has the exact expected format.
        let input: String = "y".repeat(MAX_OUTPUT_CHARS + 100);
        let result = truncate_output(&input);

        let expected_notice = format!(
            "\n... [output truncated: showing {}/{} chars]",
            MAX_OUTPUT_CHARS,
            MAX_OUTPUT_CHARS + 100
        );
        assert!(
            result.ends_with(&expected_notice),
            "truncation notice must have exact format; suffix was: {:?}",
            &result[result.len().saturating_sub(60)..]
        );
    }

    // ── safe_path ─────────────────────────────────────────────────────────────

    #[test]
    fn test_safe_path_rejects_null_byte() {
        let ws = std::path::PathBuf::from("/tmp/ws");
        let result = safe_path(&ws, "file\0name");
        assert!(result.is_err(), "null byte in path must be rejected");
        assert!(result.unwrap_err().to_string().contains("null byte"));
    }

    #[test]
    fn test_safe_path_rejects_path_traversal() {
        // `../../etc/passwd` would escape the workspace on any OS.
        let ws = std::path::PathBuf::from(std::env::temp_dir()).join("adaclaw_test_ws");
        let result = safe_path(&ws, "../../etc/passwd");
        assert!(result.is_err(), "path traversal must be rejected");
    }

    #[test]
    fn test_normalize_path_resolves_dotdot() {
        let p = std::path::PathBuf::from("/workspace/subdir/../file.txt");
        let n = normalize_path(&p);
        assert_eq!(n, std::path::PathBuf::from("/workspace/file.txt"));
    }

    #[test]
    fn test_normalize_path_resolves_dot() {
        let p = std::path::PathBuf::from("/workspace/./subdir/./file.txt");
        let n = normalize_path(&p);
        assert_eq!(n, std::path::PathBuf::from("/workspace/subdir/file.txt"));
    }
}
