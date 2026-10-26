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
        // 已吊销/已拒绝的记录：重新注册时重置为待审批，需管理员重新批准
        if matches!(status, ClientStatus::Revoked | ClientStatus::Rejected) {
            let record = state
                .store
                .update(&existing.id, |c| {
                    c.status = ClientStatus::Pending;
                    c.token = None;
                    c.last_seen = Some(Utc::now().to_rfc3339());
                })
                .map_err(internal)?;
            let record = record.unwrap_or(existing);
            state.audit.log(
                "register.reapply",
                &record.hostname,
                Some(serde_json::json!({ "client_id": record.id, "prev_status": status })),
            );
            return Ok(Json(RegisterResponse {
                client_id: record.id,
                status: ClientStatus::Pending,
                token: None,
                hostname: Some(record.hostname),
            }));
        }
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

/// DELETE /api/register：客户端凭 token 自助注销（删除注册记录与凭证）
pub async fn self_unregister(
    State(state): State<Arc<AppState>>,
    headers: axum::http::HeaderMap,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let token = headers
        .get("x-client-token")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    let Some(record) = state.store.find_by_token(token) else {
        return Err((StatusCode::UNAUTHORIZED, "客户端凭证无效".into()));
    };
    let removed = state.store.remove(&record.id).map_err(internal)?;
    if !removed {
        return Err((StatusCode::NOT_FOUND, "记录不存在".into()));
    }
    state.audit.log(
        "register.unregister",
        &record.hostname,
        Some(serde_json::json!({ "client_id": record.id })),
    );
    Ok(Json(serde_json::json!({ "ok": true })))
}

/// GET /api/register/status/{client_id}：轮询审批状态。
/// 安全：不返回 token（审批通过后由客户端凭指纹重新 POST /api/register 领取），并限流防滥用轮询。
pub async fn status(
    State(state): State<Arc<AppState>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    Path(client_id): Path<String>,
) -> Result<Json<RegisterResponse>, (StatusCode, String)> {
    if !state
        .rate
        .check("register", addr.ip(), crate::server::rate_limit::REGISTER_LIMIT, crate::server::rate_limit::REGISTER_WINDOW)
    {
        return Err((StatusCode::TOO_MANY_REQUESTS, "请求过于频繁，请稍后再试".into()));
    }
    let Some(record) = state.store.find_by_id(&client_id) else {
        return Err((StatusCode::NOT_FOUND, "申请不存在".into()));
    };
    Ok(Json(RegisterResponse {
        client_id: record.id,
        status: record.status,
        token: None,
        hostname: Some(record.hostname),
    }))
}

fn internal(e: crate::common::AppError) -> (StatusCode, String) {
    macro_log::e!("内部错误: {e}");
    (StatusCode::INTERNAL_SERVER_ERROR, "内部错误".into())
}
