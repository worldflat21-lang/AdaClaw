use adaclaw_core::tool::{Tool, ToolResult, ToolSpec};
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use serde_json::Value;
use std::path::{Path, PathBuf};
use tokio::process::Command;

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
        let blocked = ["C:\\Windows", "C:\\Program Files", "C:\\Program Files (x86)"];
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

        let result = tokio::time::timeout(
            std::time::Duration::from_secs(timeout_secs),
            cmd.output(),
        )
        .await;

        match result {
            Ok(Ok(output)) => {
                let stdout = String::from_utf8_lossy(&output.stdout).to_string();
                let stderr = String::from_utf8_lossy(&output.stderr).to_string();

                let combined = if stderr.is_empty() {
                    stdout.clone()
                } else if stdout.is_empty() {
                    format!("[stderr] {}", stderr)
                } else {
                    format!("{}\n[stderr] {}", stdout, stderr)
                };

                Ok(ToolResult {
                    success: output.status.success(),
                    output: combined,
                    error: if output.status.success() {
                        None
                    } else {
                        Some(format!(
                            "Exit code: {}",
                            output.status.code().unwrap_or(-1)
                        ))
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
