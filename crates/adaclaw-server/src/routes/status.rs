use axum::Json;
use serde_json::{Value, json};

pub async fn status() -> Json<Value> {
    Json(json!({ "status": "ok", "version": env!("CARGO_PKG_VERSION") }))
}
