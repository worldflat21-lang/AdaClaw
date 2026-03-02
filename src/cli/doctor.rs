//! `adaclaw doctor` — system diagnostic checks.
//!
//! Checks every subsystem and reports status as ✅ / ⚠️ / ❌.

use crate::config::Config;
use adaclaw_security::approval::AutonomyLevel;
use adaclaw_security::sandbox::docker::ContainerEnvironment;

/// Run all diagnostic checks and print a structured report.
pub async fn run_doctor() {
    println!("AdaClaw Doctor\n==============\n");

    let mut checks_passed = 0usize;
    let mut checks_warned = 0usize;
    let mut checks_failed = 0usize;

    let cfg = Config::load();

    // ── Config file ───────────────────────────────────────────────────────────
    if std::path::Path::new("config.toml").exists() {
        ok("config.toml found");
        checks_passed += 1;
    } else {
        warn("config.toml not found — running with defaults + env vars");
        checks_warned += 1;
    }

    // ── Providers ─────────────────────────────────────────────────────────────
    if cfg.providers.is_empty() {
        let openai_key = std::env::var("OPENAI_API_KEY")
            .or_else(|_| std::env::var("ADACLAW_OPENAI_API_KEY"))
            .is_ok();
        let anthropic_key = std::env::var("ANTHROPIC_API_KEY").is_ok();
        let openrouter_key = std::env::var("OPENROUTER_API_KEY").is_ok();

        if openai_key || anthropic_key || openrouter_key {
            ok("Provider API key detected via environment variable");
            checks_passed += 1;
        } else {
            fail("No providers configured and no API key env vars found");
            println!("    Set OPENAI_API_KEY, ANTHROPIC_API_KEY, or OPENROUTER_API_KEY");
            println!("    Or add a [providers] section to config.toml");
            checks_failed += 1;
        }
    } else {
        for (name, pcfg) in &cfg.providers {
            if pcfg.api_key.is_some() {
                ok(&format!("Provider '{name}' configured with API key"));
                checks_passed += 1;
            } else {
                // Check env vars as fallback
                let env_key = format!("ADACLAW_{}_API_KEY", name.to_uppercase().replace('-', "_"));
                if std::env::var(&env_key).is_ok() {
                    ok(&format!("Provider '{name}' — key via env var {env_key}"));
                    checks_passed += 1;
                } else {
                    warn(&format!(
                        "Provider '{name}' has no API key (set {env_key} or api_key in config)"
                    ));
                    checks_warned += 1;
                }
            }
        }
    }

    // ── Agents ────────────────────────────────────────────────────────────────
    if cfg.agents.is_empty() {
        warn("No agents configured — will use built-in 'assistant' defaults");
        checks_warned += 1;
    } else {
        for (name, agent_cfg) in &cfg.agents {
            let provider_ok = cfg.providers.contains_key(&agent_cfg.provider)
                || agent_cfg.provider == "openai"
                || agent_cfg.provider == "anthropic"
                || agent_cfg.provider == "ollama";
            if provider_ok {
                ok(&format!(
                    "Agent '{}' → provider='{}' model='{}'",
                    name, agent_cfg.provider, agent_cfg.model
                ));
                checks_passed += 1;
            } else {
                warn(&format!(
                    "Agent '{}' references provider '{}' which is not configured",
                    name, agent_cfg.provider
                ));
                checks_warned += 1;
            }
        }
    }

    // ── Memory ────────────────────────────────────────────────────────────────
    let mem_path = std::path::Path::new(&cfg.memory.path);
    match cfg.memory.backend.as_str() {
        "sqlite" => {
            if mem_path.exists() {
                ok(&format!(
                    "Memory: SQLite database exists at '{}'",
                    cfg.memory.path
                ));
                checks_passed += 1;
            } else {
                info(&format!(
                    "Memory: SQLite will be created at '{}' on first use",
                    cfg.memory.path
                ));
                checks_passed += 1;
            }
            // Check embedding provider
            match cfg.memory.embedding_provider.as_str() {
                "none" | "" => {
                    info("Memory: embedding provider=none (keyword-only search)");
                }
                "fastembed" => {
                    ok("Memory: fastembed embedding provider configured");
                    checks_passed += 1;
                }
                "openai" => {
                    if cfg.memory.embed_api_key.is_some() || std::env::var("OPENAI_API_KEY").is_ok()
                    {
                        ok("Memory: OpenAI embedding provider configured with API key");
                        checks_passed += 1;
                    } else {
                        warn("Memory: OpenAI embedding provider has no API key");
                        checks_warned += 1;
                    }
                }
                other => {
                    warn(&format!("Memory: unknown embedding provider '{other}'"));
                    checks_warned += 1;
                }
            }
        }
        "markdown" => {
            if mem_path.exists() {
                ok(&format!(
                    "Memory: Markdown directory exists at '{}'",
                    cfg.memory.path
                ));
            } else {
                info(&format!(
                    "Memory: Markdown directory will be created at '{}'",
                    cfg.memory.path
                ));
            }
            checks_passed += 1;
        }
        "none" => {
            info("Memory: explicitly disabled");
            checks_passed += 1;
        }
        other => {
            warn(&format!("Memory: unknown backend '{other}'"));
            checks_warned += 1;
        }
    }

    // ── Workspace ─────────────────────────────────────────────────────────────
    let ws = cfg.security.workspace.as_deref().unwrap_or("./workspace");
    if std::path::Path::new(ws).exists() {
        ok(&format!("Workspace directory '{}' exists", ws));
        checks_passed += 1;
    } else {
        warn(&format!(
            "Workspace directory '{}' does not exist (will be created on first use)",
            ws
        ));
        checks_warned += 1;
    }

    // ── Channels ──────────────────────────────────────────────────────────────
    if cfg.channels.is_empty() {
        warn("No channels configured — only CLI channel will be available");
        checks_warned += 1;
    } else {
        for (name, ch_cfg) in &cfg.channels {
            match ch_cfg.kind.as_str() {
                "telegram" => {
                    if ch_cfg.token.is_some() {
                        ok(&format!("Channel '{name}' (telegram): token configured"));
                        checks_passed += 1;
                    } else {
                        fail(&format!("Channel '{name}' (telegram): missing token"));
                        checks_failed += 1;
                    }
                }
                "discord" => {
                    if ch_cfg.token.is_some() {
                        ok(&format!("Channel '{name}' (discord): token configured"));
                        checks_passed += 1;
                    } else {
                        fail(&format!("Channel '{name}' (discord): missing token"));
                        checks_failed += 1;
                    }
                }
                "slack" => {
                    if ch_cfg.token.is_some() {
                        ok(&format!("Channel '{name}' (slack): token configured"));
                        checks_passed += 1;
                    } else {
                        fail(&format!("Channel '{name}' (slack): missing token"));
                        checks_failed += 1;
                    }
                }
                "cli" | "" => {
                    ok(&format!("Channel '{name}' (cli): always available"));
                    checks_passed += 1;
                }
                other => {
                    info(&format!("Channel '{name}' ({}): configured", other));
                    checks_passed += 1;
                }
            }
        }
    }

    // ── Gateway ───────────────────────────────────────────────────────────────
    if cfg.gateway.bearer_token.is_some() {
        ok(&format!(
            "Gateway: bearer token configured, listening on {}",
            cfg.gateway.bind
        ));
        checks_passed += 1;
    } else {
        warn(&format!(
            "Gateway: no bearer token configured (insecure!) — bind={}",
            cfg.gateway.bind
        ));
        checks_warned += 1;
    }

    // ── Security ──────────────────────────────────────────────────────────────
    let autonomy_level = cfg
        .security
        .autonomy_level
        .parse::<AutonomyLevel>()
        .unwrap_or(AutonomyLevel::Supervised);
    if !cfg.security.allow_full_outside_container {
        if let Some(warning) = ContainerEnvironment::check_autonomy_safety(&autonomy_level) {
            warn(&format!("Security: {}", warning.message));
            println!("    Mitigation: {}", warning.mitigation);
            checks_warned += 1;
        } else {
            ok(&format!(
                "Security: autonomy_level='{}' — environment check passed",
                cfg.security.autonomy_level
            ));
            checks_passed += 1;
        }
    } else {
        info(&format!(
            "Security: autonomy_level='{}' — container check skipped (allow_full_outside_container=true)",
            cfg.security.autonomy_level
        ));
        checks_passed += 1;
    }

    if cfg.security.require_otp_for_estop {
        ok("Security: OTP required for estop — TOTP is active");
        checks_passed += 1;
    }

    // ── Audit log ─────────────────────────────────────────────────────────────
    if let Some(audit_path) = &cfg.security.audit_log {
        let audit_dir = std::path::Path::new(audit_path).parent();
        let dir_ok = audit_dir
            .map(|d| d == std::path::Path::new("") || d.exists())
            .unwrap_or(true);
        if dir_ok {
            ok(&format!("Audit log: enabled → '{}'", audit_path));
            checks_passed += 1;
        } else {
            warn(&format!(
                "Audit log: parent directory for '{}' does not exist",
                audit_path
            ));
            checks_warned += 1;
        }
    } else {
        info("Audit log: disabled (set security.audit_log to enable)");
    }

    // ── Observability ─────────────────────────────────────────────────────────
    match cfg.observability.backend.as_str() {
        "noop" | "none" | "" => {
            info("Observability: disabled (set observability.backend to 'prometheus' or 'log')");
        }
        "prometheus" => {
            ok("Observability: Prometheus metrics enabled → GET /metrics");
            checks_passed += 1;
        }
        "log" => {
            ok("Observability: log observer enabled");
            checks_passed += 1;
        }
        other => {
            warn(&format!("Observability: unknown backend '{other}'"));
            checks_warned += 1;
        }
    }

    // ── Skills ────────────────────────────────────────────────────────────────
    let workspace_dir = cfg.security.workspace.as_deref().unwrap_or("./workspace");
    let skills_dir = std::path::Path::new(workspace_dir).join("skills");
    if skills_dir.exists() {
        let skill_count = std::fs::read_dir(&skills_dir)
            .map(|entries| entries.flatten().filter(|e| e.path().is_dir()).count())
            .unwrap_or(0);
        if skill_count > 0 {
            ok(&format!(
                "Skills: {} skill(s) found in '{}'",
                skill_count,
                skills_dir.display()
            ));
            checks_passed += 1;
        } else {
            info(&format!(
                "Skills: directory exists but no skills installed at '{}'",
                skills_dir.display()
            ));
        }
    } else {
        info(&format!(
            "Skills: directory '{}' not found (create it to add skills)",
            skills_dir.display()
        ));
    }

    // ── Tunnel ────────────────────────────────────────────────────────────────
    match cfg.tunnel.provider.as_str() {
        "none" | "" => {
            info("Tunnel: none (local only)");
        }
        "cloudflare" => {
            if cfg.tunnel.cloudflare_token.is_some() {
                ok("Tunnel: Cloudflare configured with token");
                checks_passed += 1;
            } else {
                fail("Tunnel: Cloudflare selected but no token configured");
                checks_failed += 1;
            }
        }
        "tailscale" => {
            ok("Tunnel: Tailscale configured (requires tailscale daemon running)");
            checks_passed += 1;
        }
        "ngrok" => {
            if cfg.tunnel.ngrok_token.is_some() {
                ok("Tunnel: ngrok configured with auth token");
                checks_passed += 1;
            } else {
                warn("Tunnel: ngrok selected but no auth token configured");
                checks_warned += 1;
            }
        }
        other => {
            info(&format!("Tunnel: provider='{}' configured", other));
            checks_passed += 1;
        }
    }

    // ── Binary size estimate (informational) ──────────────────────────────────
    if let Ok(exe) = std::env::current_exe()
        && let Ok(meta) = std::fs::metadata(&exe)
    {
        let size_mb = meta.len() as f64 / 1_048_576.0;
        if size_mb < 10.0 {
            ok(&format!(
                "Binary size: {:.1} MB (target: <10 MB ✓)",
                size_mb
            ));
            checks_passed += 1;
        } else {
            warn(&format!(
                "Binary size: {:.1} MB (target: <10 MB — use --release build)",
                size_mb
            ));
            checks_warned += 1;
        }
    }

    // ── Summary ───────────────────────────────────────────────────────────────
    println!();
    println!("─────────────────────────────────────────");
    println!(
        "Doctor summary: ✅ {} passed  ⚠️  {} warnings  ❌ {} failed",
        checks_passed, checks_warned, checks_failed
    );

    if checks_failed > 0 {
        println!();
        println!("⚠️  Fix the ❌ issues above before running AdaClaw in production.");
    } else if checks_warned == 0 {
        println!();
        println!("✅  All checks passed! AdaClaw is ready to run.");
        println!("   Run: adaclaw run");
    }
}

// ── Output helpers ────────────────────────────────────────────────────────────

fn ok(msg: &str) {
    println!("✅  {}", msg);
}

fn warn(msg: &str) {
    println!("⚠️   {}", msg);
}

fn fail(msg: &str) {
    println!("❌  {}", msg);
}

fn info(msg: &str) {
    println!("ℹ️   {}", msg);
}
