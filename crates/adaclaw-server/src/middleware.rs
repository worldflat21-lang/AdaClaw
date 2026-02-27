// Auth, RateLimit, CORS middlewares
use axum::{
    extract::Request,
    middleware::Next,
    response::Response,
};

pub async fn require_auth(req: Request, next: Next) -> Response {
    // TODO: Implement proper authentication
    next.run(req).await
}
