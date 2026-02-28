//! `start_daemon` — AdaClaw 守护进程主入口
//!
//! 启动顺序：
//! 1. 加载配置
//! 2. 安全检查（容器环境检测、Estop 状态恢复、审计日志初始化）
//! 3. 初始化可观察性（Prometheus / log / noop observer + runtime tracer）
//! 4. 初始化记忆后端
//! 5. 初始化 Provider 池
//! 6. 构建 `AgentRegistry`（每个 Agent 创建独立的 `AgentInstance`）
//! 7. 初始化 `MessageBus` + `AgentRouter`
//! 8. 启动渠道管理器
//! 9. 启动 Gateway HTTP 服务器
//! 10. 启动隧道（可选）
//! 11. 启动 Agent 调度循环（含安全检查：Estop / RateLimit / 审计）
//! 12. 等待 Ctrl-C → 优雅关闭

use crate::agents::delegate::DelegateTool;
use crate::agents::engine::AgentEngine;
use crate::agents::instance::AgentInstance;
use crate::agents::registry::AgentRegistry;
use crate::bus::queue::AppMessageBus;
use crate::bus::router::AgentRouter;
use crate::config::schema::{AgentConfig, Config, McpServerConfig as SchemaMcpServerConfig};
use crate::cron::scheduler::HeartbeatScheduler;
use crate::observability::{self, ObserverEvent};
use adaclaw_channels::manager::ChannelManager;
use adaclaw_core::channel::{InboundMessage, MessageBus, MessageContent, OutboundMessage};
use adaclaw_memory::factory::{create_memory, create_memory_with_config, MemoryFactoryConfig};
use adaclaw_providers::router::create_provider;
use adaclaw_security::{
    audit::{AuditKind, AuditLogger},
    estop::EstopController,
    ratelimit::{RateLimitConfig, RateLimiter},
    sandbox::docker::ContainerEnvironment,
};
use adaclaw_server::server::start_server;
use anyhow::Result;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::sync::{broadcast, mpsc};
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};
use uuid::Uuid;

pub async fn start_daemon() -> Result<()> {
    info!("Starting AdaClaw daemon...");

    // ── 1. 加载配置 ────────────────────────────────────────────────────────────
    let config = Config::load();
    info!(
        agents = ?config.agents.keys().collect::<Vec<_>>(),
        memory_backend = %config.memory.backend,
        gateway = %config.gateway.bind,
        autonomy_level = %config.security.autonomy_level,
        observability = %config.observability.backend,
        tunnel = %config.tunnel.provider,
        "Config loaded"
    );

    if let Some(ws) = &config.security.workspace {
        std::env::set_var("ADACLAW_WORKSPACE", ws);
    }

    // ── 2. 安全子系统初始化 ────────────────────────────────────────────────────

    // 2a. 容器环境检测（Full 模式下警告）
    let autonomy_level = config.security.autonomy_level
        .parse::<adaclaw_security::approval::AutonomyLevel>()
        .unwrap_or(adaclaw_security::approval::AutonomyLevel::Supervised);
    if !config.security.allow_full_outside_container {
        if let Some(warning) =
            ContainerEnvironment::check_autonomy_safety(&autonomy_level)
        {
            ContainerEnvironment::print_warning(&warning);
        }
    }

    // 2b. Estop 控制器（从磁盘恢复状态）
    let estop_path = config
        .security
        .estop_state_path
        .as_deref()
        .unwrap_or(".adaclaw/estop.json");
    let estop = Arc::new(EstopController::new(estop_path));

    if estop.is_active() {
        warn!("⚠️  ESTOP IS ACTIVE — agents will be blocked until estop is cleared");
        warn!("   Run `adaclaw stop --clear` to resume normal operation");
    }

    // 2c. 审计日志
    let audit_logger: Option<Arc<AuditLogger>> = if let Some(audit_path) =
        &config.security.audit_log
    {
        match AuditLogger::new(audit_path) {
            Ok(logger) => {
                info!(path = %audit_path, "Audit logger initialized");
                let logger = Arc::new(logger);
                logger.log_started(
                    env!("CARGO_PKG_VERSION"),
                    &config.security.autonomy_level,
                );
                Some(logger)
            }
            Err(e) => {
                warn!(error = %e, path = %audit_path, "Failed to initialize audit logger");
                None
            }
        }
    } else {
        None
    };

    // 2d. 速率限制器
    let rate_limiter = Arc::new(RateLimiter::new(RateLimitConfig {
        per_user: config.security.rate_limit.per_user,
        per_channel: config.security.rate_limit.per_channel,
        max_actions_per_hour: config.security.rate_limit.max_actions_per_hour,
        daily_cost_budget_usd: config.security.rate_limit.daily_cost_budget_usd,
    }));

    info!(
        per_user = %config.security.rate_limit.per_user,
        per_channel = %config.security.rate_limit.per_channel,
        max_actions_per_hour = %config.security.rate_limit.max_actions_per_hour,
        "Rate limiter initialized"
    );

    // ── 3. 可观察性初始化（Phase 7） ──────────────────────────────────────────

    let observer = observability::create_observer(&config.observability.backend);
    info!(
        backend = %config.observability.backend,
        observer = %observer.name(),
        "Observability initialized"
    );

    // Register global observer
    observability::init_global(Arc::clone(&observer));

    // If Prometheus, also register the /metrics encoder with the gateway
    if observer.name() == "prometheus" {
        let obs_for_metrics = Arc::clone(&observer);
        adaclaw_server::routes::metrics::set_metrics_encoder(move || {
            if let Some(p) = obs_for_metrics
                .as_any()
                .downcast_ref::<observability::PrometheusObserver>()
            {
                p.encode()
            } else {
                String::from("# prometheus observer type mismatch\n")
            }
        });
    }

    // Runtime tracer (optional JSONL file)
    let runtime_tracer = config.observability.runtime_trace_path.as_ref().map(|path| {
        let max = config.observability.runtime_trace_max_entries;
        let tracer = crate::observability::RuntimeTracer::new(path, max);
        info!(path = %path, max_entries = max, "Runtime tracer initialized");
        Arc::new(tracer)
    });

    // Log daemon start event
    observability::record(ObserverEvent::ChannelMessage {
        channel: "system".to_string(),
        direction: "inbound".to_string(),
    });

    // ── 4. 记忆后端 ────────────────────────────────────────────────────────────
    let mem_cfg = MemoryFactoryConfig {
        backend: &config.memory.backend,
        path: &config.memory.path,
        embedding_provider: &config.memory.embedding_provider,
        embed_api_key: config.memory.embed_api_key.as_deref(),
        embed_base_url: config.memory.embed_base_url.as_deref(),
    };
    let _memory = match create_memory_with_config(&mem_cfg) {
        Ok(m) => {
            info!(
                backend = %config.memory.backend,
                path = %config.memory.path,
                embedding = %config.memory.embedding_provider,
                "Memory backend initialised"
            );
            Arc::new(m)
        }
        Err(e) => {
            warn!("Failed to open memory backend: {}. Falling back to none.", e);
            Arc::new(create_memory("none", "").expect("none memory always succeeds"))
        }
    };

    // ── 5. Provider 池 ─────────────────────────────────────────────────────────
    let mut providers: HashMap<String, Arc<dyn adaclaw_core::provider::Provider>> = HashMap::new();

    for (name, pcfg) in &config.providers {
        let key = pcfg.api_key.as_deref();
        let url = pcfg.base_url.as_deref();
        match create_provider(name, key, url) {
            Ok(p) => {
                info!("Provider '{}' registered", name);
                providers.insert(name.clone(), Arc::from(p));
            }
            Err(e) => {
                warn!("Failed to create provider '{}': {}", name, e);
            }
        }
    }

    if providers.is_empty()
        && (std::env::var("OPENAI_API_KEY").is_ok()
            || std::env::var("ADACLAW_OPENAI_API_KEY").is_ok())
    {
        let key = std::env::var("OPENAI_API_KEY")
            .or_else(|_| std::env::var("ADACLAW_OPENAI_API_KEY"))
            .ok();
        if let Ok(p) = create_provider("openai", key.as_deref(), None) {
            info!("Auto-detected OpenAI provider from env");
            providers.insert("openai".to_string(), Arc::from(p));
        }
    }

    // ── Phase 10: MCP 工具加载 ─────────────────────────────────────────────────
    let mcp_tools: Arc<Vec<adaclaw_tools::mcp::McpTool>> = if !config.tools.mcp_servers.is_empty() {
        info!(servers = config.tools.mcp_servers.len(), "Loading MCP servers...");
        let loader_configs: std::collections::HashMap<String, adaclaw_tools::mcp::loader::McpServerConfig> =
            config.tools.mcp_servers.iter().map(|(name, cfg)| {
                let lc = match cfg {
                    SchemaMcpServerConfig::Stdio { command, args, env, tool_timeout } =>
                        adaclaw_tools::mcp::loader::McpServerConfig::Stdio {
                            command: command.clone(),
                            args: args.clone(),
                            env: env.clone(),
                            tool_timeout: *tool_timeout,
                        },
                    SchemaMcpServerConfig::Http { url, headers, tool_timeout } =>
                        adaclaw_tools::mcp::loader::McpServerConfig::Http {
                            url: url.clone(),
                            headers: headers.clone(),
                            tool_timeout: *tool_timeout,
                        },
                };
                (name.clone(), lc)
            }).collect();
        let mcp_vec = adaclaw_tools::mcp::loader::McpLoader::load_all_clonable(&loader_configs).await;
        info!(tools = mcp_vec.len(), "MCP tools ready");
        Arc::new(mcp_vec)
    } else {
        Arc::new(vec![])
    };

    // ── 6. 构建 AgentRegistry（含 AgentInstance） ──────────────────────────────
    let mut agents_map = config.agents.clone();
    if !agents_map.contains_key("assistant") {
        agents_map.insert("assistant".to_string(), AgentConfig::default());
    }

    let mut registry = AgentRegistry::new("assistant");

    for (agent_id, agent_cfg) in &agents_map {
        let provider = providers
            .get(&agent_cfg.provider)
            .cloned()
            .or_else(|| providers.values().next().cloned());

        let provider = match provider {
            Some(p) => p,
            None => {
                warn!(
                    "No provider '{}' available for agent '{}', skipping",
                    agent_cfg.provider, agent_id
                );
                continue;
            }
        };

        match AgentInstance::new(agent_id, agent_cfg, provider) {
            Ok(instance) => {
                info!(
                    agent_id = %agent_id,
                    model = %agent_cfg.model,
                    tools = instance.tool_registry.len(),
                    allow_delegate = ?instance.allow_delegate,
                    workspace = %instance.workspace.display(),
                    "Agent instance created"
                );
                registry.insert(instance);
            }
            Err(e) => {
                warn!("Failed to create agent '{}': {}", agent_id, e);
            }
        }
    }

    if registry.is_empty() {
        warn!("No agents registered! Creating a fallback assistant agent.");
        if let Some(provider) = providers.values().next().cloned() {
            let cfg = AgentConfig::default();
            if let Ok(inst) = AgentInstance::new("assistant", &cfg, provider) {
                registry.insert(inst);
            }
        }
    }

    info!(
        agents = ?registry.list_agents(),
        default = %registry.default_agent_id(),
        "Agent registry built"
    );

    let registry = Arc::new(registry);

    // ── 7. Message Bus + AgentRouter ───────────────────────────────────────────
    let (inbound_tx, inbound_rx) = mpsc::channel::<InboundMessage>(256);
    let (outbound_tx, _) = broadcast::channel::<OutboundMessage>(256);
    let bus = Arc::new(AppMessageBus::new(
        inbound_tx,
        outbound_tx,
        config.security.allowlist.clone(),
    ));
    let agent_router = Arc::new(AgentRouter::new(config.routing.clone()));

    // ── 8. 渠道管理器 ──────────────────────────────────────────────────────────
    let mut channel_manager = ChannelManager::new();

    for (chan_name, ch_cfg) in &config.channels {
        match ch_cfg.kind.as_str() {
            "telegram" => {
                if let Some(token) = &ch_cfg.token {
                    let mut ch = adaclaw_channels::telegram::TelegramChannel::new(
                        token.clone(),
                    )
                    .with_allow_from(ch_cfg.allow_from.clone())
                    .with_group_config(ch_cfg.allow_from_groups.clone(), ch_cfg.require_mention);
                    if let Some(proxy) = ch_cfg.extra.get("proxy") {
                        ch = ch.with_proxy(proxy.clone());
                    }
                    channel_manager.register(Arc::new(ch));
                    info!("Registered telegram channel '{}'", chan_name);
                } else {
                    warn!("Telegram channel '{}' has no token, skipping", chan_name);
                }
            }
            "cli" | "" => {
                let ch = Arc::new(adaclaw_channels::cli::CliChannel::new());
                channel_manager.register(ch);
                info!("Registered CLI channel '{}'", chan_name);
            }
            "dingtalk" => {
                let port: u16 = ch_cfg.extra.get("webhook_port").and_then(|s| s.parse().ok()).unwrap_or(9001);
                let path = ch_cfg.extra.get("webhook_path").cloned().unwrap_or_else(|| "/webhook/dingtalk".to_string());
                let ch = Arc::new(adaclaw_channels::dingtalk::DingTalkChannel::new(
                    ch_cfg.webhook_secret.clone(), ch_cfg.allow_from.clone(), port, path,
                ));
                channel_manager.register(ch);
                info!("Registered DingTalk channel '{}' on port {}", chan_name, port);
            }
            "feishu" => {
                let app_id = ch_cfg.extra.get("app_id").cloned().unwrap_or_default();
                let app_secret = ch_cfg.extra.get("app_secret").cloned().unwrap_or_default();
                let vtoken = ch_cfg.extra.get("verification_token").cloned();
                let port: u16 = ch_cfg.extra.get("webhook_port").and_then(|s| s.parse().ok()).unwrap_or(9002);
                let path = ch_cfg.extra.get("webhook_path").cloned().unwrap_or_else(|| "/webhook/feishu".to_string());
                let ch = Arc::new(adaclaw_channels::feishu::FeishuChannel::new(
                    app_id, app_secret, vtoken, ch_cfg.allow_from.clone(), port, path,
                ));
                channel_manager.register(ch);
                info!("Registered Feishu channel '{}' on port {}", chan_name, port);
            }
            "wechat_work" | "wecom" => {
                let token = ch_cfg.token.clone().unwrap_or_default();
                let aes_key = ch_cfg.extra.get("encoding_aes_key").cloned();
                let port: u16 = ch_cfg.extra.get("webhook_port").and_then(|s| s.parse().ok()).unwrap_or(9003);
                let path = ch_cfg.extra.get("webhook_path").cloned().unwrap_or_else(|| "/webhook/wecom".to_string());
                let ch = Arc::new(adaclaw_channels::wechat_work::WeComChannel::new(
                    token, aes_key, ch_cfg.allow_from.clone(), port, path,
                ));
                channel_manager.register(ch);
                info!("Registered WeCom channel '{}' on port {}", chan_name, port);
            }
            "discord" => {
                if let Some(token) = &ch_cfg.token {
                    let intents: Option<u64> = ch_cfg.extra.get("intents").and_then(|s| s.parse().ok());
                    let ch = Arc::new(adaclaw_channels::discord::DiscordChannel::new(
                        token.clone(), ch_cfg.allow_from.clone(), intents,
                    ));
                    channel_manager.register(ch);
                    info!("Registered Discord channel '{}'", chan_name);
                } else {
                    warn!("Discord channel '{}' has no token, skipping", chan_name);
                }
            }
            "slack" => {
                if let Some(token) = &ch_cfg.token {
                    let signing_secret = ch_cfg.webhook_secret.clone().or_else(|| ch_cfg.extra.get("signing_secret").cloned());
                    let port: u16 = ch_cfg.extra.get("webhook_port").and_then(|s| s.parse().ok()).unwrap_or(9004);
                    let path = ch_cfg.extra.get("webhook_path").cloned().unwrap_or_else(|| "/webhook/slack".to_string());
                    let ch = Arc::new(adaclaw_channels::slack::SlackChannel::new(
                        token.clone(), signing_secret, ch_cfg.allow_from.clone(), port, path,
                    ));
                    channel_manager.register(ch);
                    info!("Registered Slack channel '{}' on port {}", chan_name, port);
                } else {
                    warn!("Slack channel '{}' has no token, skipping", chan_name);
                }
            }
            "webhook" => {
                let port: u16 = ch_cfg.extra.get("webhook_port").and_then(|s| s.parse().ok()).unwrap_or(9005);
                let path = ch_cfg.extra.get("webhook_path").cloned().unwrap_or_else(|| "/webhook/custom".to_string());
                let outbound_url = ch_cfg.extra.get("outbound_url").cloned();
                let ch = Arc::new(adaclaw_channels::webhook::WebhookChannel::new(
                    ch_cfg.webhook_secret.clone(), ch_cfg.allow_from.clone(), port, path, outbound_url,
                ));
                channel_manager.register(ch);
                info!("Registered Webhook channel '{}' on port {}", chan_name, port);
            }
            other => {
                warn!("Unknown channel kind '{}' for '{}', skipping", other, chan_name);
            }
        }
    }

    // ── 取消令牌 ────────────────────────────────────────────────────────────────
    let cancel = CancellationToken::new();

    // ── 9. Gateway HTTP 服务器 ──────────────────────────────────────────────────
    let gateway_addr: SocketAddr = config
        .gateway
        .bind
        .parse()
        .unwrap_or_else(|_| "127.0.0.1:8080".parse().unwrap());
    let cancel_gw = cancel.clone();
    tokio::spawn(async move {
        tokio::select! {
            res = start_server(gateway_addr) => {
                if let Err(e) = res {
                    error!("Gateway error: {}", e);
                }
            }
            _ = cancel_gw.cancelled() => {
                info!("Gateway shutting down");
            }
        }
    });
    info!("Gateway listening on http://{}", gateway_addr);
    if config.observability.backend == "prometheus" {
        info!("Metrics available at http://{}/metrics", gateway_addr);
    }

    // ── 渠道管理器 ──────────────────────────────────────────────────────────────
    let outbound_rx = bus.subscribe_outbound();
    let bus_dyn: Arc<dyn MessageBus> = Arc::clone(&bus) as Arc<dyn MessageBus>;
    let cancel_ch = cancel.clone();
    tokio::spawn(async move {
        tokio::select! {
            res = channel_manager.start_all(bus_dyn, outbound_rx) => {
                if let Err(e) = res {
                    error!("Channel manager error: {}", e);
                }
            }
            _ = cancel_ch.cancelled() => {
                info!("Channels shutting down");
            }
        }
    });

    // ── 10. 隧道启动（Phase 7） ─────────────────────────────────────────────────
    let gateway_port: u16 = gateway_addr.port();
    let _tunnel_handle = if config.tunnel.provider != "none" && !config.tunnel.provider.is_empty() {
        let handle = crate::tunnel::start_tunnel(
            &config.tunnel.provider,
            gateway_port,
            config.tunnel.cloudflare_token.as_deref(),
            config.tunnel.ngrok_token.as_deref(),
            config.tunnel.ngrok_domain.as_deref(),
            config.tunnel.tailscale_funnel,
        );
        if let Some(ref h) = handle {
            info!(
                provider = %h.provider,
                public_url = ?h.public_url,
                "Tunnel started"
            );
        }
        handle
    } else {
        None
    };

    // ── Phase 10: Heartbeat 调度器 ─────────────────────────────────────────────
    if config.heartbeat.enabled {
        let heartbeat_scheduler = HeartbeatScheduler::new(
            config.heartbeat.clone(),
            config
                .security
                .workspace
                .as_deref()
                .unwrap_or("workspace")
                .to_string(),
        );
        let bus_heartbeat: Arc<dyn MessageBus> = Arc::clone(&bus) as Arc<dyn MessageBus>;
        let cancel_hb = cancel.clone();
        tokio::spawn(async move {
            tokio::select! {
                res = heartbeat_scheduler.start(bus_heartbeat) => {
                    if let Err(e) = res {
                        error!("Heartbeat scheduler error: {}", e);
                    }
                }
                _ = cancel_hb.cancelled() => {
                    info!("Heartbeat scheduler shutting down");
                }
            }
        });
        info!(
            interval_mins = config.heartbeat.interval_minutes,
            "Heartbeat scheduler started"
        );
    }

    // ── 11. Agent 调度循环 ───────────────────────────────────────────────────────
    let cancel_agent = cancel.clone();
    let observer_clone = Arc::clone(&observer);
    let tracer_clone = runtime_tracer.clone();
    tokio::spawn(agent_dispatch_loop(
        inbound_rx,
        bus,
        agent_router,
        registry,
        estop,
        rate_limiter,
        audit_logger,
        observer_clone,
        tracer_clone,
        Arc::clone(&mcp_tools),
        cancel_agent,
    ));

    info!("AdaClaw daemon running (autonomy: {}). Press Ctrl-C to stop.",
          config.security.autonomy_level);

    // ── 12. 等待 Ctrl-C → 优雅关闭 ────────────────────────────────────────────
    tokio::signal::ctrl_c()
        .await
        .expect("Failed to listen for ctrl_c");
    info!("Shutting down gracefully...");

    // Flush observer
    observer.flush();

    cancel.cancel();
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    info!("Daemon stopped.");
    Ok(())
}

// ── Agent 调度循环 ────────────────────────────────────────────────────────────

#[allow(clippy::too_many_arguments)]
async fn agent_dispatch_loop(
    mut inbound_rx: mpsc::Receiver<InboundMessage>,
    bus: Arc<AppMessageBus>,
    router: Arc<AgentRouter>,
    registry: Arc<AgentRegistry>,
    estop: Arc<EstopController>,
    rate_limiter: Arc<RateLimiter>,
    audit_logger: Option<Arc<AuditLogger>>,
    observer: Arc<dyn observability::Observer>,
    tracer: Option<Arc<crate::observability::RuntimeTracer>>,
    mcp_tools: Arc<Vec<adaclaw_tools::mcp::McpTool>>,
    cancel: CancellationToken,
) {
    loop {
        tokio::select! {
            Some(msg) = inbound_rx.recv() => {
                // ── 特殊处理：system channel ──────────────────────────────────
                if msg.channel == "system" {
                    handle_system_message(msg, &bus);
                    continue;
                }

                // ── Observe inbound message ───────────────────────────────────
                observer.record_event(&ObserverEvent::ChannelMessage {
                    channel: msg.channel.clone(),
                    direction: "inbound".to_string(),
                });

                // ── 1. Estop 检查 ─────────────────────────────────────────────
                if estop.is_killed() {
                    warn!(
                        sender = %msg.sender_id,
                        channel = %msg.channel,
                        "Message rejected: KillAll estop is active"
                    );
                    if let Some(ref logger) = audit_logger {
                        logger.log(adaclaw_security::audit::AuditEvent::new(
                            AuditKind::UnauthorizedAccess {
                                sender_id: msg.sender_id.clone(),
                                channel: msg.channel.clone(),
                                reason: "KillAll estop is active".to_string(),
                            }
                        ));
                    }
                    let out = OutboundMessage {
                        id: Uuid::new_v4(),
                        target_channel: msg.channel.clone(),
                        target_session_id: msg.session_id.clone(),
                        content: MessageContent::Text(
                            "🚨 Emergency stop is active. All operations are suspended.".to_string()
                        ),
                        reply_to: Some(msg.id),
                    };
                    let _ = bus.send_outbound(out);
                    continue;
                }

                // ── 2. 速率限制检查 ───────────────────────────────────────────
                if let Err(reason) = rate_limiter.check_message(&msg.sender_id, &msg.channel) {
                    warn!(
                        sender = %msg.sender_id,
                        channel = %msg.channel,
                        reason = %reason,
                        "Message rejected: rate limit exceeded"
                    );
                    if let Some(ref logger) = audit_logger {
                        logger.log_rate_limit(&msg.sender_id, &msg.channel, "per_user");
                    }
                    observer.record_event(&ObserverEvent::Error {
                        component: "ratelimit".to_string(),
                        message: reason.clone(),
                    });
                    let out = OutboundMessage {
                        id: Uuid::new_v4(),
                        target_channel: msg.channel.clone(),
                        target_session_id: msg.session_id.clone(),
                        content: MessageContent::Text(reason),
                        reply_to: Some(msg.id),
                    };
                    let _ = bus.send_outbound(out);
                    continue;
                }

                // ── 3. 审计：消息接收 ─────────────────────────────────────────
                if let Some(ref logger) = audit_logger {
                    let preview = match &msg.content {
                        MessageContent::Text(t) => t.chars().take(100).collect::<String>(),
                        MessageContent::Image(_) => "[image]".to_string(),
                        MessageContent::Audio(_) => "[audio]".to_string(),
                        MessageContent::File { name, .. } => format!("[file: {}]", name),
                    };
                    logger.log_message(&msg.channel, &msg.sender_id, &preview);
                }

                // ── 提取文本内容 ──────────────────────────────────────────────
                let text = match &msg.content {
                    MessageContent::Text(t) => t.clone(),
                    _ => {
                        warn!(sender = %msg.sender_id, "Non-text message received, ignoring");
                        continue;
                    }
                };

                // ── 路由到目标 Agent ──────────────────────────────────────────
                let agent_id = router.route_or(&msg, registry.default_agent_id());

                let instance = match registry.get(&agent_id) {
                    Some(i) => i,
                    None => {
                        warn!(agent = %agent_id, "Agent not found, trying default");
                        match registry.get_default() {
                            Some(i) => i,
                            None => {
                                error!("No agents available to handle message");
                                continue;
                            }
                        }
                    }
                };

                // ── 构建工具列表：基础工具 + MCP 工具 + 可选 DelegateTool ────
                let mut tools = instance.build_tools();

                // Phase 10: 注入 MCP 工具（透明包装，与原生工具同等对待）
                for mcp_tool in mcp_tools.iter() {
                    tools.push(Box::new(mcp_tool.clone()));
                }

                if instance.can_delegate() {
                    tools.push(Box::new(DelegateTool::new(
                        instance.agent_id.clone(),
                        Arc::clone(&registry),
                        Arc::clone(&bus),
                    )));
                }

                // ── 提取执行参数 ──────────────────────────────────────────────
                let provider = Arc::clone(&instance.provider);
                let model = instance.model.clone();
                let temperature = instance.temperature;
                let max_iterations = instance.max_iterations;
                let system_extra = instance.system_extra.clone();
                let agent_id_owned = instance.agent_id.clone();
                let session_id = msg.session_id.clone();
                let target_channel = msg.channel.clone();
                let reply_to = Some(msg.id);
                let bus_ref = Arc::clone(&bus);
                let estop_ref = Arc::clone(&estop);
                let audit_ref = audit_logger.clone();
                let observer_ref = Arc::clone(&observer);
                let tracer_ref = tracer.clone();

                // ── 审计：Agent 启动 ──────────────────────────────────────────
                if let Some(ref logger) = audit_logger {
                    logger.log(
                        adaclaw_security::audit::AuditEvent::new(
                            AuditKind::AgentStarted {
                                agent_id: agent_id_owned.clone(),
                                model: model.clone(),
                            }
                        ).with_agent(&agent_id_owned).with_channel(&target_channel)
                    );
                }

                // ── Observe agent turn start ──────────────────────────────────
                observer.record_event(&ObserverEvent::AgentTurn {
                    agent_id: agent_id_owned.clone(),
                    provider: instance.provider.name().to_string(),
                    model: model.clone(),
                });

                // ── spawn：每条消息独立任务 ────────────────────────────────────
                let turn_start = std::time::Instant::now();
                tokio::spawn(async move {
                    // Check estop again before running
                    if estop_ref.is_tool_frozen() {
                        let out = OutboundMessage {
                            id: Uuid::new_v4(),
                            target_channel: target_channel.clone(),
                            target_session_id: session_id,
                            content: MessageContent::Text(
                                "⚠️ Tools are currently frozen (estop active).".to_string(),
                            ),
                            reply_to,
                        };
                        let _ = bus_ref.send_outbound(out);
                        return;
                    }

                    let engine = AgentEngine::new();
                    let result = engine
                        .run_tool_loop_with_options(
                            provider.as_ref(),
                            &tools,
                            &text,
                            &model,
                            temperature,
                            system_extra.as_deref(),
                            Some(max_iterations),
                        )
                        .await;

                    let duration = turn_start.elapsed();

                    match result {
                        Ok(response) => {
                            // Record success in observer
                            observer_ref.record_event(&ObserverEvent::AgentTurnEnd {
                                agent_id: agent_id_owned.clone(),
                                provider: "".to_string(),
                                model: model.clone(),
                                duration,
                                success: true,
                            });

                            // Record in runtime tracer
                            if let Some(ref tracer) = tracer_ref {
                                tracer.record_simple(
                                    "agent_turn_end",
                                    Some(&agent_id_owned),
                                    Some(&target_channel),
                                    None,
                                    Some(&model),
                                    Some(true),
                                    None,
                                    Some(duration.as_millis() as u64),
                                );
                            }

                            // Record outbound message
                            observer_ref.record_event(&ObserverEvent::ChannelMessage {
                                channel: target_channel.clone(),
                                direction: "outbound".to_string(),
                            });

                            let out = OutboundMessage {
                                id: Uuid::new_v4(),
                                target_channel,
                                target_session_id: session_id,
                                content: MessageContent::Text(response),
                                reply_to,
                            };
                            if let Err(e) = bus_ref.send_outbound(out) {
                                warn!("Failed to send outbound message: {}", e);
                            }
                        }
                        Err(e) => {
                            error!(agent = %agent_id_owned, error = %e, "Agent error");

                            // Record failure in observer
                            observer_ref.record_event(&ObserverEvent::AgentTurnEnd {
                                agent_id: agent_id_owned.clone(),
                                provider: "".to_string(),
                                model: model.clone(),
                                duration,
                                success: false,
                            });
                            observer_ref.record_event(&ObserverEvent::Error {
                                component: "agent".to_string(),
                                message: e.to_string(),
                            });

                            if let Some(ref logger) = audit_ref {
                                logger.log(
                                    adaclaw_security::audit::AuditEvent::new(
                                        AuditKind::AgentError {
                                            agent_id: agent_id_owned.clone(),
                                            error: e.to_string(),
                                        }
                                    ).with_agent(&agent_id_owned)
                                );
                            }
                        }
                    }
                });
            }

            _ = cancel.cancelled() => {
                info!("Agent dispatch loop shutting down");
                break;
            }
        }
    }
}

// ── System Channel 处理 ───────────────────────────────────────────────────────

fn handle_system_message(msg: InboundMessage, bus: &Arc<AppMessageBus>) {
    let text = match &msg.content {
        MessageContent::Text(t) => t.clone(),
        _ => {
            warn!("system message with non-text content, ignoring");
            return;
        }
    };

    let origin_channel = msg
        .metadata
        .get("origin_channel")
        .and_then(|v| v.as_str())
        .unwrap_or_else(|| {
            msg.session_id
                .split('\x1E')
                .next()
                .unwrap_or("cli")
        })
        .to_string();

    let origin_session = msg
        .metadata
        .get("origin_session")
        .and_then(|v| v.as_str())
        .unwrap_or_else(|| {
            msg.session_id
                .split('\x1E')
                .nth(1)
                .unwrap_or("default")
        })
        .to_string();

    let task_id = msg
        .metadata
        .get("task_id")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");

    tracing::debug!(
        task_id = %task_id,
        origin_channel = %origin_channel,
        origin_session = %origin_session,
        "Routing sub-agent result back to origin"
    );

    let out = OutboundMessage {
        id: Uuid::new_v4(),
        target_channel: origin_channel,
        target_session_id: origin_session,
        content: MessageContent::Text(text),
        reply_to: None,
    };

    if let Err(e) = bus.send_outbound(out) {
        warn!(error = %e, "Failed to route sub-agent result back to origin channel");
    }
}
