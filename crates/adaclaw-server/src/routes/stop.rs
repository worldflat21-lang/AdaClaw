use axum::Json;
use serde_json::{Value, json};

pub async fn stop() -> Json<Value> {
    Json(json!({ "status": "estop_engaged" }))
}
