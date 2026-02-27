use axum::Json;
use serde_json::{json, Value};

pub async fn chat() -> Json<Value> {
    Json(json!({ "error": "Not implemented" }))
}
