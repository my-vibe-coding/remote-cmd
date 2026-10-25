pub mod admin;
pub mod frontend;
pub mod register;
pub mod exec_ws;

use axum::routing::{delete, get, post, put};
use axum::{Json, Router};
use serde_json::json;
use std::sync::Arc;

use crate::server::state::AppState;

pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/api/health", get(health))
        .route(
            "/api/register",
            post(register::register).delete(register::self_unregister),
        )
        .route("/api/register/status/{client_id}", get(register::status))
        .route("/api/exec/ws", get(exec_ws::exec_ws))
        .route("/api/admin/login", post(admin::login))
        .route("/api/admin/registrations", get(admin::registrations))
        .route("/api/admin/registrations/{id}/approve", post(admin::approve))
        .route("/api/admin/registrations/{id}/reject", post(admin::reject))
        .route("/api/admin/registrations/{id}", delete(admin::remove))
        .route("/api/admin/credentials", get(admin::credentials))
        .route("/api/admin/credentials/{id}/whitelist", put(admin::set_whitelist))
        .route("/api/admin/credentials/{id}", delete(admin::revoke))
        .route("/api/admin/audit", get(admin::audit))
        .fallback(frontend::static_handler)
        .with_state(state)
}

async fn health() -> Json<serde_json::Value> {
    Json(json!({ "ok": true }))
}
