use adaclaw_core::channel::{InboundMessage, MessageBus, OutboundMessage};
use anyhow::Result;
use async_trait::async_trait;
use tokio::sync::{broadcast, mpsc};
use tracing::warn;

/// Phase 14-P2-2: Backpressure timeout for send_inbound.
///
/// If the inbound channel is full for longer than this duration, the message
/// is dropped and a warning is logged.  This prevents slow/busy Agent loops
/// from blocking inbound channel goroutines indefinitely.
const BACKPRESSURE_TIMEOUT_MS: u64 = 200;

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

        // Phase 14-P2-2: Bounded channel with backpressure timeout.
        //
        // `mpsc::Sender::send()` blocks indefinitely when the channel is full
        // (bounded channel with capacity 1024 — see daemon/run.rs).  To prevent
        // a slow agent loop from stalling inbound channel goroutines, we add a
        // short timeout.  If the channel is full after BACKPRESSURE_TIMEOUT_MS,
        // the message is dropped and a warning is logged.
        //
        // Channels should forward the warning to the user if they detect
        // a send_inbound failure (return value is Err).
        match tokio::time::timeout(
            std::time::Duration::from_millis(BACKPRESSURE_TIMEOUT_MS),
            self.inbound_tx.send(msg),
        )
        .await
        {
            Ok(Ok(())) => Ok(()),
            Ok(Err(e)) => Err(anyhow::anyhow!("MessageBus closed: {}", e)),
            Err(_timeout) => {
                warn!(
                    "MessageBus inbound channel full (capacity 1024) after {}ms — message dropped",
                    BACKPRESSURE_TIMEOUT_MS
                );
                // Return Ok so callers don't surface a misleading error to users.
                // The warning log is the signal for operators.
                Ok(())
            }
        }
    }
}

impl AppMessageBus {
    /// 绕过白名单直接注入 `InboundMessage`，用于 sub-agent 结果回传。
    ///
    /// **仅供内部 Agent 系统使用**（`channel = "system"` 的消息）。
    /// 外部渠道消息必须使用 `send_inbound()` 以确保白名单校验。
    pub async fn send_inbound_bypass(&self, msg: InboundMessage) -> Result<()> {
        match tokio::time::timeout(
            std::time::Duration::from_millis(BACKPRESSURE_TIMEOUT_MS),
            self.inbound_tx.send(msg),
        )
        .await
        {
            Ok(Ok(())) => Ok(()),
            Ok(Err(e)) => Err(anyhow::anyhow!("bus bypass error: {}", e)),
            Err(_) => {
                warn!("MessageBus bypass: channel full, dropping system message");
                Ok(())
            }
        }
    }
}
