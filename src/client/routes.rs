use std::convert::Infallible;

use axum::body::Body;
use axum::extract::{Path, Request, State};
use axum::http::{header, StatusCode, Uri};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{delete, get, post, put};
use axum::{Json, Router};
use futures::StreamExt;
use serde::Deserialize;

use crate::common::fingerprint::MachineInfo;
use crate::server::routes::frontend;

pub fn router() -> Router {
    Router::new()
        .route("/api/local/mode", get(mode))
        .route("/api/local/info", get(local_info))
        .route("/api/local/server/register", post(register_to_server))
        .route("/api/local/status", get(status))
        .route("/api/local/aliases", get(list_aliases).put(upsert_alias))
        .route("/api/local/aliases/{name}", delete(remove_alias))
        .route("/api/local/exec", post(exec))
        .route("/api/local/script", post(gen_script))
        .fallback(local_static)
        .with_state(())
}

// ---- 本地静态资源 ----

async fn local_static(uri: Uri, _req: Request) -> Result<Response, Infallible> {
    frontend::static_handler(uri, _req).await
}

// ---- API ----

async fn mode() -> Json<serde_json::Value> {
    let config = crate::client::config::ClientConfig::load().unwrap_or_default();
    Json(serde_json::json!({
        "mode": "client",
        "server_url": config.server_url,
        "registered": config.registered,
        "client_id": config.client_id,
        "has_token": config.token.is_some(),
    }))
}

async fn local_info() -> Json<serde_json::Value> {
    let info = MachineInfo::collect();
    Json(serde_json::json!({
        "hostname": info.hostname,
        "ip": info.ip,
        "os": info.os,
        "user": info.user,
    }))
}

#[derive(Deserialize)]
struct RegisterBody {
    server_url: String,
}

async fn register_to_server(
    Json(body): Json<RegisterBody>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let server_url = body.server_url.trim_end_matches('/').to_string();
    let info = MachineInfo::collect();
    let resp = crate::client::remote::register(&server_url, &info)
        .await
        .map_err(|e| (StatusCode::BAD_GATEWAY, format!("请求服务器失败: {e}")))?;

    // 保存配置（client_id 用于轮询；token 在 approved 时返回）
    let mut config = crate::client::config::ClientConfig::load().unwrap_or_default();
    config.server_url = server_url;
    config.machine_guid = info.machine_guid;
    config.hostname = info.hostname;
    config.client_id = resp["client_id"].as_str().unwrap_or_default().to_string();
    if let Some(token) = resp["token"].as_str() {
        config.token = Some(token.to_string());
        config.registered = true;
    }
    config.save().map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(resp))
}

async fn status() -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let mut config = crate::client::config::ClientConfig::load()
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    if config.registered || config.client_id.is_empty() {
        return Ok(Json(serde_json::json!({
            "registered": config.registered,
            "server_url": config.server_url,
            "hostname": config.hostname,
        })));
    }
    // 轮询远端审批状态
    let resp = crate::client::remote::register_status(&config.server_url, &config.client_id)
        .await
        .map_err(|e| (StatusCode::BAD_GATEWAY, format!("查询服务器失败: {e}")))?;
    let remote_status = resp["status"].as_str().unwrap_or("pending");
    if remote_status == "approved" {
        config.token = resp["token"].as_str().map(|s| s.to_string());
        config.registered = config.token.is_some();
        config.save().map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    }
    Ok(Json(serde_json::json!({
        "registered": config.registered,
        "status": remote_status,
        "server_url": config.server_url,
        "hostname": config.hostname,
    })))
}

async fn list_aliases() -> Json<serde_json::Value> {
    let aliases = crate::client::config::load_aliases().unwrap_or_default();
    Json(serde_json::to_value(&aliases).unwrap_or_default())
}

#[derive(Deserialize)]
struct AliasBody {
    name: String,
    template: String,
}

async fn upsert_alias(
    Json(body): Json<AliasBody>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    crate::client::script::validate_alias_name(&body.name)
        .map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?;
    crate::client::script::validate_template(&body.template)
        .map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?;
    let mut aliases = crate::client::config::load_aliases()
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    aliases.insert(
        body.name.clone(),
        crate::client::config::AliasDef {
            template: body.template,
        },
    );
    crate::client::config::save_aliases(&aliases)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(serde_json::json!({ "ok": true })))
}

async fn remove_alias(
    Path(name): Path<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let mut aliases = crate::client::config::load_aliases()
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    if aliases.remove(&name).is_none() {
        return Err((StatusCode::NOT_FOUND, "别名不存在".into()));
    }
    crate::client::config::save_aliases(&aliases)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(serde_json::json!({ "ok": true })))
}

#[derive(Deserialize)]
struct ExecBody {
    alias: String,
    #[serde(default)]
    args: Vec<String>,
}

/// 本地执行：解析别名 → 转发远端 → SSE 原样转发给浏览器
async fn exec(Json(body): Json<ExecBody>) -> Result<Response, (StatusCode, String)> {
    let config = crate::client::config::ClientConfig::load()
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    if !config.registered {
        return Err((StatusCode::PRECONDITION_REQUIRED, "尚未完成注册".into()));
    }
    let payload = crate::client::run_cmd::resolve_alias(&body.alias, &body.args)
        .map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?;
    let resp = crate::client::remote::exec_sse(&config.server_url, config.token.as_deref().unwrap_or_default(), &payload)
        .await
        .map_err(|e| {
            // 转发远端错误状态
            let status = e
                .downcast_ref::<reqwest::Error>()
                .and_then(|re| re.status())
                .map(|s| StatusCode::from_u16(s.as_u16()).unwrap_or(StatusCode::BAD_GATEWAY))
                .unwrap_or(StatusCode::BAD_GATEWAY);
            (status, format!("执行请求失败: {e}"))
        })?;
    let status = StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::OK);
    let stream = resp
        .bytes_stream()
        .map(|r| r.map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e)));
    let body = Body::from_stream(stream);
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .body(body)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))
}

#[derive(Deserialize)]
struct ScriptBody {
    alias: String,
}

async fn gen_script(Json(body): Json<ScriptBody>) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let path = crate::client::script::gen_script(&body.alias)
        .map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?;
    let bin_dir = crate::common::paths::client_data_dir().join("bin");
    Ok(Json(serde_json::json!({
        "path": path.display().to_string(),
        "bin_dir": bin_dir.display().to_string(),
    })))
}
