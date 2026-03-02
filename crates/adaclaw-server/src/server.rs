use anyhow::Result;
use axum::{
    Router, middleware,
    routing::{get, post},
};
use std::net::SocketAddr;
use tokio::net::TcpListener;
use tracing::info;

use crate::middleware::require_auth;
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
///
/// ## 认证
/// `/v1/chat` 和 `/v1/stop` 受 Bearer Token 中间件保护（见 `middleware::require_auth`）。
/// `/v1/status`、`/pair`、`/metrics` 为公开端点，不要求认证。
pub fn build_router(whatsapp: Option<WhatsAppRouteState>) -> Router {
    // P0-3: 受保护路由（需要 Bearer Token 认证）
    let protected = Router::new()
        .route("/v1/chat", post(crate::routes::chat::chat))
        .route("/v1/stop", post(crate::routes::stop::stop))
        .layer(middleware::from_fn(require_auth));

    // 公开路由（不要求认证）
    let mut app = Router::new()
        .route("/v1/status", get(crate::routes::status::status))
        .route("/pair", get(crate::pairing::pair))
        .route("/metrics", get(crate::routes::metrics::metrics))
        .merge(protected);

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
