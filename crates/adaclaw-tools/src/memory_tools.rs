use adaclaw_core::memory::{Category, Memory, RecallScope};
use adaclaw_core::tool::{Tool, ToolResult, ToolSpec};
use anyhow::Result;
use async_trait::async_trait;
use serde_json::Value;
use std::sync::Arc;

// ── helpers ───────────────────────────────────────────────────────────────────

fn parse_category(s: &str) -> Category {
    match s {
        "Core" | "core" => Category::Core,
        "Daily" | "daily" => Category::Daily,
        "Global" | "global" => Category::Global,
        "Conversation" | "conversation" => Category::Conversation,
        other => Category::Custom(other.to_string()),
    }
}

fn category_label(cat: &Category) -> &'static str {
    match cat {
        Category::Core => "Core",
        Category::Daily => "Daily",
        Category::Global => "Global",
        Category::Conversation => "Conversation",
        Category::Custom(_) => "Custom",
    }
}

// ── MemoryStoreTool ───────────────────────────────────────────────────────────

/// Tool that stores a key-value pair into the long-term memory backend.
///
/// Parameters:
///   - `key`      (required): unique identifier for the memory entry.
///   - `content`  (required): text content to store.
///   - `category` (optional): "Core" (default) | "Daily" | "Global" | "Conversation".
///
/// When `category` is "Global" the entry is visible to ALL agents.
/// Defaults to "Core" (long-lived, high-priority facts).
pub struct MemoryStoreTool {
    memory: Option<Arc<dyn Memory>>,
}

impl Default for MemoryStoreTool {
    fn default() -> Self {
        Self::new()
    }
}

impl MemoryStoreTool {
    /// Create without a memory backend (no-op mode).
    pub fn new() -> Self {
        Self { memory: None }
    }

    /// Create with an injected memory backend.
    pub fn with_memory(memory: Arc<dyn Memory>) -> Self {
        Self { memory: Some(memory) }
    }
}

#[async_trait]
impl Tool for MemoryStoreTool {
    fn name(&self) -> &str {
        "memory_store"
    }

    fn description(&self) -> &str {
        "Store a fact or note into long-term memory. \
         Use `category=Core` for important persistent facts (default), \
         `category=Daily` for working notes, \
         `category=Global` for facts shared across all agents."
    }

    fn parameters_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "key": {
                    "type": "string",
                    "description": "Unique identifier for this memory entry (e.g. 'user:name', 'project:stack')."
                },
                "content": {
                    "type": "string",
                    "description": "The text content to remember."
                },
                "category": {
                    "type": "string",
                    "enum": ["Core", "Daily", "Global"],
                    "description": "Memory category. Defaults to 'Core'."
                }
            },
            "required": ["key", "content"]
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
        let key = match args["key"].as_str() {
            Some(k) if !k.is_empty() => k,
            _ => {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some("memory_store: 'key' is required and must be a non-empty string".to_string()),
                });
            }
        };

        let content = match args["content"].as_str() {
            Some(c) if !c.is_empty() => c,
            _ => {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some("memory_store: 'content' is required and must be a non-empty string".to_string()),
                });
            }
        };

        let category = args["category"]
            .as_str()
            .map(parse_category)
            .unwrap_or(Category::Core);

        let memory = match &self.memory {
            Some(m) => m,
            None => {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some("memory_store: no memory backend configured".to_string()),
                });
            }
        };

        match memory.store(key, content, category.clone(), None, None).await {
            Ok(()) => Ok(ToolResult {
                success: true,
                output: format!(
                    "✓ Stored memory [{}] '{}' ({} bytes, category={})",
                    category_label(&category),
                    key,
                    content.len(),
                    category_label(&category)
                ),
                error: None,
            }),
            Err(e) => Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!("memory_store: failed to store '{}': {}", key, e)),
            }),
        }
    }
}

// ── MemoryRecallTool ──────────────────────────────────────────────────────────

/// Tool that retrieves relevant memory entries by semantic/keyword search.
///
/// Parameters:
///   - `query` (required): the search query.
///   - `limit` (optional): max number of results to return (default 5, max 20).
pub struct MemoryRecallTool {
    memory: Option<Arc<dyn Memory>>,
}

impl Default for MemoryRecallTool {
    fn default() -> Self {
        Self::new()
    }
}

impl MemoryRecallTool {
    pub fn new() -> Self {
        Self { memory: None }
    }

    pub fn with_memory(memory: Arc<dyn Memory>) -> Self {
        Self { memory: Some(memory) }
    }
}

#[async_trait]
impl Tool for MemoryRecallTool {
    fn name(&self) -> &str {
        "memory_recall"
    }

    fn description(&self) -> &str {
        "Search long-term memory for relevant facts. \
         Returns the most relevant stored entries matching the query."
    }

    fn parameters_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "Keywords or natural-language description of what to recall."
                },
                "limit": {
                    "type": "integer",
                    "description": "Maximum number of results (default 5, max 20).",
                    "minimum": 1,
                    "maximum": 20
                }
            },
            "required": ["query"]
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
        let query = match args["query"].as_str() {
            Some(q) if !q.is_empty() => q,
            _ => {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some("memory_recall: 'query' is required and must be a non-empty string".to_string()),
                });
            }
        };

        let limit = args["limit"]
            .as_u64()
            .map(|n| (n as usize).min(20))
            .unwrap_or(5);

        let memory = match &self.memory {
            Some(m) => m,
            None => {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some("memory_recall: no memory backend configured".to_string()),
                });
            }
        };

        match memory.recall(query, limit, None, RecallScope::Full).await {
            Ok(entries) if entries.is_empty() => Ok(ToolResult {
                success: true,
                output: "No memory entries found matching your query.".to_string(),
                error: None,
            }),
            Ok(entries) => {
                let mut lines = vec![format!("Found {} memory entries:", entries.len())];
                for (i, entry) in entries.iter().enumerate() {
                    lines.push(format!(
                        "\n[{}] key={} category={}\n{}",
                        i + 1,
                        entry.key,
                        category_label(&entry.category),
                        entry.content
                    ));
                }
                Ok(ToolResult {
                    success: true,
                    output: lines.join("\n"),
                    error: None,
                })
            }
            Err(e) => Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!("memory_recall: search failed: {}", e)),
            }),
        }
    }
}

// ── MemoryForgetTool ──────────────────────────────────────────────────────────

/// Tool that deletes a memory entry by key.
///
/// Parameters:
///   - `key` (required): the exact key of the entry to delete.
pub struct MemoryForgetTool {
    memory: Option<Arc<dyn Memory>>,
}

impl Default for MemoryForgetTool {
    fn default() -> Self {
        Self::new()
    }
}

impl MemoryForgetTool {
    pub fn new() -> Self {
        Self { memory: None }
    }

    pub fn with_memory(memory: Arc<dyn Memory>) -> Self {
        Self { memory: Some(memory) }
    }
}

#[async_trait]
impl Tool for MemoryForgetTool {
    fn name(&self) -> &str {
        "memory_forget"
    }

    fn description(&self) -> &str {
        "Delete a specific memory entry by its exact key. \
         Use memory_recall first to find the key you want to remove."
    }

    fn parameters_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "key": {
                    "type": "string",
                    "description": "The exact key of the memory entry to delete."
                }
            },
            "required": ["key"]
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
        let key = match args["key"].as_str() {
            Some(k) if !k.is_empty() => k,
            _ => {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some("memory_forget: 'key' is required and must be a non-empty string".to_string()),
                });
            }
        };

        let memory = match &self.memory {
            Some(m) => m,
            None => {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some("memory_forget: no memory backend configured".to_string()),
                });
            }
        };

        match memory.forget(key).await {
            Ok(true) => Ok(ToolResult {
                success: true,
                output: format!("✓ Memory entry '{}' deleted.", key),
                error: None,
            }),
            Ok(false) => Ok(ToolResult {
                success: true,
                output: format!("Memory entry '{}' not found (may have already been deleted).", key),
                error: None,
            }),
            Err(e) => Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!("memory_forget: failed to delete '{}': {}", key, e)),
            }),
        }
    }
}
