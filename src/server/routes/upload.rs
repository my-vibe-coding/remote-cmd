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
/// 协议：init 文本帧（name/size/session）→ Binary 数据帧×N → upload_end 文本帧
/// → 应答 uploaded（path = 落地绝对路径）/ rejected 文本帧。
/// 客户端由 PTY 会话的拖放/粘贴路径检测触发（见 client/run_cmd.rs），文件落地
/// Unix 时优先所传会话 shell 进程 cwd、否则数据目录 uploads/ 下，同名自动加 _N 后缀，不做覆盖。
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
    /// 发起上传的 exec 会话 id：服务端据此定位该会话 shell 进程 cwd 作为落地目录
    #[serde(default)]
    session: String,
}

fn rejected(reason: &str) -> Message {
    Message::Text(serde_json::json!({ "type": "rejected", "reason": reason }).to_string().into())
}

/// 上传落地目录：Unix 经 cwd 解析链取所传 exec 会话（shell）进程的 cwd
/// （tmux 感知：tmux 内 cd 后取活动 pane 实时 cwd，见 common/cwd.rs；拖放上传语义
/// = 文件落在 shell 当前工作目录）；会话 id 缺失/未知/归属客户端不符、解析失败
/// 或 Windows 服务端 → 默认 uploads 目录
async fn upload_dir(state: &AppState, client_id: &str, session: Option<&str>) -> PathBuf {
    let pid = session.and_then(|s| state.pty_pid(s, client_id));
    let Some(pid) = pid else {
        macro_log::d!("[upload] 回退默认目录: session={session:?} 无已登记 pid（会话未登记/已结束/客户端未回传 session）");
        return state.data_dir.join("uploads");
    };
    if let Some(cwd) = crate::common::cwd::resolve_process_cwd(pid).await {
        macro_log::d!("[upload] 落地进程 cwd: pid={pid} dir={cwd}");
        return PathBuf::from(cwd);
    }
    macro_log::d!("[upload] 回退默认目录: pid={pid} cwd 解析链失败（见 [cwd] 日志）");
    state.data_dir.join("uploads")
}

async fn session(
    mut socket: WebSocket,
    state: Arc<AppState>,
    client: crate::server::store::ClientRecord,
    addr: SocketAddr,
) {
    let hostname = client.hostname.clone();

    // init 帧（15s 超时）
    let init = match tokio::time::timeout(Duration::from_secs(15), read_init(&mut socket)).await {
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
    let Some(name) = sanitize_name(&init.name) else {
        let _ = socket.send(rejected("文件名非法")).await;
        let _ = socket.close().await;
        return;
    };

    // 落地目录：Unix 优先取该客户端 exec 会话（shell）进程的 cwd——拖放上传语义
    // 即"文件落在 shell 当前工作目录"；会话 id 无效/读取失败/Windows → 默认 uploads 目录。
    // 同名加 _N 后缀，不覆盖已有文件。
    let session = (!init.session.is_empty()).then_some(init.session.as_str());
    let dir = upload_dir(&state, &client.id, session).await;
    if let Err(e) = tokio::fs::create_dir_all(&dir).await {
        macro_log::w!("[upload] 创建上传目录失败 {}: {e}", dir.display());
        let _ = socket.send(rejected("服务端上传目录创建失败")).await;
        let _ = socket.close().await;
        return;
    }
    let (dest, mut file) = match open_unique_file(&dir, &name).await {
        Ok(v) => v,
        Err(e) => {
            macro_log::w!("[upload] 创建文件失败 {}/{}: {e}", dir.display(), name);
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
        Some(serde_json::json!({ "name": name, "bytes": total, "path": path, "dir": dir.display().to_string(), "ip": addr.ip().to_string() })),
    );
    let _ = socket
        .send(Message::Text(
            serde_json::json!({ "type": "uploaded", "path": path, "bytes": total }).to_string().into(),
        ))
        .await;
    let _ = socket.close().await;
    macro_log::d!("[upload] 接收完成 {name} ({total} bytes) -> {path} from {hostname}");
}

async fn read_init(socket: &mut WebSocket) -> Result<UploadInit, String> {
    loop {
        match socket.recv().await {
            Some(Ok(Message::Text(t))) => {
                let v: UploadInit = serde_json::from_str(&t).map_err(|e| format!("init 帧无效: {e}"))?;
                return Ok(v);
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

/// 原子创建不覆盖的落地文件：`create_new`（不存在才创建）替代 exists+create 两步
/// 操作，消除同名并发上传的 TOCTOU 竞态（先查后建互相截断）。冲突时加 _N 后缀重试。
fn create_new_opts() -> tokio::fs::OpenOptions {
    let mut o = tokio::fs::OpenOptions::new();
    o.write(true).create_new(true);
    o
}

async fn open_unique_file(dir: &std::path::Path, name: &str) -> std::io::Result<(PathBuf, tokio::fs::File)> {
    // 首选原名
    let dest = dir.join(name);
    match create_new_opts().open(&dest).await {
        Ok(f) => return Ok((dest, f)),
        Err(e) if e.kind() != std::io::ErrorKind::AlreadyExists => return Err(e),
        _ => {}
    }
    // 同名冲突：a.txt → a_1.txt、a_2.txt …（create_new 原子抢位，天然并发安全）
    // 隐藏文件（.gitignore）整体视为无扩展名
    let (stem, ext) = match name.rsplit_once('.') {
        Some((stem, _)) if !stem.is_empty() => (&name[..stem.len()], &name[stem.len()..]),
        _ => (name, ""),
    };
    for i in 1..1000u32 {
        let cand = dir.join(format!("{stem}_{i}{ext}"));
        match create_new_opts().open(&cand).await {
            Ok(f) => return Ok((cand, f)),
            Err(e) if e.kind() != std::io::ErrorKind::AlreadyExists => return Err(e),
            _ => {}
        }
    }
    // 兜底：时间戳名（1000 个后缀占满的极端场景）
    let cand = dir.join(format!("{stem}_{}{ext}", chrono::Utc::now().timestamp_millis()));
    let f = create_new_opts().open(&cand).await?;
    Ok((cand, f))
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

    /// 原子不覆盖创建：同名自动 _N 后缀，已有文件内容不被截断（create_new 语义）
    #[tokio::test]
    async fn open_unique_file_creates_without_overwrite() {
        let dir = std::env::temp_dir().join(format!("rc-upload-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();

        // 首选原名创建
        let (p1, mut f1) = open_unique_file(&dir, "a.txt").await.unwrap();
        assert_eq!(p1, dir.join("a.txt"));
        use tokio::io::AsyncWriteExt as _;
        f1.write_all(b"original").await.unwrap();
        f1.sync_all().await.unwrap();

        // 同名 → a_1.txt；原文件内容不被截断
        let (p2, f2) = open_unique_file(&dir, "a.txt").await.unwrap();
        assert_eq!(p2, dir.join("a_1.txt"));
        drop(f2);
        assert_eq!(std::fs::read(dir.join("a.txt")).unwrap(), b"original");

        // 无扩展名 → b_1
        let (p3, f3) = open_unique_file(&dir, "b").await.unwrap();
        assert_eq!(p3, dir.join("b"));
        drop(f3);
        // 隐藏文件整体视为无扩展名：先占用原名 → 后缀
        std::fs::write(dir.join(".gitignore"), "x").unwrap();
        let (p4, f4) = open_unique_file(&dir, ".gitignore").await.unwrap();
        assert_eq!(p4, dir.join(".gitignore_1"));
        drop(f4);
        // 后缀计数中断续接：再建一个 a.txt → a_2.txt
        let (p5, f5) = open_unique_file(&dir, "a.txt").await.unwrap();
        assert_eq!(p5, dir.join("a_2.txt"));
        drop(f5);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
