use axum::{response::IntoResponse, Json};
use rand::Rng;
use serde_json::json;

pub async fn pair() -> impl IntoResponse {
    let code = generate_pairing_code();
    Json(json!({ "pairing_code": code }))
}

pub fn generate_pairing_code() -> String {
    let mut rng = rand::thread_rng();
    let code: u32 = rng.gen_range(100_000..999_999);
    code.to_string()
}
