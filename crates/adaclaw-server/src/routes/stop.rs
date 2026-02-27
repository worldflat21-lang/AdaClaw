use axum::Json;
use serde_json::{json, Value};

pub async fn stop() -> Json<Value> {
    Json(json!({ "status": "estop_engaged" }))
}
