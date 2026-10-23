//! HTTP routes, wire shapes, validation, auth, and error mapping.

#[derive(Clone)]
pub struct AppState;

pub fn router(_state: AppState) -> axum::Router {
    axum::Router::new().route("/health", axum::routing::get(|| async { "ok" }))
}
