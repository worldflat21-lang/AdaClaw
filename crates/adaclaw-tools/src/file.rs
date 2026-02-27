use crate::shell::{safe_path, workspace_root};
use adaclaw_core::tool::{Tool, ToolResult, ToolSpec};
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use serde_json::Value;
use std::path::PathBuf;

// ── FileReadTool ──────────────────────────────────────────────────────────────

pub struct FileReadTool {
    workspace: PathBuf,
}

impl Default for FileReadTool {
    fn default() -> Self {
        Self::new()
    }
}

impl FileReadTool {
    pub fn new() -> Self {
        Self {
            workspace: workspace_root(),
        }
    }
}

#[async_trait]
impl Tool for FileReadTool {
    fn name(&self) -> &str {
        "file_read"
    }

    fn description(&self) -> &str {
        "Read the content of a file within the workspace. Path is relative to workspace root."
    }

    fn parameters_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "File path relative to workspace root"
                }
            },
            "required": ["path"]
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
        let path_str = args["path"]
            .as_str()
            .ok_or_else(|| anyhow!("Missing 'path' argument"))?;

        let abs_path = match safe_path(&self.workspace, path_str) {
            Ok(p) => p,
            Err(e) => {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(e.to_string()),
                });
            }
        };

        match tokio::fs::read_to_string(&abs_path).await {
            Ok(content) => Ok(ToolResult {
                success: true,
                output: content,
                error: None,
            }),
            Err(e) => Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!("Failed to read '{}': {}", path_str, e)),
            }),
        }
    }
}

// ── FileWriteTool ─────────────────────────────────────────────────────────────

pub struct FileWriteTool {
    workspace: PathBuf,
}

impl Default for FileWriteTool {
    fn default() -> Self {
        Self::new()
    }
}

impl FileWriteTool {
    pub fn new() -> Self {
        Self {
            workspace: workspace_root(),
        }
    }
}

#[async_trait]
impl Tool for FileWriteTool {
    fn name(&self) -> &str {
        "file_write"
    }

    fn description(&self) -> &str {
        "Write content to a file within the workspace. Creates parent directories as needed. Path is relative to workspace root."
    }

    fn parameters_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "File path relative to workspace root"
                },
                "content": {
                    "type": "string",
                    "description": "Content to write to the file"
                },
                "append": {
                    "type": "boolean",
                    "description": "Append to file instead of overwriting (default: false)",
                    "default": false
                }
            },
            "required": ["path", "content"]
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
        let path_str = args["path"]
            .as_str()
            .ok_or_else(|| anyhow!("Missing 'path' argument"))?;
        let content = args["content"]
            .as_str()
            .ok_or_else(|| anyhow!("Missing 'content' argument"))?;
        let append = args["append"].as_bool().unwrap_or(false);

        let abs_path = match safe_path(&self.workspace, path_str) {
            Ok(p) => p,
            Err(e) => {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(e.to_string()),
                });
            }
        };

        // Create parent directories if they don't exist
        if let Some(parent) = abs_path.parent() {
            if let Err(e) = tokio::fs::create_dir_all(parent).await {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(format!("Failed to create directories: {}", e)),
                });
            }
        }

        let result = if append {
            use tokio::io::AsyncWriteExt;
            let mut file = tokio::fs::OpenOptions::new()
                .append(true)
                .create(true)
                .open(&abs_path)
                .await?;
            file.write_all(content.as_bytes()).await
        } else {
            tokio::fs::write(&abs_path, content.as_bytes()).await
        };

        match result {
            Ok(_) => Ok(ToolResult {
                success: true,
                output: format!("Written {} bytes to '{}'", content.len(), path_str),
                error: None,
            }),
            Err(e) => Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!("Failed to write '{}': {}", path_str, e)),
            }),
        }
    }
}

// ── FileListTool ──────────────────────────────────────────────────────────────

pub struct FileListTool {
    workspace: PathBuf,
}

impl Default for FileListTool {
    fn default() -> Self {
        Self::new()
    }
}

impl FileListTool {
    pub fn new() -> Self {
        Self {
            workspace: workspace_root(),
        }
    }
}

#[async_trait]
impl Tool for FileListTool {
    fn name(&self) -> &str {
        "file_list"
    }

    fn description(&self) -> &str {
        "List files and directories within a workspace path. Path is relative to workspace root."
    }

    fn parameters_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Directory path relative to workspace root (default: '.')",
                    "default": "."
                }
            }
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
        let path_str = args["path"].as_str().unwrap_or(".");

        let abs_path = match safe_path(&self.workspace, path_str) {
            Ok(p) => p,
            Err(e) => {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(e.to_string()),
                });
            }
        };

        let mut read_dir = match tokio::fs::read_dir(&abs_path).await {
            Ok(rd) => rd,
            Err(e) => {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(format!("Failed to list '{}': {}", path_str, e)),
                });
            }
        };

        let mut entries: Vec<String> = Vec::new();
        while let Ok(Some(entry)) = read_dir.next_entry().await {
            let name = entry.file_name().to_string_lossy().to_string();
            let metadata = entry.metadata().await;
            let suffix = match metadata {
                Ok(m) if m.is_dir() => "/",
                Ok(m) if m.is_symlink() => "@",
                _ => "",
            };
            entries.push(format!("{}{}", name, suffix));
        }

        entries.sort();
        Ok(ToolResult {
            success: true,
            output: entries.join("\n"),
            error: None,
        })
    }
}
