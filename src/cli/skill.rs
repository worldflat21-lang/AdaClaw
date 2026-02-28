//! `adaclaw skill` 子命令 — 技能市场对接（ClawHub）
//!
//! ## 命令
//! - `adaclaw skill list`              — 列出已安装技能
//! - `adaclaw skill install <url>`     — 从 URL 或 `clawhub:<name>` 安装
//! - `adaclaw skill remove <name>`     — 删除已安装技能
//! - `adaclaw skill audit <name>`      — 运行安全审计
//!
//! ## 安全原则（对标 zeroclaw `skills audit`）
//! - 拒绝符号链接
//! - 检测 script 注入模式（如 `<script>`, `eval(`, `exec(` 等）
//! - 技能名称只允许 `[a-z0-9_-]`

use anyhow::{anyhow, Context, Result};
use std::path::{Path, PathBuf};
use tracing::{info, warn};

const CLAWHUB_BASE_URL: &str = "https://clawhub.ai/skills";
/// 注入模式检测（安全审计用）
const INJECTION_PATTERNS: &[&str] = &[
    "<script",
    "</script>",
    "javascript:",
    "eval(",
    "exec(",
    "__import__",
    "subprocess",
    "os.system",
    "shell_exec",
    "system(",
    "`rm ",
    "curl | bash",
    "wget -O- |",
];

// ── 公共函数 ──────────────────────────────────────────────────────────────────

/// 获取技能目录（workspace/skills/，不存在时自动创建）
pub fn get_skills_dir() -> PathBuf {
    let workspace = std::env::var("ADACLAW_WORKSPACE")
        .unwrap_or_else(|_| "workspace".to_string());
    PathBuf::from(&workspace).join("skills")
}

// ── list ──────────────────────────────────────────────────────────────────────

/// 列出所有已安装技能
pub fn cmd_list() {
    let skills_dir = get_skills_dir();

    if !skills_dir.exists() {
        println!("No skills installed. Skills directory: {}", skills_dir.display());
        println!("Install a skill with: adaclaw skill install <url-or-name>");
        return;
    }

    let entries: Vec<_> = std::fs::read_dir(&skills_dir)
        .map(|rd| rd.flatten().collect())
        .unwrap_or_default();

    let dirs: Vec<_> = entries
        .iter()
        .filter(|e| e.path().is_dir() && !is_symlink(&e.path()))
        .collect();

    if dirs.is_empty() {
        println!("No skills installed.");
        println!("Install a skill with: adaclaw skill install <url-or-name>");
        return;
    }

    println!("Installed skills ({}):", dirs.len());
    println!("{:<20} {:<12} Description", "Name", "Version");
    println!("{}", "-".repeat(60));

    for entry in dirs {
        let path = entry.path();
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("?")
            .to_string();

        // 尝试从 SKILL.toml 读取元数据
        let (version, description) = read_skill_meta(&path);
        println!("{:<20} {:<12} {}", name, version, description);
    }
}

fn read_skill_meta(dir: &Path) -> (String, String) {
    let toml_path = dir.join("SKILL.toml");
    if toml_path.exists() {
        if let Ok(content) = std::fs::read_to_string(&toml_path) {
            if let Ok(val) = toml::from_str::<toml::Value>(&content) {
                let version = val
                    .get("skill")
                    .and_then(|s| s.get("version"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("0.1.0")
                    .to_string();
                let description = val
                    .get("skill")
                    .and_then(|s| s.get("description"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .chars()
                    .take(40)
                    .collect::<String>();
                return (version, description);
            }
        }
    }
    // fallback: read first line of SKILL.md
    let md_path = dir.join("SKILL.md");
    let description = if md_path.exists() {
        std::fs::read_to_string(&md_path)
            .ok()
            .and_then(|s| {
                s.lines()
                    .find(|l| {
                        let t = l.trim();
                        !t.is_empty() && !t.starts_with('#')
                    })
                    .map(|l| l.trim().chars().take(40).collect())
            })
            .unwrap_or_default()
    } else {
        String::new()
    };

    ("0.1.0".to_string(), description)
}

// ── install ───────────────────────────────────────────────────────────────────

/// 安装技能（支持 URL 或 `clawhub:<name>` 短前缀）
pub async fn cmd_install(source: &str) -> Result<()> {
    let skills_dir = get_skills_dir();
    std::fs::create_dir_all(&skills_dir)
        .context("Failed to create skills directory")?;

    // 解析来源
    let (skill_name, url) = if let Some(name) = source.strip_prefix("clawhub:") {
        let name = name.to_string();
        let url = format!("{}/{}/SKILL.md", CLAWHUB_BASE_URL, name);
        (name, url)
    } else if source.starts_with("https://") || source.starts_with("http://") {
        // 从 URL 推断技能名
        let name = source
            .trim_end_matches('/')
            .rsplit('/')
            .next()
            .unwrap_or("unknown-skill")
            .trim_end_matches(".md")
            .to_string();
        let url = if source.ends_with(".md") {
            source.to_string()
        } else {
            format!("{}/SKILL.md", source.trim_end_matches('/'))
        };
        (name, url)
    } else {
        // 本地路径（绝对路径或相对路径）
        let path = PathBuf::from(source);
        if path.exists() {
            return install_from_local_path(source, &skills_dir);
        }
        // 否则当作 ClawHub 名字
        let url = format!("{}/{}/SKILL.md", CLAWHUB_BASE_URL, source);
        (source.to_string(), url)
    };

    // 验证技能名
    if !is_valid_skill_name(&skill_name) {
        return Err(anyhow!(
            "Invalid skill name '{}': only [a-z0-9_-] are allowed",
            skill_name
        ));
    }

    let skill_dir = skills_dir.join(&skill_name);
    if skill_dir.exists() {
        println!("Skill '{}' is already installed.", skill_name);
        println!("Remove it first with: adaclaw skill remove {}", skill_name);
        return Ok(());
    }

    println!("Installing skill '{}' from {}...", skill_name, url);

    // 下载 SKILL.md
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .user_agent("adaclaw/0.1.0")
        .build()?;

    let resp = client
        .get(&url)
        .send()
        .await
        .with_context(|| format!("Failed to download skill from {}", url))?;

    if !resp.status().is_success() {
        return Err(anyhow!(
            "Skill download failed: HTTP {} from {}",
            resp.status(),
            url
        ));
    }

    let content = resp
        .text()
        .await
        .context("Failed to read skill content")?;

    // 安全审计
    let issues = audit_content(&skill_name, &content);
    if !issues.is_empty() {
        eprintln!("⚠️  Security audit failed for '{}':", skill_name);
        for issue in &issues {
            eprintln!("   - {}", issue);
        }
        return Err(anyhow!("Skill '{}' failed security audit", skill_name));
    }

    // 写入文件
    std::fs::create_dir_all(&skill_dir)?;
    let md_path = skill_dir.join("SKILL.md");
    std::fs::write(&md_path, &content)
        .with_context(|| format!("Failed to write {}", md_path.display()))?;

    println!("✅ Skill '{}' installed successfully.", skill_name);
    info!(skill = %skill_name, source = %url, "Skill installed");
    Ok(())
}

fn install_from_local_path(src: &str, skills_dir: &Path) -> Result<()> {
    let src_path = PathBuf::from(src);

    if is_symlink(&src_path) {
        return Err(anyhow!("Refusing to install from symlink: {}", src));
    }

    let name = src_path
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| anyhow!("Cannot determine skill name from path: {}", src))?
        .to_string();

    if !is_valid_skill_name(&name) {
        return Err(anyhow!(
            "Invalid skill directory name '{}': only [a-z0-9_-] allowed",
            name
        ));
    }

    let dest = skills_dir.join(&name);
    if dest.exists() {
        return Err(anyhow!("Skill '{}' already installed", name));
    }

    // Copy the directory
    copy_dir(&src_path, &dest)
        .with_context(|| format!("Failed to copy skill from {}", src))?;

    println!("✅ Skill '{}' installed from local path.", name);
    Ok(())
}

fn copy_dir(src: &Path, dst: &Path) -> Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)?.flatten() {
        let path = entry.path();
        if is_symlink(&path) {
            warn!("Skipping symlink in skill source: {}", path.display());
            continue;
        }
        let dst_path = dst.join(entry.file_name());
        if path.is_dir() {
            copy_dir(&path, &dst_path)?;
        } else {
            std::fs::copy(&path, &dst_path)?;
        }
    }
    Ok(())
}

// ── remove ────────────────────────────────────────────────────────────────────

/// 删除已安装的技能
pub fn cmd_remove(name: &str) -> Result<()> {
    if !is_valid_skill_name(name) {
        return Err(anyhow!("Invalid skill name '{}'", name));
    }

    let skill_dir = get_skills_dir().join(name);

    if !skill_dir.exists() {
        println!("Skill '{}' is not installed.", name);
        return Ok(());
    }

    if is_symlink(&skill_dir) {
        return Err(anyhow!("Refusing to remove symlinked skill directory"));
    }

    // 安全确认
    print!("Remove skill '{}'? This cannot be undone. [y/N] ", name);
    use std::io::Write;
    std::io::stdout().flush()?;
    let mut input = String::new();
    std::io::stdin().read_line(&mut input)?;

    if input.trim().to_lowercase() != "y" {
        println!("Cancelled.");
        return Ok(());
    }

    std::fs::remove_dir_all(&skill_dir)
        .with_context(|| format!("Failed to remove skill directory: {}", skill_dir.display()))?;

    println!("✅ Skill '{}' removed.", name);
    info!(skill = %name, "Skill removed");
    Ok(())
}

// ── audit ─────────────────────────────────────────────────────────────────────

/// 对已安装技能运行安全审计
pub fn cmd_audit(name: &str) -> Result<()> {
    if !is_valid_skill_name(name) {
        return Err(anyhow!("Invalid skill name '{}'", name));
    }

    let skill_dir = get_skills_dir().join(name);
    if !skill_dir.exists() {
        return Err(anyhow!("Skill '{}' is not installed", name));
    }

    println!("Auditing skill '{}'...", name);

    let mut all_issues: Vec<String> = Vec::new();

    // 检查符号链接
    if is_symlink(&skill_dir) {
        all_issues.push("Skill directory is a symlink (rejected)".to_string());
    }

    // 遍历文件并检查内容
    if let Ok(entries) = std::fs::read_dir(&skill_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if is_symlink(&path) {
                all_issues.push(format!(
                    "File '{}' is a symlink (rejected)",
                    path.file_name().unwrap_or_default().to_string_lossy()
                ));
                continue;
            }
            if path.is_file() {
                if let Ok(content) = std::fs::read_to_string(&path) {
                    let file_issues = audit_content(
                        &path
                            .file_name()
                            .unwrap_or_default()
                            .to_string_lossy(),
                        &content,
                    );
                    all_issues.extend(file_issues);
                }
            }
        }
    }

    if all_issues.is_empty() {
        println!("✅ Skill '{}' passed security audit.", name);
    } else {
        println!("⚠️  Skill '{}' has {} security issue(s):", name, all_issues.len());
        for issue in &all_issues {
            println!("   - {}", issue);
        }
    }

    Ok(())
}

// ── 安全审计辅助函数 ──────────────────────────────────────────────────────────

fn audit_content(name: &str, content: &str) -> Vec<String> {
    let mut issues = Vec::new();
    let content_lower = content.to_lowercase();

    for pattern in INJECTION_PATTERNS {
        if content_lower.contains(pattern) {
            issues.push(format!(
                "'{}': contains suspicious pattern '{}'",
                name, pattern
            ));
        }
    }

    // 检查是否有隐藏的 Unicode 方向控制字符（Trojan Source attack）
    if content.chars().any(|c| {
        matches!(
            c,
            '\u{202A}'
                | '\u{202B}'
                | '\u{202C}'
                | '\u{202D}'
                | '\u{202E}'
                | '\u{2066}'
                | '\u{2067}'
                | '\u{2068}'
                | '\u{2069}'
                | '\u{200F}'
        )
    }) {
        issues.push(format!(
            "'{}': contains bidirectional Unicode control characters",
            name
        ));
    }

    issues
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_valid_skill_names() {
        assert!(is_valid_skill_name("weather"));
        assert!(is_valid_skill_name("my-skill"));
        assert!(is_valid_skill_name("my_skill_v2"));
        assert!(!is_valid_skill_name("../escape"));
        assert!(!is_valid_skill_name("has space"));
        assert!(!is_valid_skill_name(""));
    }

    #[test]
    fn test_injection_patterns_detected() {
        let issues = audit_content("test", "<script>alert('xss')</script>");
        assert!(!issues.is_empty());
    }

    #[test]
    fn test_clean_content_passes() {
        let clean = "# Weather Skill\n\nFetch current weather for a location.\n\nUse the weather API to get current conditions.";
        let issues = audit_content("weather", clean);
        assert!(issues.is_empty());
    }
}
