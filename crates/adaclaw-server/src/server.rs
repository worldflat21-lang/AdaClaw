use anyhow::Result;
use axum::{
    routing::{get, post},
    Router,
};
use std::net::SocketAddr;
use tokio::net::TcpListener;
use tracing::info;

use crate::routes::whatsapp::WhatsAppRouteState;

/// 启动不带 WhatsApp 的基础 Gateway 服务器
pub async fn start_server(addr: SocketAddr) -> Result<()> {
    let app = build_router(None);

    info!("Starting gateway server on {}", addr);
    let listener = TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

/// 启动带 WhatsApp 路由的 Gateway 服务器（共享 HTTPS 端口）
///
/// 当 WhatsApp Webhook 需要通过主 Gateway 端口（配合 HTTPS 隧道）处理时使用。
/// 独立端口模式（默认）请使用 `WhatsAppChannel::start()`，无需此函数。
pub async fn start_server_with_whatsapp(
    addr: SocketAddr,
    whatsapp: Option<WhatsAppRouteState>,
) -> Result<()> {
    let app = build_router(whatsapp);

    info!("Starting gateway server on {}", addr);
    let listener = TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

/// 构建 axum Router
///
/// - `whatsapp`: 若为 `Some`，挂载 `/whatsapp` GET + POST 路由
pub fn build_router(whatsapp: Option<WhatsAppRouteState>) -> Router {
    let mut app = Router::new()
        .route("/v1/status", get(crate::routes::status::status))
        .route("/v1/chat", post(crate::routes::chat::chat))
        .route("/v1/stop", post(crate::routes::stop::stop))
        .route("/pair", get(crate::pairing::pair))
        .route("/metrics", get(crate::routes::metrics::metrics));

    // WhatsApp 路由（可选，共享 HTTPS 端口模式）
    if let Some(wa_state) = whatsapp {
        let wa_router = Router::new()
            .route(
                "/whatsapp",
                get(crate::routes::whatsapp::whatsapp_verify)
                    .post(crate::routes::whatsapp::whatsapp_receive),
            )
            .with_state(wa_state);
        app = app.merge(wa_router);
        info!("WhatsApp routes mounted at /whatsapp (gateway mode)");
    }

    app
}
