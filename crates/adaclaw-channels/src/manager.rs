//! `ChannelManager` — 渠道生命周期管理 + Outbound Dispatch
//!
//! # 设计亮点（借鉴 nanobot + picoclaw）
//!
//! - **Outbound Dispatch Loop**：独立任务从 `broadcast::Receiver<OutboundMessage>` 消费
//!   出站消息，按 `target_channel` 路由到对应渠道的 `send()` 方法。
//! - **RwLock + 动态注册**：`Arc<tokio::sync::RwLock<HashMap>>` 保护渠道 Map，
//!   支持运行时 `register_channel` / `unregister_channel` 热插拔。
//! - **内部渠道过滤**：`target_channel = "system"` 消息不派发（由 daemon 处理）。

use adaclaw_core::channel::{Channel, MessageBus, OutboundMessage};
use anyhow::Result;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{broadcast, RwLock};

pub struct ChannelManager {
    channels: Arc<RwLock<HashMap<String, Arc<dyn Channel>>>>,
}

impl Default for ChannelManager {
    fn default() -> Self {
        Self::new()
    }
}

impl ChannelManager {
    pub fn new() -> Self {
        Self {
            channels: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// 注册渠道（同步接口，供 `run.rs` 在启动阶段调用）。
    pub fn register(&mut self, channel: Arc<dyn Channel>) {
        // 此时尚未启动，直接 blocking_write 是安全的
        let name = channel.name().to_string();
        // 使用 try_write() 避免 panic（启动阶段一定没有并发读者）
        if let Ok(mut map) = self.channels.try_write() {
            map.insert(name, channel);
        }
    }

    /// 运行时动态注册渠道（参考 picoclaw RegisterChannel）。
    pub async fn register_channel(&self, channel: Arc<dyn Channel>) {
        let name = channel.name().to_string();
        let mut map = self.channels.write().await;
        map.insert(name, channel);
        tracing::info!("Channel registered dynamically");
    }

    /// 运行时动态注销渠道。
    pub async fn unregister_channel(&self, name: &str) {
        let mut map = self.channels.write().await;
        if map.remove(name).is_some() {
            tracing::info!(channel = %name, "Channel unregistered");
        }
    }

    /// 启动所有渠道并运行 Outbound Dispatch Loop。
    ///
    /// # 参数
    /// - `bus`: 用于渠道的 `send_inbound()` 调用
    /// - `outbound_rx`: 从 `AppMessageBus::subscribe_outbound()` 获得的广播接收器
    ///
    /// 此方法是长驻任务，直到所有渠道退出（通常不会）。
    pub async fn start_all(
        &self,
        bus: Arc<dyn MessageBus>,
        outbound_rx: broadcast::Receiver<OutboundMessage>,
    ) -> Result<()> {
        let channels_snapshot = {
            let map = self.channels.read().await;
            map.clone()
        };

        if channels_snapshot.is_empty() {
            tracing::warn!("No channels registered, ChannelManager is idle");
        }

        // ── 1. 启动 Outbound Dispatch Loop ────────────────────────────────────
        let channels_for_dispatch = Arc::clone(&self.channels);
        tokio::spawn(outbound_dispatch_loop(outbound_rx, channels_for_dispatch));

        // ── 2. 并发启动所有渠道 ───────────────────────────────────────────────
        let mut handles = Vec::new();
        for (name, channel) in channels_snapshot {
            let ch = Arc::clone(&channel);
            let bus_clone = Arc::clone(&bus);
            let handle = tokio::spawn(async move {
                tracing::info!(channel = %name, "Starting channel");
                if let Err(e) = ch.start(bus_clone).await {
                    tracing::error!(channel = %name, error = %e, "Channel exited with error");
                } else {
                    tracing::info!(channel = %name, "Channel stopped");
                }
            });
            handles.push(handle);
        }

        // 等待所有渠道任务（它们理论上永远运行）
        for h in handles {
            let _ = h.await;
        }

        Ok(())
    }

    /// 停止所有渠道。
    pub async fn stop_all(&self) -> Result<()> {
        let map = self.channels.read().await;
        for (name, channel) in map.iter() {
            if let Err(e) = channel.stop().await {
                tracing::error!(channel = %name, error = %e, "Error stopping channel");
            } else {
                tracing::info!(channel = %name, "Channel stopped");
            }
        }
        Ok(())
    }

    /// 获取渠道（只读）。
    pub async fn get(&self, name: &str) -> Option<Arc<dyn Channel>> {
        let map = self.channels.read().await;
        map.get(name).cloned()
    }

    /// 获取所有已注册渠道的状态。
    pub async fn get_status(&self) -> HashMap<String, bool> {
        let map = self.channels.read().await;
        map.iter()
            .map(|(name, ch)| (name.clone(), ch.is_running()))
            .collect()
    }

    /// 获取已启用的渠道名列表。
    pub async fn enabled_channels(&self) -> Vec<String> {
        let map = self.channels.read().await;
        map.keys().cloned().collect()
    }
}

// ── Outbound Dispatch Loop ────────────────────────────────────────────────────

/// 持续从广播接收器消费 `OutboundMessage`，按 `target_channel` 派发到对应渠道。
///
/// - 忽略 `target_channel = "system"` 的内部消息
/// - 广播 lagged 时打印警告但继续
/// - 广播关闭时退出
async fn outbound_dispatch_loop(
    mut rx: broadcast::Receiver<OutboundMessage>,
    channels: Arc<RwLock<HashMap<String, Arc<dyn Channel>>>>,
) {
    tracing::info!("Outbound dispatch loop started");

    loop {
        match rx.recv().await {
            Ok(msg) => {
                // 忽略 system 内部消息（sub-agent 结果回传，由 daemon 处理）
                if msg.target_channel == "system" {
                    continue;
                }

                let channel = {
                    let map = channels.read().await;
                    map.get(&msg.target_channel).cloned()
                };

                match channel {
                    Some(ch) => {
                        if let Err(e) = ch.send(msg).await {
                            tracing::error!(error = %e, "Channel send error in outbound dispatch");
                        }
                    }
                    None => {
                        tracing::warn!(
                            target_channel = %msg.target_channel,
                            "Outbound message for unknown channel, dropping"
                        );
                    }
                }
            }

            Err(broadcast::error::RecvError::Lagged(n)) => {
                tracing::warn!(
                    lagged = n,
                    "Outbound dispatch lagged: {} messages dropped",
                    n
                );
            }

            Err(broadcast::error::RecvError::Closed) => {
                tracing::info!("Outbound broadcast closed, dispatch loop exiting");
                break;
            }
        }
    }
}
