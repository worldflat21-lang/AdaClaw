//! CLI 渠道 — 本地交互式 REPL
//!
//! - `start()`: 从 stdin 读取每行输入，发布到 MessageBus
//! - `send()`: 将 Agent 回复打印到 stdout
//! - 支持 `/new`、`/stop`、`/help` 斜线命令（作为普通消息转发，由 Agent 处理）

use crate::base::BaseChannel;
use adaclaw_core::channel::{Channel, MessageBus, MessageContent, OutboundMessage};
use anyhow::Result;
use async_trait::async_trait;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, BufReader};
use tracing::info;

pub struct CliChannel {
    base: BaseChannel,
}

impl Default for CliChannel {
    fn default() -> Self {
        Self::new()
    }
}

impl CliChannel {
    pub fn new() -> Self {
        Self {
            base: BaseChannel::new("cli"),
        }
    }
}

#[async_trait]
impl Channel for CliChannel {
    fn name(&self) -> &str {
        "cli"
    }

    fn is_running(&self) -> bool {
        self.base.is_running()
    }

    async fn start(&self, bus: Arc<dyn MessageBus>) -> Result<()> {
        self.base.set_running(true);
        info!("CLI channel started. Type a message and press Enter.");
        info!("Commands: /new (new session), /stop, /help");

        let stdin = tokio::io::stdin();
        let mut reader = BufReader::new(stdin);
        let mut line = String::new();

        print_prompt();

        while self.base.is_running() {
            line.clear();
            let n = reader.read_line(&mut line).await?;
            if n == 0 {
                // EOF (Ctrl-D)
                info!("CLI channel: EOF received, stopping");
                break;
            }

            let text = line.trim().to_string();
            if text.is_empty() {
                print_prompt();
                continue;
            }

            let metadata: HashMap<String, Value> = HashMap::new();
            self.base
                .handle_message(&bus, "local_user", "User", "cli:default", &text, metadata)
                .await;
        }

        self.base.set_running(false);
        info!("CLI channel stopped");
        Ok(())
    }

    async fn send(&self, msg: OutboundMessage) -> Result<()> {
        let content = match &msg.content {
            MessageContent::Text(t) => t.clone(),
            MessageContent::Image(_) => "[image received]".to_string(),
            MessageContent::Audio(_) => "[audio received]".to_string(),
            MessageContent::File { name, .. } => format!("[file received: {}]", name),
        };

        // 打印 Agent 回复，带清晰的分隔
        println!("\n🤖 \x1b[36mAssistant\x1b[0m:\n{}\n", content);
        print_prompt();
        Ok(())
    }

    async fn stop(&self) -> Result<()> {
        self.base.set_running(false);
        Ok(())
    }
}

fn print_prompt() {
    use std::io::Write;
    print!("\x1b[32mYou\x1b[0m: ");
    let _ = std::io::stdout().flush();
}
