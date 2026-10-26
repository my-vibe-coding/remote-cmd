use std::collections::HashMap;
use std::convert::Infallible;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use axum::extract::{Path, Query, Request};
use axum::http::{StatusCode, Uri};
use axum::response::Response;
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use serde::Deserialize;

use crate::client::config::ServerReg;
use crate::common::fingerprint::MachineInfo;
use crate::server::routes::frontend;

pub fn router() -> Router {
    Router::new()
        .route("/api/local/mode", get(mode))
        .route("/api/local/info", get(local_info))
        .route("/api/local/server/register", post(register_to_server))
        .route("/api/local/server/unregister", post(unregister_server))
        .route("/api/local/status", get(status))
        .route("/api/local/servers", get(list_servers))
        .route("/api/local/aliases", get(list_aliases).put(upsert_alias))
        .route("/api/local/aliases/{name}", delete(remove_alias))
        .route("/api/local/aliases/{name}/script", delete(delete_alias_script))
        .route("/api/local/script/reveal", post(reveal_script))
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
    let registered = config.default_server().is_some();
    Json(serde_json::json!({
        "mode": "client",
        "server_url": config.default_server().map(|s| s.server_url.clone()).unwrap_or_default(),
        "registered": registered,
        "client_id": config.default_server().map(|s| s.client_id.clone()).unwrap_or_default(),
        "has_token": registered,
        "servers": config.servers.iter().map(server_json).collect::<Vec<_>>(),
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

/// 向某台服务器注册/重新注册（多服务器：按地址 upsert）
async fn register_to_server(
    Json(body): Json<RegisterBody>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let server_url = body.server_url.trim_end_matches('/').to_string();
    if server_url.is_empty() {
        return Err((StatusCode::BAD_REQUEST, "服务器地址不能为空".into()));
    }
    let info = MachineInfo::collect();
    let resp = crate::client::remote::register(&server_url, &info)
        .await
        .map_err(|e| (StatusCode::BAD_GATEWAY, format!("请求服务器失败: {e}")))?;

    // 保存配置（client_id 用于轮询；token 在 approved 时返回）
    let mut config = crate::client::config::ClientConfig::load()
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    config.machine_guid = info.machine_guid;
    config.hostname = info.hostname.clone();
    let client_id = resp["client_id"].as_str().unwrap_or_default().to_string();
    let token = resp["token"].as_str().map(|s| s.to_string());
    let registered = token.is_some();

    if let Some(existing) = config.servers.iter_mut().find(|s| s.server_url == server_url) {
        existing.client_id = client_id;
        existing.token = token;
        existing.registered = registered;
    } else {
        config.servers.push(ServerReg {
            server_url: server_url.clone(),
            client_id,
            hostname: info.hostname.clone(),
            token: token.clone(),
            registered,
        });
    }
    config.save().map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    invalidate_status_cache(&server_url);
    Ok(Json(resp))
}

#[derive(Deserialize)]
struct UnregisterBody {
    server_url: String,
}

/// 注销：调用服务器删除注册记录（凭 token），成功后移除本地记录
async fn unregister_server(
    Json(body): Json<UnregisterBody>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let server_url = body.server_url.trim_end_matches('/').to_string();
    let mut config = crate::client::config::ClientConfig::load()
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let Some(entry) = config.find_server(&server_url).cloned() else {
        return Err((StatusCode::NOT_FOUND, "未找到该服务器的注册记录".into()));
    };
    if let Some(token) = &entry.token {
        match crate::client::remote::unregister(&server_url, token).await {
            Ok(()) => {}
            Err(e) => {
                // 已吊销/记录不存在：服务器侧已无凭证，本地可直接移除；网络错误则保留以便重试
                let gone = e
                    .downcast_ref::<reqwest::Error>()
                    .and_then(|re| re.status())
                    .is_some_and(|s| s.as_u16() == 401 || s.as_u16() == 404);
                if !gone {
                    return Err((StatusCode::BAD_GATEWAY, format!("注销请求失败: {e}")));
                }
            }
        }
    }
    config.servers.retain(|s| s.server_url != server_url);
    config.save().map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    invalidate_status_cache(&server_url);
    Ok(Json(serde_json::json!({ "ok": true })))
}

fn server_json(s: &ServerReg) -> serde_json::Value {
    serde_json::json!({
        "server_url": s.server_url,
        "client_id": s.client_id,
        "hostname": s.hostname,
        "registered": s.registered,
    })
}

async fn list_servers() -> Json<serde_json::Value> {
    let config = crate::client::config::ClientConfig::load().unwrap_or_default();
    Json(serde_json::json!({
        "servers": config.servers.iter().map(server_json).collect::<Vec<_>>(),
    }))
}

// ---- 远端状态查询缓存（避免前端高频轮询打爆服务器） ----

const STATUS_TTL: Duration = Duration::from_secs(10);

type StatusCache = Mutex<HashMap<String, (Instant, Result<serde_json::Value, (u16, String)>)>>;

fn status_cache() -> &'static StatusCache {
    static CACHE: OnceLock<StatusCache> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn invalidate_status_cache(server_url: &str) {
    status_cache().lock().unwrap().remove(server_url);
}

/// 查询远端注册状态（带缓存）；Ok((status, resp)) / Err((http_status, msg))
async fn fetch_remote_status(
    server_url: &str,
    client_id: &str,
) -> Result<(String, serde_json::Value), (u16, String)> {
    if let Some((at, cached)) = status_cache().lock().unwrap().get(server_url)
        && at.elapsed() < STATUS_TTL {
            return cached
                .as_ref()
                .map(|v| (v["status"].as_str().unwrap_or("pending").to_string(), v.clone()))
                .map_err(|e| e.clone());
        }
    let result = crate::client::remote::register_status(server_url, client_id)
        .await
        .map_err(|e| {
            let code = e
                .downcast_ref::<reqwest::Error>()
                .and_then(|re| re.status())
                .map(|s| s.as_u16())
                .unwrap_or(0);
            (code, format!("{e}"))
        });
    let value = match &result {
        Ok(v) => Ok(v.clone()),
        Err(e) => Err(e.clone()),
    };
    status_cache().lock().unwrap().insert(server_url.to_string(), (Instant::now(), value));
    result.map(|v| (v["status"].as_str().unwrap_or("pending").to_string(), v))
}

/// 整体状态：逐台服务器感知吊销/删除/审批结果
async fn status() -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let mut config = crate::client::config::ClientConfig::load()
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let mut changed = false;
    let mut entries = Vec::new();

    for reg in config.servers.clone() {
        let mut status = if reg.registered { "approved" } else { "pending" }.to_string();
        if !reg.client_id.is_empty() {
            match fetch_remote_status(&reg.server_url, &reg.client_id).await {
                Ok((remote_status, _)) => {
                    match remote_status.as_str() {
                        "approved" => {
                            // status 端点不再下发 token：本地无凭证时凭指纹重新 POST /api/register 幂等领取
                            if reg.token.is_none()
                                && let Ok(claim) =
                                    crate::client::remote::register(&reg.server_url, &MachineInfo::collect()).await
                                    && let Some(t) = claim["token"].as_str()
                                        && let Some(e) = config
                                            .servers
                                            .iter_mut()
                                            .find(|s| s.server_url == reg.server_url)
                                        {
                                            e.token = Some(t.to_string());
                                            e.registered = true;
                                            changed = true;
                                        }
                            status = "approved".into();
                        }
                        "rejected" | "revoked" => {
                            if let Some(e) =
                                config.servers.iter_mut().find(|s| s.server_url == reg.server_url)
                                && (e.token.is_some() || e.registered) {
                                    e.token = None;
                                    e.registered = false;
                                    changed = true;
                                }
                            status = remote_status;
                        }
                        other => status = other.to_string(),
                    }
                }
                Err((code, _)) => {
                    if code == 404 {
                        // 服务端已删除该记录：保留本地数据并标记，允许重新注册
                        status = "deleted".into();
                    } else {
                        status = "unreachable".into();
                    }
                }
            }
        }
        entries.push(serde_json::json!({
            "server_url": reg.server_url,
            "client_id": reg.client_id,
            "hostname": reg.hostname,
            "registered": reg.registered,
            "status": status,
        }));
    }
    if changed {
        config.save().map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    }
    let registered = config.default_server().is_some();
    Ok(Json(serde_json::json!({
        "registered": registered,
        "server_url": config.default_server().map(|s| s.server_url.clone()).unwrap_or_default(),
        "hostname": config.hostname,
        "servers": entries,
    })))
}

#[derive(Deserialize)]
struct AliasQuery {
    /// 存在时检查每个别名对应脚本是否已存在，存在则附加 script 字段（脚本路径）
    script: Option<String>,
}

async fn list_aliases(Query(q): Query<AliasQuery>) -> Json<serde_json::Value> {
    let aliases = crate::client::config::load_aliases().unwrap_or_default();
    let mut value = serde_json::to_value(&aliases).unwrap_or_default();
    if q.script.is_some()
        && let Some(map) = value.as_object_mut()
    {
        for (name, v) in map.iter_mut() {
            let path = crate::client::script::script_path(name);
            if path.is_file()
                && let Some(obj) = v.as_object_mut()
            {
                obj.insert(
                    "script".into(),
                    serde_json::Value::String(path.display().to_string()),
                );
            }
        }
    }
    Json(value)
}

#[derive(Deserialize)]
struct AliasBody {
    name: String,
    template: String,
    #[serde(default)]
    server: Option<String>,
}

async fn upsert_alias(
    Json(body): Json<AliasBody>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    crate::client::script::validate_alias_name(&body.name)
        .map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?;
    crate::client::script::validate_template(&body.template)
        .map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?;
    // 绑定服务器需已存在（pending 也允许，审批通过后即可用）
    let server = body
        .server
        .map(|s| s.trim().trim_end_matches('/').to_string())
        .filter(|s| !s.is_empty());
    if let Some(url) = &server {
        let config = crate::client::config::ClientConfig::load().unwrap_or_default();
        if config.find_server(url).is_none() {
            return Err((StatusCode::BAD_REQUEST, format!("未注册的服务器: {url}")));
        }
    }
    let mut aliases = crate::client::config::load_aliases()
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    aliases.insert(
        body.name.clone(),
        crate::client::config::AliasDef {
            template: body.template,
            server,
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

/// 本地执行：解析别名（含服务器绑定）→ WS 会话（stdin 立即 EOF）→ 映射回 SSE 给浏览器
async fn exec(Json(body): Json<ExecBody>) -> Result<Response, (StatusCode, String)> {
    use axum::response::sse::{Event, KeepAlive, Sse};
    use axum::response::IntoResponse as _;
    use base64::Engine as _;
    use futures::{SinkExt as _, StreamExt as _};
    use tokio_tungstenite::tungstenite::Message as WsMessage;

    let (server, payload) = crate::client::run_cmd::resolve_exec_target(&body.alias, &body.args)
        .map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?;
    let token = server.token.clone().unwrap_or_default();
    let program = payload["program"].as_str().unwrap_or_default().to_string();
    let args: Vec<String> = serde_json::from_value(payload["args"].clone())
        .map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?;
    let mut ws = crate::client::remote::ws_exec(&server.server_url, &token, &program, &args, None, false, None)
        .await
        .map_err(|e| (StatusCode::BAD_GATEWAY, format!("执行请求失败: {e}")))?;

    // Web 页无 stdin 交互：立即 EOF
    let _ = ws.sink.send(WsMessage::Text(crate::client::remote::STDIN_CLOSE.into())).await;

    // WS 帧 → SSE 事件（与旧 SSE 端点的事件名/负载形状一致，浏览器零改动）
    let stream = futures::stream::unfold(ws.stream, |mut stream| async move {
        loop {
            match stream.next().await {
                Some(Ok(frame)) => match crate::client::remote::parse_frame(frame) {
                    Some(crate::client::remote::ExecFrame::Output { stream: s, data }) => {
                        let data_b64 = base64::engine::general_purpose::STANDARD.encode(data);
                        let json = serde_json::json!({ "stream": s, "data_b64": data_b64 });
                        return Some((Ok::<_, std::convert::Infallible>(Event::default().event("output").data(json.to_string())), stream));
                    }
                    Some(crate::client::remote::ExecFrame::Exit { exit_code, timeout, duration_ms, truncated }) => {
                        let json = serde_json::json!({
                            "exit_code": exit_code, "timeout": timeout,
                            "duration_ms": duration_ms, "truncated": truncated,
                        });
                        return Some((Ok(Event::default().event("exit").data(json.to_string())), stream));
                    }
                    None => continue,
                },
                Some(Err(_)) | None => return None,
            }
        }
    });
    Ok(Sse::new(stream).keep_alive(KeepAlive::default()).into_response())
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

/// 删除别名对应的 .bat 脚本（脚本不存在时同样返回成功）
async fn delete_alias_script(
    Path(name): Path<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let path = crate::client::script::script_path(&name);
    if path.is_file() {
        std::fs::remove_file(&path)
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("删除脚本失败: {e}")))?;
    }
    Ok(Json(serde_json::json!({ "ok": true })))
}

#[derive(Deserialize)]
struct RevealBody {
    alias: String,
}

/// 在系统文件管理器中定位别名脚本
async fn reveal_script(
    Json(body): Json<RevealBody>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let path = crate::client::script::script_path(&body.alias);
    if !path.is_file() {
        return Err((StatusCode::NOT_FOUND, "脚本不存在，请先生成".into()));
    }
    crate::client::script::reveal_in_explorer(&path).map_err(|e| {
        (StatusCode::INTERNAL_SERVER_ERROR, format!("打开文件管理器失败: {e}"))
    })?;
    Ok(Json(serde_json::json!({ "ok": true })))
}
