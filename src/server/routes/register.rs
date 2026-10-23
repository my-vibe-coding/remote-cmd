use axum::extract::{ConnectInfo, Path, State};
use axum::http::StatusCode;
use axum::Json;
use chrono::Utc;
use std::net::SocketAddr;
use std::sync::Arc;
use uuid::Uuid;

use crate::common::fingerprint::{validate_info, MachineInfo};
use crate::server::state::AppState;
use crate::server::store::{ClientRecord, ClientStatus};

#[derive(serde::Serialize)]
pub struct RegisterResponse {
    pub client_id: String,
    pub status: ClientStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hostname: Option<String>,
}

/// POST /api/register：指纹幂等的注册申请/凭证领取
pub async fn register(
    State(state): State<Arc<AppState>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    Json(info): Json<MachineInfo>,
) -> Result<Json<RegisterResponse>, (StatusCode, String)> {
    if !state
        .rate
        .check("register", addr.ip(), crate::server::rate_limit::REGISTER_LIMIT, crate::server::rate_limit::REGISTER_WINDOW)
    {
        return Err((StatusCode::TOO_MANY_REQUESTS, "注册请求过于频繁，请稍后再试".into()));
    }
    validate_info(&info).map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?;

    let fp = info.fingerprint();
    if let Some(existing) = state.store.find_by_fingerprint(&fp) {
        let status = existing.status;
        state
            .store
            .update(&existing.id, |c| c.last_seen = Some(Utc::now().to_rfc3339()))
            .map_err(internal)?;
        return Ok(Json(RegisterResponse {
            client_id: existing.id,
            status,
            token: existing.token,
            hostname: Some(existing.hostname),
        }));
    }

    let record = ClientRecord {
        id: Uuid::new_v4().to_string(),
        fingerprint: fp,
        hostname: info.hostname.trim().to_string(),
        ip: addr.ip().to_string(),
        os: info.os.clone(),
        user: info.user.clone(),
        status: ClientStatus::Pending,
        token: None,
        whitelist: vec![],
        created_at: Utc::now().to_rfc3339(),
        approved_at: None,
        last_seen: None,
    };
    state.audit.log(
        "register.submit",
        &record.hostname,
        Some(serde_json::json!({
            "ip": record.ip, "os": record.os, "user": record.user,
            "fingerprint": record.fingerprint,
        })),
    );
    let client_id = record.id.clone();
    state.store.insert(record).map_err(internal)?;
    Ok(Json(RegisterResponse {
        client_id,
        status: ClientStatus::Pending,
        token: None,
        hostname: None,
    }))
}

/// GET /api/register/status/{client_id}：轮询审批状态
pub async fn status(
    State(state): State<Arc<AppState>>,
    Path(client_id): Path<String>,
) -> Result<Json<RegisterResponse>, (StatusCode, String)> {
    let Some(record) = state.store.find_by_id(&client_id) else {
        return Err((StatusCode::NOT_FOUND, "申请不存在".into()));
    };
    Ok(Json(RegisterResponse {
        client_id: record.id,
        status: record.status,
        token: record.token,
        hostname: Some(record.hostname),
    }))
}

fn internal(e: crate::common::AppError) -> (StatusCode, String) {
    macro_log::e!("内部错误: {e}");
    (StatusCode::INTERNAL_SERVER_ERROR, "内部错误".into())
}
