use anyhow::Result;
use axum::{
    routing::{get, post},
    Router,
};
use std::net::SocketAddr;
use tokio::net::TcpListener;
use tracing::info;

pub async fn start_server(addr: SocketAddr) -> Result<()> {
    let app = Router::new()
        .route("/v1/status", get(crate::routes::status::status))
        .route("/v1/chat", post(crate::routes::chat::chat))
        .route("/v1/stop", post(crate::routes::stop::stop))
        .route("/pair", get(crate::pairing::pair))
        .route("/metrics", get(crate::routes::metrics::metrics));

    info!("Starting gateway server on {}", addr);
    let listener = TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}
