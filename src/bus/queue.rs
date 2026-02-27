use adaclaw_core::channel::{InboundMessage, MessageBus, OutboundMessage};
use anyhow::Result;
use async_trait::async_trait;
use tokio::sync::{broadcast, mpsc};

#[derive(Clone)]
pub struct AppMessageBus {
    inbound_tx: mpsc::Sender<InboundMessage>,
    outbound_tx: broadcast::Sender<OutboundMessage>,
    allowlist: Vec<String>,
}

impl AppMessageBus {
    pub fn new(
        inbound_tx: mpsc::Sender<InboundMessage>,
        outbound_tx: broadcast::Sender<OutboundMessage>,
        allowlist: Vec<String>,
    ) -> Self {
        Self {
            inbound_tx,
            outbound_tx,
            allowlist,
        }
    }

    pub fn subscribe_outbound(&self) -> broadcast::Receiver<OutboundMessage> {
        self.outbound_tx.subscribe()
    }

    pub fn send_outbound(&self, msg: OutboundMessage) -> Result<()> {
        let _ = self.outbound_tx.send(msg);
        Ok(())
    }
}

#[async_trait]
impl MessageBus for AppMessageBus {
    async fn send_inbound(&self, msg: InboundMessage) -> Result<()> {
        if !self.allowlist.is_empty()
            && !self.allowlist.contains(&msg.sender_id)
            && !self.allowlist.contains(&msg.sender_name)
        {
            return Err(anyhow::anyhow!(
                "Unauthorized sender (deny-by-default): {}",
                msg.sender_id
            ));
        }

        self.inbound_tx
            .send(msg)
            .await
            .map_err(|e| anyhow::anyhow!("bus error: {}", e))
    }
}

impl AppMessageBus {
    /// 绕过白名单直接注入 `InboundMessage`，用于 sub-agent 结果回传。
    ///
    /// **仅供内部 Agent 系统使用**（`channel = "system"` 的消息）。
    /// 外部渠道消息必须使用 `send_inbound()` 以确保白名单校验。
    pub async fn send_inbound_bypass(&self, msg: InboundMessage) -> Result<()> {
        self.inbound_tx
            .send(msg)
            .await
            .map_err(|e| anyhow::anyhow!("bus bypass error: {}", e))
    }
}
