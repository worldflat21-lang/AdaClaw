//! SKILL.md / SKILL.toml loader.
//!
//! Security rules (inspired by zeroclaw):
//! - Symlinks inside the skills directory are rejected.
//! - No executable content is loaded — skills are prompt text only.
//! - Skill names must contain only `[a-z0-9_-]` characters.
//! - Total injected prompt size is capped at `MAX_SKILLS_PROMPT_BYTES`.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use tracing::{debug, warn};

const MAX_SKILLS_PROMPT_BYTES: usize = 32 * 1024; // 32 KB

/// A loaded skill, ready to be injected into the system prompt.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Skill {
    pub name: String,
    pub description: String,
    #[serde(default = "default_version")]
    pub version: String,
    #[serde(default)]
    pub author: Option<String>,
    #[serde(default)]
    pub tags: Vec<String>,
    /// Full text of the SKILL.md instructions.
    #[serde(skip)]
    pub instructions: String,
    /// Path to the skill directory.
    #[serde(skip)]
    pub location: Option<PathBuf>,
}

fn default_version() -> String {
    "0.1.0".to_string()
}

/// SKILL.toml manifest (optional structured metadata).
#[derive(Debug, Deserialize)]
struct SkillManifest {
    skill: SkillMeta,
}

#[derive(Debug, Deserialize)]
struct SkillMeta {
    name: String,
    description: String,
    #[serde(default = "default_version")]
    version: String,
    #[serde(default)]
    author: Option<String>,
    #[serde(default)]
    tags: Vec<String>,
}

/// Load all skills from `workspace_dir/skills/`.
/// Returns an empty Vec if the directory does not exist.
pub fn load_skills(workspace_dir: &Path) -> Vec<Skill> {
    let skills_dir = workspace_dir.join("skills");
    if !skills_dir.exists() {
        debug!("skills directory not found: {}", skills_dir.display());
        return Vec::new();
    }

    let mut skills = Vec::new();

    let entries = match std::fs::read_dir(&skills_dir) {
        Ok(e) => e,
        Err(err) => {
            warn!(
                "failed to read skills directory {}: {}",
                skills_dir.display(),
                err
            );
            return Vec::new();
        }
    };

    for entry in entries.flatten() {
        let path = entry.path();

        // Only process directories
        if !path.is_dir() {
            continue;
        }

        // Reject symlinks
        if is_symlink(&path) {
            warn!("rejecting symlinked skill directory: {}", path.display());
            continue;
        }

        // Validate skill directory name
        let dir_name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if !is_valid_skill_name(dir_name) {
            warn!(
                "rejecting skill with invalid directory name: {:?}",
                dir_name
            );
            continue;
        }

        if let Some(skill) = load_skill_from_dir(&path, dir_name) {
            debug!("loaded skill: {} from {}", skill.name, path.display());
            skills.push(skill);
        }
    }

    // Sort by name for deterministic order
    skills.sort_by(|a, b| a.name.cmp(&b.name));
    skills
}

fn load_skill_from_dir(dir: &Path, dir_name: &str) -> Option<Skill> {
    let toml_path = dir.join("SKILL.toml");
    let md_path = dir.join("SKILL.md");

    // Read instructions from SKILL.md (may be absent when only SKILL.toml exists)
    let instructions = if md_path.exists() {
        if is_symlink(&md_path) {
            warn!("rejecting symlinked SKILL.md in {}", dir.display());
            return None;
        }
        match std::fs::read_to_string(&md_path) {
            Ok(s) => s,
            Err(e) => {
                warn!("failed to read SKILL.md in {}: {}", dir.display(), e);
                return None;
            }
        }
    } else {
        String::new()
    };

    // If SKILL.toml exists, use it for structured metadata
    if toml_path.exists() {
        if is_symlink(&toml_path) {
            warn!("rejecting symlinked SKILL.toml in {}", dir.display());
            return None;
        }
        let toml_str = match std::fs::read_to_string(&toml_path) {
            Ok(s) => s,
            Err(e) => {
                warn!("failed to read SKILL.toml in {}: {}", dir.display(), e);
                return None;
            }
        };
        match toml::from_str::<SkillManifest>(&toml_str) {
            Ok(manifest) => {
                return Some(Skill {
                    name: manifest.skill.name,
                    description: manifest.skill.description,
                    version: manifest.skill.version,
                    author: manifest.skill.author,
                    tags: manifest.skill.tags,
                    instructions,
                    location: Some(dir.to_path_buf()),
                });
            }
            Err(e) => {
                warn!("failed to parse SKILL.toml in {}: {}", dir.display(), e);
                // Fall through to SKILL.md-only load
            }
        }
    }

    // No SKILL.toml — require SKILL.md
    if instructions.is_empty() {
        return None;
    }

    // Extract description from first non-heading line of SKILL.md
    let description = extract_description(&instructions);

    Some(Skill {
        name: dir_name.to_string(),
        description,
        version: "0.1.0".to_string(),
        author: None,
        tags: Vec::new(),
        instructions,
        location: Some(dir.to_path_buf()),
    })
}

fn extract_description(content: &str) -> String {
    content
        .lines()
        .find(|line| {
            let trimmed = line.trim();
            !trimmed.is_empty() && !trimmed.starts_with('#')
        })
        .unwrap_or("No description")
        .trim()
        .chars()
        .take(200)
        .collect()
}

fn is_valid_skill_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

fn is_symlink(path: &Path) -> bool {
    std::fs::symlink_metadata(path)
        .map(|m| m.file_type().is_symlink())
        .unwrap_or(false)
}

// ── Prompt injection ──────────────────────────────────────────────────────────

/// Build the "Available Skills" section for the agent system prompt.
/// Returns an empty string if there are no skills.
pub fn skills_to_prompt(skills: &[Skill]) -> String {
    if skills.is_empty() {
        return String::new();
    }

    let mut out = String::from(
        "## Available Skills\n\n\
         The following skills are available. Follow their instructions directly.\n\n\
         <available_skills>\n",
    );

    let mut total_bytes = out.len();

    for skill in skills {
        let entry = format_skill_entry(skill);
        if total_bytes + entry.len() > MAX_SKILLS_PROMPT_BYTES {
            warn!(
                "skills prompt size limit reached, skipping skill '{}' and subsequent skills",
                skill.name
            );
            break;
        }
        out.push_str(&entry);
        total_bytes += entry.len();
    }

    out.push_str("</available_skills>\n");
    out
}

fn format_skill_entry(skill: &Skill) -> String {
    let mut entry = String::new();
    entry.push_str("  <skill>\n");
    entry.push_str(&format!("    <name>{}</name>\n", xml_escape(&skill.name)));
    entry.push_str(&format!(
        "    <description>{}</description>\n",
        xml_escape(&skill.description)
    ));

    if !skill.instructions.is_empty() {
        entry.push_str("    <instructions>\n");
        entry.push_str(&format!(
            "      <instruction>{}</instruction>\n",
            xml_escape(&skill.instructions)
        ));
        entry.push_str("    </instructions>\n");
    }

    entry.push_str("  </skill>\n");
    entry
}

fn xml_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn load_skill_from_md() {
        let dir = tempdir().unwrap();
        let skill_dir = dir.path().join("skills").join("weather");
        fs::create_dir_all(&skill_dir).unwrap();
        fs::write(
            skill_dir.join("SKILL.md"),
            "# Weather Skill\nFetch current weather for a location.\n",
        )
        .unwrap();

        let skills = load_skills(dir.path());
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].name, "weather");
        assert!(skills[0].description.contains("weather"));
    }

    #[test]
    fn load_skill_from_toml() {
        let dir = tempdir().unwrap();
        let skill_dir = dir.path().join("skills").join("github");
        fs::create_dir_all(&skill_dir).unwrap();
        fs::write(
            skill_dir.join("SKILL.toml"),
            "[skill]\nname = \"github\"\ndescription = \"GitHub integration\"\n",
        )
        .unwrap();
        fs::write(skill_dir.join("SKILL.md"), "Use GitHub API.\n").unwrap();

        let skills = load_skills(dir.path());
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].name, "github");
        assert_eq!(skills[0].description, "GitHub integration");
    }

    #[test]
    fn empty_directory_returns_no_skills() {
        let dir = tempdir().unwrap();
        let skills = load_skills(dir.path());
        assert!(skills.is_empty());
    }

    #[test]
    fn skills_to_prompt_empty() {
        assert!(skills_to_prompt(&[]).is_empty());
    }

    #[test]
    fn skills_to_prompt_contains_xml_structure() {
        let skills = vec![Skill {
            name: "test".into(),
            description: "A test skill".into(),
            version: "1.0.0".into(),
            author: None,
            tags: vec![],
            instructions: "Do the thing.".into(),
            location: None,
        }];
        let prompt = skills_to_prompt(&skills);
        assert!(prompt.contains("<available_skills>"));
        assert!(prompt.contains("<name>test</name>"));
        assert!(prompt.contains("<description>A test skill</description>"));
        assert!(prompt.contains("Do the thing."));
    }

    #[test]
    fn xml_escaping_works() {
        let skills = vec![Skill {
            name: "test".into(),
            description: "A & B <test>".into(),
            version: "1.0.0".into(),
            author: None,
            tags: vec![],
            instructions: String::new(),
            location: None,
        }];
        let prompt = skills_to_prompt(&skills);
        assert!(prompt.contains("A &amp; B &lt;test&gt;"));
    }

    #[test]
    fn invalid_skill_name_rejected() {
        assert!(!is_valid_skill_name("../escape"));
        assert!(!is_valid_skill_name("has space"));
        assert!(!is_valid_skill_name(""));
        assert!(is_valid_skill_name("weather"));
        assert!(is_valid_skill_name("my-skill"));
        assert!(is_valid_skill_name("my_skill_v2"));
    }
}
