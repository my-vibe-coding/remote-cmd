use axum::extract::{ConnectInfo, Path, Query, State};
use axum::http::StatusCode;
use axum::Json;
use chrono::Utc;
use serde::Deserialize;
use std::net::SocketAddr;
use std::sync::Arc;

use crate::common::crypto;
use crate::server::auth::AdminAuth;
use crate::server::rate_limit::{LOGIN_LIMIT, LOGIN_WINDOW};
use crate::server::state::AppState;
use crate::server::store::{ClientStatus, ClientSummary};

#[derive(Deserialize)]
pub struct LoginRequest {
    pub password: String,
}

#[derive(serde::Serialize)]
pub struct LoginResponse {
    pub session_token: String,
    pub expires_at: String,
}

/// POST /api/admin/login
pub async fn login(
    State(state): State<Arc<AppState>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    Json(req): Json<LoginRequest>,
) -> Result<Json<LoginResponse>, (StatusCode, String)> {
    if !state.rate.check("login", addr.ip(), LOGIN_LIMIT, LOGIN_WINDOW) {
        return Err((StatusCode::TOO_MANY_REQUESTS, "登录尝试过于频繁，请稍后再试".into()));
    }
    if !crypto::verify_password(&req.password, &state.admin_password_hash) {
        state.audit.log(
            "admin.login_failed",
            "anonymous",
            Some(serde_json::json!({ "ip": addr.ip().to_string() })),
        );
        return Err((StatusCode::UNAUTHORIZED, "密码错误".into()));
    }
    let token = state.create_session();
    state.audit.log(
        "admin.login",
        "admin",
        Some(serde_json::json!({ "ip": addr.ip().to_string() })),
    );
    Ok(Json(LoginResponse {
        session_token: token,
        expires_at: (Utc::now() + chrono::Duration::seconds(crate::server::state::SESSION_TTL.as_secs() as i64))
            .to_rfc3339(),
    }))
}

#[derive(Deserialize)]
pub struct ListQuery {
    pub status: Option<String>,
    pub limit: Option<usize>,
}

/// GET /api/admin/registrations?status=
pub async fn registrations(
    State(state): State<Arc<AppState>>,
    _admin: AdminAuth,
    Query(q): Query<ListQuery>,
) -> Json<Vec<ClientSummary>> {
    let status = q.status.as_deref().map(parse_status);
    let list = state
        .store
        .list()
        .into_iter()
        .filter(|c| status.map(|s| c.status == s).unwrap_or(true))
        .map(|c| ClientSummary::from(&c))
        .collect();
    Json(list)
}

/// POST /api/admin/registrations/{id}/approve
pub async fn approve(
    State(state): State<Arc<AppState>>,
    _admin: AdminAuth,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let record = state.store.update(&id, |c| {
        c.status = ClientStatus::Approved;
        c.token = Some(crypto::random_token());
        c.approved_at = Some(Utc::now().to_rfc3339());
    }).map_err(internal)?;
    let Some(record) = record else {
        return Err((StatusCode::NOT_FOUND, "申请不存在".into()));
    };
    state.audit.log(
        "register.approve",
        "admin",
        Some(serde_json::json!({ "client_id": record.id, "hostname": record.hostname })),
    );
    Ok(Json(serde_json::json!({ "client_id": record.id, "token": record.token })))
}

/// POST /api/admin/registrations/{id}/reject
pub async fn reject(
    State(state): State<Arc<AppState>>,
    _admin: AdminAuth,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let record = state
        .store
        .update(&id, |c| {
            c.status = ClientStatus::Rejected;
            c.token = None;
        })
        .map_err(internal)?;
    let Some(record) = record else {
        return Err((StatusCode::NOT_FOUND, "申请不存在".into()));
    };
    state.audit.log(
        "register.reject",
        "admin",
        Some(serde_json::json!({ "client_id": record.id, "hostname": record.hostname })),
    );
    Ok(Json(serde_json::json!({ "ok": true })))
}

/// DELETE /api/admin/registrations/{id}
pub async fn remove(
    State(state): State<Arc<AppState>>,
    _admin: AdminAuth,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let removed = state.store.remove(&id).map_err(internal)?;
    if !removed {
        return Err((StatusCode::NOT_FOUND, "记录不存在".into()));
    }
    state.audit.log("register.delete", "admin", Some(serde_json::json!({ "client_id": id })));
    Ok(Json(serde_json::json!({ "ok": true })))
}

/// GET /api/admin/credentials
pub async fn credentials(
    State(state): State<Arc<AppState>>,
    _admin: AdminAuth,
) -> Json<Vec<ClientSummary>> {
    Json(
        state
            .store
            .list()
            .into_iter()
            .filter(|c| matches!(c.status, ClientStatus::Approved | ClientStatus::Revoked))
            .map(|c| ClientSummary::from(&c))
            .collect(),
    )
}

#[derive(Deserialize)]
pub struct WhitelistRequest {
    pub programs: Vec<String>,
}

/// PUT /api/admin/credentials/{id}/whitelist
pub async fn set_whitelist(
    State(state): State<Arc<AppState>>,
    _admin: AdminAuth,
    Path(id): Path<String>,
    Json(req): Json<WhitelistRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let mut seen = std::collections::HashSet::new();
    let programs: Vec<String> = req
        .programs
        .into_iter()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .filter(|s| seen.insert(s.to_lowercase()))
        .collect();
    let record = state
        .store
        .update(&id, |c| c.whitelist = programs.clone())
        .map_err(internal)?;
    let Some(record) = record else {
        return Err((StatusCode::NOT_FOUND, "凭证不存在".into()));
    };
    state.audit.log(
        "credential.update",
        "admin",
        Some(serde_json::json!({ "client_id": id, "whitelist": programs })),
    );
    Ok(Json(serde_json::json!({ "ok": true, "whitelist": record.whitelist })))
}

/// DELETE /api/admin/credentials/{id}：吊销凭证
pub async fn revoke(
    State(state): State<Arc<AppState>>,
    _admin: AdminAuth,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let record = state
        .store
        .update(&id, |c| {
            c.status = ClientStatus::Revoked;
            c.token = None;
        })
        .map_err(internal)?;
    let Some(record) = record else {
        return Err((StatusCode::NOT_FOUND, "凭证不存在".into()));
    };
    state.audit.log(
        "credential.revoke",
        "admin",
        Some(serde_json::json!({ "client_id": id, "hostname": record.hostname })),
    );
    Ok(Json(serde_json::json!({ "ok": true })))
}

/// GET /api/admin/audit?limit=
pub async fn audit(
    State(state): State<Arc<AppState>>,
    _admin: AdminAuth,
    Query(q): Query<ListQuery>,
) -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "audit": state.audit.query(q.limit.unwrap_or(200)),
        "executions": state.audit.query_execs(q.limit.unwrap_or(200)),
    }))
}

fn parse_status(s: &str) -> ClientStatus {
    match s.to_lowercase().as_str() {
        "approved" => ClientStatus::Approved,
        "rejected" => ClientStatus::Rejected,
        "revoked" => ClientStatus::Revoked,
        _ => ClientStatus::Pending,
    }
}

fn internal(e: crate::common::AppError) -> (StatusCode, String) {
    macro_log::e!("内部错误: {e}");
    (StatusCode::INTERNAL_SERVER_ERROR, "内部错误".into())
}
