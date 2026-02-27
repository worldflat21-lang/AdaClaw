//! IDENTITY.md loader and identity builder.

use std::path::Path;
use tracing::debug;

/// Loaded agent identity.
#[derive(Debug, Clone)]
pub struct Identity {
    /// The raw content of `IDENTITY.md`.
    pub content: String,
    /// Parsed agent name (from `**Name:** ...` pattern).
    pub agent_name: Option<String>,
    /// Source path (for diagnostics).
    pub source: String,
}

impl Identity {
    /// Build a default identity (used when IDENTITY.md does not exist).
    pub fn default_identity() -> Self {
        Self {
            content: DEFAULT_IDENTITY.to_string(),
            agent_name: Some("AdaClaw".to_string()),
            source: "<built-in default>".to_string(),
        }
    }

    /// Build the identity section for the agent system prompt.
    /// Returns the content wrapped in a markdown section header.
    pub fn to_prompt_section(&self) -> String {
        if self.content.trim().is_empty() {
            return String::new();
        }
        format!("{}\n\n", self.content.trim())
    }
}

/// Load identity from `workspace_dir/IDENTITY.md`.
/// Falls back to a built-in default if the file does not exist.
pub fn load_identity(workspace_dir: &Path, agent_name_override: Option<&str>) -> Identity {
    let identity_path = workspace_dir.join("IDENTITY.md");

    if identity_path.exists() {
        // Reject symlinks
        if std::fs::symlink_metadata(&identity_path)
            .map(|m| m.file_type().is_symlink())
            .unwrap_or(false)
        {
            tracing::warn!(
                "rejecting symlinked IDENTITY.md at {}",
                identity_path.display()
            );
            return Identity::default_identity();
        }

        match std::fs::read_to_string(&identity_path) {
            Ok(content) => {
                let agent_name = extract_agent_name(&content)
                    .or_else(|| agent_name_override.map(str::to_string));
                debug!(
                    path = %identity_path.display(),
                    agent_name = ?agent_name,
                    "loaded IDENTITY.md"
                );
                Identity {
                    content,
                    agent_name,
                    source: identity_path.display().to_string(),
                }
            }
            Err(e) => {
                tracing::warn!(
                    "failed to read IDENTITY.md at {}: {}",
                    identity_path.display(),
                    e
                );
                let mut id = Identity::default_identity();
                if let Some(name) = agent_name_override {
                    id.agent_name = Some(name.to_string());
                }
                id
            }
        }
    } else {
        debug!(
            "IDENTITY.md not found at {}, using built-in default",
            identity_path.display()
        );
        let mut id = Identity::default_identity();
        if let Some(name) = agent_name_override {
            id.agent_name = Some(name.to_string());
            // Patch the default content with the provided agent name
            id.content = id.content.replace("AdaClaw", name);
        }
        id
    }
}

/// Extract agent name from `**Name:** <name>` pattern in IDENTITY.md.
fn extract_agent_name(content: &str) -> Option<String> {
    for line in content.lines() {
        let trimmed = line.trim();
        // Match "- **Name:** AgentName" or "**Name:** AgentName"
        let value = trimmed
            .strip_prefix("- **Name:**")
            .or_else(|| trimmed.strip_prefix("**Name:**"));
        if let Some(v) = value {
            let name = v.trim().to_string();
            if !name.is_empty() {
                return Some(name);
            }
        }
    }
    None
}

const DEFAULT_IDENTITY: &str = r"# IDENTITY.md

- **Name:** AdaClaw
- **Creature:** A Rust-forged AI agent — fast, lean, and reliable
- **Vibe:** Sharp, direct, resourceful. Helpful without being sycophantic.

You are AdaClaw, an AI assistant. Be helpful, honest, and concise.
";

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn load_from_identity_md() {
        let dir = tempdir().unwrap();
        fs::write(
            dir.path().join("IDENTITY.md"),
            "# IDENTITY.md\n\n- **Name:** Claw\n\nYou are Claw.\n",
        )
        .unwrap();

        let identity = load_identity(dir.path(), None);
        assert_eq!(identity.agent_name.as_deref(), Some("Claw"));
        assert!(identity.content.contains("You are Claw."));
    }

    #[test]
    fn fallback_to_default_when_missing() {
        let dir = tempdir().unwrap();
        let identity = load_identity(dir.path(), None);
        assert_eq!(identity.agent_name.as_deref(), Some("AdaClaw"));
        assert_eq!(identity.source, "<built-in default>");
    }

    #[test]
    fn agent_name_override_patches_default() {
        let dir = tempdir().unwrap();
        let identity = load_identity(dir.path(), Some("MyBot"));
        assert_eq!(identity.agent_name.as_deref(), Some("MyBot"));
        assert!(identity.content.contains("MyBot"));
    }

    #[test]
    fn extract_agent_name_various_formats() {
        assert_eq!(
            extract_agent_name("- **Name:** Claw\n"),
            Some("Claw".to_string())
        );
        assert_eq!(
            extract_agent_name("**Name:** AdaClaw\n"),
            Some("AdaClaw".to_string())
        );
        assert_eq!(extract_agent_name("No name here\n"), None);
    }

    #[test]
    fn to_prompt_section_not_empty() {
        let identity = Identity::default_identity();
        let section = identity.to_prompt_section();
        assert!(!section.is_empty());
        assert!(section.contains("AdaClaw"));
    }
}
