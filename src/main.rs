// Phase 14-P2-3: Optional jemalloc global allocator.
// Enable with `cargo build --features jemalloc` for lower fragmentation on
// long-running daemon processes.  Not included in `default` features so that
// the binary size stays < 10 MB for development builds.
#[cfg(all(feature = "jemalloc", not(target_env = "msvc")))]
#[global_allocator]
static ALLOC: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

use clap::{Parser, Subcommand};

pub mod agents;
pub mod bus;
pub mod cli;
pub mod config;
pub mod cron;
pub mod daemon;
pub mod identity;
pub mod observability;
pub mod skills;
pub mod state;
pub mod tunnel;

#[derive(Parser)]
#[command(
    author,
    version,
    about = "AdaClaw — Lightweight Rust AI Agent Runtime",
    long_about = None
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Start the AdaClaw daemon (gateway + channels + agent loop)
    Run,
    /// Start an interactive chat session in the terminal
    Chat {
        /// Provider to use (default: auto-detect from env)
        #[arg(long)]
        provider: Option<String>,
        /// Model to use (default: from config or gpt-4o)
        #[arg(long)]
        model: Option<String>,
    },
    /// Manage and validate configuration
    Config {
        #[command(subcommand)]
        action: ConfigAction,
    },
    /// Stop the running daemon or trigger emergency stop
    Stop,
    /// Show current status
    Status,
    /// Run diagnostic checks
    Doctor,
    /// Run the interactive setup wizard
    Onboard,
    /// Manage skills (list, install, remove, audit)
    Skill {
        #[command(subcommand)]
        action: SkillAction,
    },
}

#[derive(Subcommand)]
enum ConfigAction {
    /// Validate config.toml and show all semantic errors
    Check {
        /// Path to config file (default: config.toml)
        #[arg(short, long, default_value = "config.toml")]
        file: String,
    },
    /// Show the current config schema version
    Version,
}

#[derive(Subcommand)]
enum SkillAction {
    /// List installed skills
    List,
    /// Install a skill from URL or clawhub:<name>
    Install {
        /// URL, clawhub:<name>, or local path
        source: String,
    },
    /// Remove an installed skill
    Remove {
        /// Skill name to remove
        name: String,
    },
    /// Run security audit on a skill
    Audit {
        /// Skill name to audit
        name: String,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Respect RUST_LOG env var, default to "info"
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();

    match &cli.command {
        Commands::Run => {
            daemon::run::start_daemon().await?;
        }

        Commands::Chat { provider, model } => {
            run_chat(provider.as_deref(), model.as_deref()).await?;
        }

        Commands::Config { action } => match action {
            ConfigAction::Check { file } => {
                cmd_config_check(file);
            }
            ConfigAction::Version => {
                println!(
                    "AdaClaw config schema version: {} (current)",
                    config::migration::CURRENT_VERSION
                );
            }
        },

        Commands::Stop => {
            cmd_stop().await;
        }

        Commands::Status => {
            cmd_status().await;
        }

        Commands::Doctor => {
            cli::doctor::run_doctor().await;
        }

        Commands::Onboard => {
            cli::onboard::run_onboard().await;
        }

        Commands::Skill { action } => match action {
            SkillAction::List => {
                cli::skill::cmd_list();
            }
            SkillAction::Install { source } => {
                if let Err(e) = cli::skill::cmd_install(source).await {
                    eprintln!("Error: {}", e);
                    std::process::exit(1);
                }
            }
            SkillAction::Remove { name } => {
                if let Err(e) = cli::skill::cmd_remove(name) {
                    eprintln!("Error: {}", e);
                    std::process::exit(1);
                }
            }
            SkillAction::Audit { name } => {
                if let Err(e) = cli::skill::cmd_audit(name) {
                    eprintln!("Error: {}", e);
                    std::process::exit(1);
                }
            }
        },
    }

    Ok(())
}

/// Simple interactive REPL for quick testing without the full daemon.
async fn run_chat(
    provider_name: Option<&str>,
    model_override: Option<&str>,
) -> anyhow::Result<()> {
    use std::io::{self, BufRead, Write};

    let cfg = config::Config::load();

    // Resolve provider
    let (pname, pcfg) = if let Some(name) = provider_name {
        (name.to_string(), cfg.providers.get(name).cloned().unwrap_or_default())
    } else {
        // Pick the first configured provider, or default to openai
        cfg.providers
            .iter()
            .next()
            .map(|(k, v)| (k.clone(), v.clone()))
            .unwrap_or_else(|| ("openai".to_string(), Default::default()))
    };

    let provider = adaclaw_providers::router::create_provider(
        &pname,
        pcfg.api_key.as_deref(),
        pcfg.base_url.as_deref(),
    )?;

    let model = model_override
        .map(|s| s.to_string())
        .or_else(|| pcfg.default_model.clone())
        .or_else(|| {
            cfg.agents
                .get("assistant")
                .map(|a| a.model.clone())
        })
        .unwrap_or_else(|| "gpt-4o".to_string());

    let tools = adaclaw_tools::registry::all_tools(None);
    let engine = agents::engine::AgentEngine::new();

    println!("AdaClaw Chat — provider: {}, model: {}", pname, model);
    println!("Type your message and press Enter. Ctrl-C or Ctrl-D to exit.\n");

    let stdin = io::stdin();
    loop {
        print!("> ");
        io::stdout().flush()?;

        let mut line = String::new();
        match stdin.lock().read_line(&mut line) {
            Ok(0) => break, // EOF
            Ok(_) => {}
            Err(e) => {
                eprintln!("Read error: {}", e);
                break;
            }
        }

        let input = line.trim();
        if input.is_empty() {
            continue;
        }

        match engine
            .run_tool_loop(provider.as_ref(), &tools, input, &model, 0.7)
            .await
        {
            Ok(response) => {
                println!("\n{}\n", response);
            }
            Err(e) => {
                eprintln!("Error: {}", e);
            }
        }
    }

    println!("Goodbye.");
    Ok(())
}

/// `adaclaw stop` — Send stop signal to the running daemon via gateway API.
///
/// Reads `gateway.bind` and `gateway.bearer_token` from config.toml and sends
/// `POST /v1/stop`.  If the daemon is not reachable, prints a clear message.
async fn cmd_stop() {
    let cfg = config::Config::load();
    let addr = &cfg.gateway.bind;

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()
        .unwrap_or_default();

    let url = format!("http://{}/v1/stop", addr);
    let mut req = client.post(&url).json(&serde_json::json!({}));

    if let Some(token) = &cfg.gateway.bearer_token {
        req = req.header("Authorization", format!("Bearer {}", token));
    }

    match req.send().await {
        Ok(resp) => {
            if resp.status().is_success() {
                println!("✅ Stop signal sent. Daemon is shutting down.");
            } else {
                eprintln!(
                    "⚠️  Gateway responded with HTTP {}: {}",
                    resp.status(),
                    resp.text().await.unwrap_or_default()
                );
            }
        }
        Err(e) if e.is_connect() || e.is_timeout() => {
            println!("ℹ️  Daemon not running (could not connect to http://{}).", addr);
        }
        Err(e) => {
            eprintln!("Error sending stop signal: {}", e);
        }
    }
}

/// `adaclaw status` — Query daemon status via gateway API.
///
/// Reads `gateway.bind` from config.toml and sends `GET /v1/status`.
/// Prints a summary if the daemon is running, or reports it as not running.
async fn cmd_status() {
    let cfg = config::Config::load();
    let addr = &cfg.gateway.bind;

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()
        .unwrap_or_default();

    let url = format!("http://{}/v1/status", addr);

    match client.get(&url).send().await {
        Ok(resp) => {
            if resp.status().is_success() {
                let body = resp.text().await.unwrap_or_default();
                println!("✅ Daemon is running (http://{})\n{}", addr, body);
            } else {
                eprintln!(
                    "⚠️  Gateway responded with HTTP {}: {}",
                    resp.status(),
                    resp.text().await.unwrap_or_default()
                );
            }
        }
        Err(e) if e.is_connect() || e.is_timeout() => {
            println!("ℹ️  Daemon not running (could not connect to http://{}).", addr);
            println!("   Run `adaclaw run` or `adaclaw daemon start` to start the daemon.");
        }
        Err(e) => {
            eprintln!("Error querying status: {}", e);
        }
    }
}

/// `adaclaw config check [--file <path>]`
///
/// Loads the config file, runs migration, then runs all semantic validators.
/// Prints every error with field paths and exits 1 if any are found.
fn cmd_config_check(file: &str) {
    // ── Step 1: load + parse + migrate ────────────────────────────────────────
    let cfg = match config::Config::load_from_file(file) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("✗ Failed to load '{file}':\n  {e}");
            std::process::exit(1);
        }
    };

    let version = cfg.config_version;
    let current = config::migration::CURRENT_VERSION;

    // ── Step 2: semantic validation ────────────────────────────────────────────
    let errors = cfg.validate();

    if errors.is_empty() {
        println!(
            "✓ Config '{file}' is valid  (schema version {version}/{current})"
        );
    } else {
        eprintln!(
            "✗ Config '{file}' has {} error(s)  (schema version {version}/{current}):\n",
            errors.len()
        );
        for (i, e) in errors.iter().enumerate() {
            eprintln!("  {}. {}", i + 1, e);
        }
        eprintln!();
        std::process::exit(1);
    }
}
