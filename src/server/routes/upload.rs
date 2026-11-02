use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{ConnectInfo, State};
use axum::response::Response;
use futures::SinkExt;
use serde::Deserialize;
use tokio::io::AsyncWriteExt;

use crate::server::auth::ClientAuth;
use crate::server::state::AppState;

/// 单文件上传上限
const MAX_UPLOAD_BYTES: u64 = 2 * 1024 * 1024 * 1024;

/// GET /api/upload/ws：WS 连接即单文件上传会话（upgrade 前经 ClientAuth 认证）。
/// 协议：init 文本帧（name/size）→ Binary 数据帧×N → upload_end 文本帧
/// → 应答 uploaded（path = 落地绝对路径）/ rejected 文本帧。
/// 客户端由 PTY 会话的拖放/粘贴路径检测触发（见 client/run_cmd.rs），文件落地
/// 服务端数据目录 uploads/ 下，同名自动加 _N 后缀，不做覆盖。
pub async fn upload_ws(
    State(state): State<Arc<AppState>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    ClientAuth(client): ClientAuth,
    ws: WebSocketUpgrade,
) -> Response {
    ws.on_upgrade(move |socket| session(socket, state, client, addr))
}

#[derive(Deserialize)]
struct UploadInit {
    name: String,
}

fn rejected(reason: &str) -> Message {
    Message::Text(serde_json::json!({ "type": "rejected", "reason": reason }).to_string().into())
}

async fn session(
    mut socket: WebSocket,
    state: Arc<AppState>,
    client: crate::server::store::ClientRecord,
    addr: SocketAddr,
) {
    let hostname = client.hostname.clone();

    // init 帧（15s 超时）
    let name = match tokio::time::timeout(Duration::from_secs(15), read_init(&mut socket)).await {
        Ok(Ok(v)) => v,
        Ok(Err(reason)) => {
            let _ = socket.send(rejected(&reason)).await;
            let _ = socket.close().await;
            return;
        }
        Err(_) => {
            let _ = socket.send(rejected("等待 init 帧超时")).await;
            let _ = socket.close().await;
            return;
        }
    };

    // 文件名净化：仅保留文件名部分，拒绝空/特殊名/控制字符（防路径穿越）
    let Some(name) = sanitize_name(&name) else {
        let _ = socket.send(rejected("文件名非法")).await;
        let _ = socket.close().await;
        return;
    };

    // 落地目录：<data_dir>/uploads；同名加 _N 后缀，不覆盖已有文件
    let dir = state.data_dir.join("uploads");
    if let Err(e) = tokio::fs::create_dir_all(&dir).await {
        macro_log::w!("[upload] 创建上传目录失败 {}: {e}", dir.display());
        let _ = socket.send(rejected("服务端上传目录创建失败")).await;
        let _ = socket.close().await;
        return;
    }
    let dest = unique_path(dir.join(&name));
    let mut file = match tokio::fs::File::create(&dest).await {
        Ok(f) => f,
        Err(e) => {
            macro_log::w!("[upload] 创建文件失败 {}: {e}", dest.display());
            let _ = socket.send(rejected("服务端文件写入失败")).await;
            let _ = socket.close().await;
            return;
        }
    };
    macro_log::d!("[upload] 开始接收 {} -> {} (from {} {})", name, dest.display(), hostname, addr.ip());

    // 数据泵：Binary 帧 → 落盘；upload_end 文本帧 → 正常收尾
    let mut total: u64 = 0;
    let mut done = false;
    while let Some(msg) = socket.recv().await {
        match msg {
            Ok(Message::Binary(data)) => {
                total += data.len() as u64;
                if total > MAX_UPLOAD_BYTES {
                    macro_log::w!("[upload] 上传超限 ({total} bytes): {} from {}", name, hostname);
                    break;
                }
                if file.write_all(&data).await.is_err() {
                    macro_log::w!("[upload] 文件写入失败: {} from {}", name, hostname);
                    break;
                }
            }
            Ok(Message::Text(t)) => {
                let is_end = serde_json::from_str::<serde_json::Value>(&t)
                    .ok()
                    .and_then(|v| v["type"].as_str().map(|s| s == "upload_end"))
                    .unwrap_or(false);
                if is_end {
                    done = true;
                    break;
                }
            }
            Ok(Message::Close(_)) => break,
            Err(e) => {
                macro_log::w!("[upload] 连接读错误: {e} (name={name})");
                break;
            }
            _ => {}
        }
    }

    let _ = file.flush().await;
    drop(file);
    if !done {
        // 非正常收尾（断连/超限/写失败）：删除残留部分文件
        let _ = tokio::fs::remove_file(&dest).await;
        let _ = socket.close().await;
        macro_log::d!("[upload] 会话未正常收尾，已清理残留: {}", dest.display());
        return;
    }

    let path = display_path(&dest);
    state.audit.log(
        "file.upload",
        &hostname,
        Some(serde_json::json!({ "name": name, "bytes": total, "path": path, "ip": addr.ip().to_string() })),
    );
    let _ = socket
        .send(Message::Text(
            serde_json::json!({ "type": "uploaded", "path": path, "bytes": total }).to_string().into(),
        ))
        .await;
    let _ = socket.close().await;
    macro_log::d!("[upload] 接收完成 {name} ({total} bytes) -> {path} from {hostname}");
}

async fn read_init(socket: &mut WebSocket) -> Result<String, String> {
    loop {
        match socket.recv().await {
            Some(Ok(Message::Text(t))) => {
                let v: UploadInit = serde_json::from_str(&t).map_err(|e| format!("init 帧无效: {e}"))?;
                return Ok(v.name);
            }
            Some(Ok(_)) => return Err("首帧必须为 init".into()),
            Some(Err(e)) => return Err(format!("连接错误: {e}")),
            None => return Err("连接已关闭".into()),
        }
    }
}

/// 文件名净化：取最后一节文件名（兼容 / 与 \ 分隔），拒绝空、`.`/`..`、控制字符
fn sanitize_name(raw: &str) -> Option<String> {
    let name = raw.rsplit(['/', '\\']).next().unwrap_or("").trim();
    if name.is_empty() || name == "." || name == ".." {
        return None;
    }
    if name.chars().any(|c| c.is_control()) {
        return None;
    }
    Some(name.to_string())
}

/// 同名不覆盖：`a.txt` 已存在 → `a_1.txt`、`a_2.txt` …
fn unique_path(dest: PathBuf) -> PathBuf {
    if !dest.exists() {
        return dest;
    }
    let stem = dest
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let ext = dest
        .extension()
        .map(|s| format!(".{}", s.to_string_lossy()))
        .unwrap_or_default();
    for i in 1..1000u32 {
        let cand = dest.with_file_name(format!("{stem}_{i}{ext}"));
        if !cand.exists() {
            return cand;
        }
    }
    dest.with_file_name(format!("{stem}_{}{ext}", chrono::Utc::now().timestamp_millis()))
}

/// 落地路径展示：canonicalize 取绝对路径；剥离 Windows `\\?\` 前缀
fn display_path(p: &std::path::Path) -> String {
    let s = std::fs::canonicalize(p)
        .map(|c| c.to_string_lossy().into_owned())
        .unwrap_or_else(|_| p.to_string_lossy().into_owned());
    s.strip_prefix(r"\\?\").map(|s| s.to_string()).unwrap_or(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_name_strips_path_components() {
        assert_eq!(sanitize_name("a.txt"), Some("a.txt".into()));
        assert_eq!(sanitize_name("/etc/passwd"), Some("passwd".into()));
        assert_eq!(sanitize_name("C:\\evil\\a.txt"), Some("a.txt".into()));
        assert_eq!(sanitize_name("../a.txt"), Some("a.txt".into()));
        assert_eq!(sanitize_name("  a b.txt  "), Some("a b.txt".into()));
        assert_eq!(sanitize_name(""), None);
        assert_eq!(sanitize_name(".."), None);
        assert_eq!(sanitize_name("a\u{0}b"), None);
    }

    #[test]
    fn unique_path_appends_suffix_without_overwrite() {
        let dir = std::env::temp_dir().join(format!("rc-upload-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let base = dir.join("a.txt");
        std::fs::write(&base, "x").unwrap();
        assert_eq!(unique_path(base.clone()), dir.join("a_1.txt"));
        std::fs::write(dir.join("a_1.txt"), "x").unwrap();
        assert_eq!(unique_path(base), dir.join("a_2.txt"));
        // 无扩展名
        let noext = dir.join("b");
        std::fs::write(&noext, "x").unwrap();
        assert_eq!(unique_path(noext), dir.join("b_1"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
