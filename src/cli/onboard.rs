//! `adaclaw onboard` — interactive first-run setup wizard.
//!
//! Guides the user through:
//! 1. Provider selection and API key entry
//! 2. Model selection
//! 3. Channel configuration (optional)
//! 4. Security/autonomy level
//! 5. Generates `config.toml` and workspace skeleton files
//!
//! This implementation uses basic `stdin` I/O (no external `dialoguer` crate)
//! to keep binary size minimal.

use std::io::{self, BufRead, Write};
use std::path::Path;
use tracing::info;

/// Entry point for `adaclaw onboard`.
pub async fn run_onboard() {
    print_banner();
    println!("  Welcome to AdaClaw — Lightweight Rust AI Agent Runtime");
    println!("  This wizard will configure your agent in under 2 minutes.");
    println!();

    // Check if config already exists
    let config_path = "config.toml";
    if Path::new(config_path).exists() {
        println!("  ⚠️  config.toml already exists.");
        let overwrite = prompt("  Overwrite? [y/N]: ");
        if !overwrite.trim().eq_ignore_ascii_case("y") {
            println!("  Onboarding canceled — existing config preserved.");
            return;
        }
        println!();
    }

    // ── Step 1: Provider ──────────────────────────────────────────────────────
    print_step(1, 5, "AI Provider & API Key");

    println!("  Supported providers:");
    println!("    1. openrouter   — 200+ models via one API key (recommended)");
    println!("    2. openai       — GPT-4o, o1, GPT-4 Turbo");
    println!("    3. anthropic    — Claude Sonnet & Opus");
    println!("    4. deepseek     — DeepSeek Chat & Reasoner (affordable)");
    println!("    5. ollama       — Local models (no API key needed)");
    println!("    6. other        — Any OpenAI-compatible endpoint");
    println!();

    let provider_choice = prompt("  Choose provider [1-6, default=1]: ");
    let (provider_name, default_model) = match provider_choice.trim() {
        "2" => ("openai", "gpt-4o"),
        "3" => ("anthropic", "claude-3-5-sonnet-20241022"),
        "4" => ("deepseek", "deepseek-chat"),
        "5" => ("ollama", "llama3"),
        "6" => ("openai", "gpt-4o"), // custom — will ask for base_url
        _ => ("openrouter", "anthropic/claude-3.5-sonnet"),
    };

    let api_key = if provider_name == "ollama" {
        println!("  ✅  Ollama: no API key needed (using http://localhost:11434)");
        String::new()
    } else {
        let key_url = match provider_name {
            "openrouter" => "https://openrouter.ai/keys",
            "openai" => "https://platform.openai.com/api-keys",
            "anthropic" => "https://console.anthropic.com/settings/keys",
            "deepseek" => "https://platform.deepseek.com/api_keys",
            _ => "",
        };
        if !key_url.is_empty() {
            println!("  Get your API key at: {}", key_url);
        }
        let key = prompt("  Paste your API key (or Enter to skip): ");
        key.trim().to_string()
    };

    // Custom base_url for provider 6
    let base_url = if provider_choice.trim() == "6" {
        let url = prompt("  Custom base URL (e.g. http://localhost:8080/v1): ");
        url.trim().to_string()
    } else {
        String::new()
    };

    // ── Step 2: Model ─────────────────────────────────────────────────────────
    print_step(2, 5, "Model");

    let model = {
        let input = prompt(&format!("  Model name [default: {default_model}]: "));
        let trimmed = input.trim();
        if trimmed.is_empty() {
            default_model.to_string()
        } else {
            trimmed.to_string()
        }
    };

    // ── Step 3: Channels ──────────────────────────────────────────────────────
    print_step(3, 5, "Channels (optional)");

    println!("  Available channels: telegram, discord, slack, cli (always active)");
    let add_channel = prompt("  Add a Telegram bot? [y/N]: ");
    let telegram_token = if add_channel.trim().eq_ignore_ascii_case("y") {
        println!("  Create a bot at @BotFather on Telegram, then paste the token:");
        let token = prompt("  Telegram bot token: ");
        token.trim().to_string()
    } else {
        String::new()
    };

    // ── Step 4: Autonomy ──────────────────────────────────────────────────────
    print_step(4, 5, "Autonomy Level");

    println!("  readonly   — no tool execution, read-only queries only");
    println!("  supervised — tools require confirmation before running (default)");
    println!("  full       — fully autonomous, no confirmation needed");
    println!();

    let autonomy = {
        let input = prompt("  Autonomy level [readonly/supervised/full, default=supervised]: ");
        match input.trim().to_ascii_lowercase().as_str() {
            "readonly" | "r" => "readonly",
            "full" | "f" => "full",
            _ => "supervised",
        }
    };

    if autonomy == "full" {
        let in_container = Path::new("/.dockerenv").exists()
            || std::env::var("container").is_ok()
            || std::env::var("DOCKER_CONTAINER").is_ok();
        if !in_container {
            println!();
            println!("  ⚠️  WARNING: Full autonomy outside a container is risky!");
            println!("     Consider: docker compose up -d");
            println!("     Or set security.allow_full_outside_container = true to suppress this.");
        }
    }

    // ── Step 5: Generate config ───────────────────────────────────────────────
    print_step(5, 5, "Generating Configuration");

    let config_toml = build_config_toml(
        provider_name,
        &api_key,
        &base_url,
        &model,
        &telegram_token,
        autonomy,
    );

    match std::fs::write(config_path, &config_toml) {
        Ok(()) => {
            println!("  ✅  config.toml written");
        }
        Err(e) => {
            println!("  ❌  Failed to write config.toml: {}", e);
            return;
        }
    }

    // Create workspace directory
    let workspace = "./workspace";
    if !Path::new(workspace).exists() {
        match std::fs::create_dir_all(workspace) {
            Ok(()) => println!("  ✅  Created workspace directory: {}", workspace),
            Err(e) => println!("  ⚠️  Failed to create workspace: {}", e),
        }
    }

    // Create skills directory
    let skills_dir = format!("{workspace}/skills");
    if !Path::new(&skills_dir).exists() {
        let _ = std::fs::create_dir_all(&skills_dir);
        println!("  ✅  Created skills directory: {}", skills_dir);
    }

    // Create IDENTITY.md in workspace
    let identity_path = format!("{workspace}/IDENTITY.md");
    if !Path::new(&identity_path).exists() {
        let identity_content = "# IDENTITY.md\n\n\
            - **Name:** AdaClaw\n\
            - **Creature:** A Rust-forged AI agent — fast, lean, and reliable\n\
            - **Vibe:** Sharp, direct, resourceful. Helpful without being sycophantic.\n\n\
            You are AdaClaw, an AI assistant. Be helpful, honest, and concise.\n";
        let _ = std::fs::write(&identity_path, identity_content);
        println!("  ✅  Created IDENTITY.md");
    }

    // ── Final summary ─────────────────────────────────────────────────────────
    println!();
    println!("  ══════════════════════════════════════════════════");
    println!("  ⚡ AdaClaw configured successfully!");
    println!("  ══════════════════════════════════════════════════");
    println!();
    println!("  Provider:   {} / {}", provider_name, model);
    if !telegram_token.is_empty() {
        println!("  Telegram:   bot configured ✅");
    }
    println!("  Autonomy:   {}", autonomy);
    println!("  Workspace:  {}", workspace);
    println!();
    println!("  Next steps:");
    println!("    adaclaw run      # start the daemon");
    println!("    adaclaw chat     # interactive chat");
    println!("    adaclaw doctor   # check configuration");
    println!();

    info!("Onboarding complete");
}

// ── Config generation ─────────────────────────────────────────────────────────

fn build_config_toml(
    provider_name: &str,
    api_key: &str,
    base_url: &str,
    model: &str,
    telegram_token: &str,
    autonomy: &str,
) -> String {
    let mut out = String::new();

    out.push_str("# AdaClaw configuration — generated by `adaclaw onboard`\n");
    out.push_str("# Edit this file to customize your setup.\n");
    out.push_str("# Docs: https://github.com/adaclaw/adaclaw\n\n");

    // Provider
    out.push_str(&format!("[providers.{}]\n", provider_name));
    if !api_key.is_empty() {
        out.push_str(&format!("api_key = \"{}\"\n", api_key));
    }
    if !base_url.is_empty() {
        out.push_str(&format!("base_url = \"{}\"\n", base_url));
    }
    out.push_str(&format!("default_model = \"{}\"\n", model));
    out.push_str("timeout_secs = 60\n\n");

    // Agent
    out.push_str("[agents.assistant]\n");
    out.push_str(&format!("provider = \"{}\"\n", provider_name));
    out.push_str(&format!("model = \"{}\"\n", model));
    out.push_str("temperature = 0.7\n");
    out.push_str("max_iterations = 10\n\n");

    // Memory
    out.push_str("[memory]\n");
    out.push_str("backend = \"sqlite\"\n");
    out.push_str("path = \"memory.db\"\n");
    out.push_str("embedding_provider = \"none\"\n\n");

    // Security
    out.push_str("[security]\n");
    out.push_str(&format!("autonomy_level = \"{}\"\n", autonomy));
    out.push_str("workspace = \"./workspace\"\n");
    if autonomy == "full" {
        out.push_str("# allow_full_outside_container = true  # uncomment to suppress container warning\n");
    }
    out.push('\n');

    // Channels
    if !telegram_token.is_empty() {
        out.push_str("[channels.telegram]\n");
        out.push_str("kind = \"telegram\"\n");
        out.push_str(&format!("token = \"{}\"\n", telegram_token));
        out.push_str("# allow_from = [\"your_telegram_username\"]\n\n");
    }

    // Gateway
    out.push_str("[gateway]\n");
    out.push_str("bind = \"127.0.0.1:8080\"\n");
    out.push_str("# bearer_token = \"change-me\"\n\n");

    // Observability
    out.push_str("[observability]\n");
    out.push_str("backend = \"log\"  # options: noop, log, prometheus\n");
    out.push_str("# runtime_trace_path = \".adaclaw/runtime-trace.jsonl\"\n");
    out.push_str("# runtime_trace_max_entries = 1000\n\n");

    // Routing
    out.push_str("[[routing]]\n");
    out.push_str("default = true\n");
    out.push_str("agent = \"assistant\"\n");

    out
}

// ── UI helpers ────────────────────────────────────────────────────────────────

fn prompt(question: &str) -> String {
    print!("{}", question);
    let _ = io::stdout().flush();

    let stdin = io::stdin();
    let mut line = String::new();
    if stdin.lock().read_line(&mut line).is_err() {
        return String::new();
    }
    line.trim_end_matches('\n')
        .trim_end_matches('\r')
        .to_string()
}

fn print_step(current: u8, total: u8, title: &str) {
    println!();
    println!("  [{}/{}] {}", current, total, title);
    println!("  {}", "─".repeat(48));
}

fn print_banner() {
    println!();
    println!("  ╔══════════════════════════════════════════╗");
    println!("  ║  ⚡ AdaClaw — Rust AI Agent Runtime      ║");
    println!("  ║  Fast · Secure · Multi-Channel · Open    ║");
    println!("  ╚══════════════════════════════════════════╝");
    println!();
}
