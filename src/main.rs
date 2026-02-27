use clap::{Parser, Subcommand};
use tracing::info;

pub mod agents;
pub mod bus;
pub mod cli;
pub mod config;
pub mod cron;
pub mod daemon;
pub mod identity;
pub mod observability;
pub mod skills;
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
    /// Manage configuration
    Config,
    /// Stop the running daemon or trigger emergency stop
    Stop,
    /// Show current status
    Status,
    /// Run diagnostic checks
    Doctor,
    /// Run the interactive setup wizard
    Onboard,
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

        Commands::Config => {
            info!("Configuration management coming soon. Edit config.toml directly.");
        }

        Commands::Stop => {
            info!("Sending stop signal... (not yet implemented)");
        }

        Commands::Status => {
            info!("AdaClaw status: (not yet implemented)");
        }

        Commands::Doctor => {
            cli::doctor::run_doctor().await;
        }

        Commands::Onboard => {
            cli::onboard::run_onboard().await;
        }
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

    let tools = adaclaw_tools::registry::all_tools();
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
