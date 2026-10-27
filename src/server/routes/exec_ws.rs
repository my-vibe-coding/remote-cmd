use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{ConnectInfo, State};
use axum::response::Response;
use futures::{SinkExt, StreamExt};
use serde::Deserialize;

use crate::server::auth::ClientAuth;
use crate::server::runner::{self, ExecEvent, ExecRequest, StdinMsg, DEFAULT_TIMEOUT_SECS, MAX_TIMEOUT_SECS};
use crate::server::state::AppState;

const MAX_STDIN_FRAME: usize = 64 * 1024;
const MAX_STDIN_TOTAL: u64 = 10 * 1024 * 1024;

/// GET /api/exec/ws：WS 连接即执行会话（upgrade 前经 ClientAuth 认证）
pub async fn exec_ws(
    State(state): State<Arc<AppState>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    ClientAuth(client): ClientAuth,
    ws: WebSocketUpgrade,
) -> Response {
    ws.on_upgrade(move |socket| session(socket, state, client, addr))
}

#[derive(Deserialize)]
struct WsInit {
    program: String,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    timeout_secs: Option<u64>,
    /// 请求 PTY 会话（伪终端 + ANSI 透传，无超时）
    #[serde(default)]
    tty: bool,
    /// 客户端本地终端尺寸（PTY 会话初始尺寸，避免 80x24 初始竞态）
    #[serde(default)]
    cols: Option<u16>,
    #[serde(default)]
    rows: Option<u16>,
}

fn rejected(reason: &str) -> Message {
    Message::Text(serde_json::json!({ "type": "rejected", "reason": reason }).to_string().into())
}

async fn session(mut socket: WebSocket, state: Arc<AppState>, client: crate::server::store::ClientRecord, addr: SocketAddr) {
    let hostname = client.hostname.clone();

    // 并发限制（连接期间持有）
    let global = state.global_sem.clone();
    let per_client = state.per_client(&client.id);
    let permits = tokio::join!(global.acquire(), per_client.acquire());
    let (global_permit, client_permit) = match permits {
        (Ok(g), Ok(c)) => (g, c),
        _ => {
            let _ = socket.send(rejected("并发已达上限")).await;
            let _ = socket.close().await;
            return;
        }
    };
    let _ = (global_permit, client_permit);

    // 首帧 init（15s 超时防止许可被空占）
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

    // 白名单校验
    if init.program.trim().is_empty() || !runner::whitelist_matches(&client.whitelist, &init.program) {
        state.audit.log(
            "exec.denied",
            &hostname,
            Some(serde_json::json!({ "program": init.program, "args": init.args })),
        );
        let _ = socket.send(rejected(&format!("命令未在白名单中: {}", init.program))).await;
        let _ = socket.close().await;
        return;
    }

    // PTY 会话无超时（交互式长连接，断连即终止）
    let timeout_secs = if init.tty { None } else { Some(init.timeout_secs.unwrap_or(DEFAULT_TIMEOUT_SECS).clamp(1, MAX_TIMEOUT_SECS)) };
    state.audit.log(
        "exec.start",
        &hostname,
        Some(serde_json::json!({
            "program": init.program, "args": init.args,
            "timeout_secs": timeout_secs, "tty": init.tty,
            "ip": addr.ip().to_string(),
            "transport": "ws",
        })),
    );

    let tty_size = match (init.cols, init.rows) {
        (Some(c), Some(r)) if c > 0 && r > 0 && c <= 1024 && r <= 1024 => Some((c, r)),
        _ => None,
    };
    let (mut rx, stdin_tx, kill_tx) = runner::spawn_exec_stdin(
        &ExecRequest {
            program: init.program.clone(),
            args: init.args.clone(),
            timeout_secs,
            tty: init.tty,
        },
        tty_size,
    );

    if socket.send(Message::Text(serde_json::json!({ "type": "started" }).to_string().into())).await.is_err() {
        return; // rx 随函数返回被 drop → 子进程被 kill
    }

    // 下行泵：exec 事件 → WS 帧。Exit 终态先写审计/历史再尝试发送——
    // 客户端断连时发送必然失败，但审计闭环不能因此丢失（原先 abort 下行任务导致断连会话缺 exec.finish）。
    let client_id = client.id.clone();
    let program = init.program.clone();
    let program_dl = program.clone();
    let args = init.args.clone();
    let audit = Arc::clone(&state.audit);
    let (mut sink, mut stream) = socket.split();
    let downlink = tokio::spawn(async move {
        let program = program_dl;
        while let Some(ev) = rx.recv().await {
            match ev {
                ExecEvent::Exit { exit_code, timeout, duration_ms, truncated } => {
                    write_history(&audit, &client_id, &hostname, &program, &args, exit_code, timeout, duration_ms, truncated);
                    audit.log("exec.finish", &hostname, Some(serde_json::json!({ "program": program })));
                    let msg = event_message(&ExecEvent::Exit { exit_code, timeout, duration_ms, truncated });
                    let _ = sink.send(msg).await;
                    let _ = sink.close().await;
                    break;
                }
                other => {
                    // 客户端断连后发送失败：继续排空直至 Exit 终态，保证审计闭环
                    if sink.send(event_message(&other)).await.is_err() {
                        continue;
                    }
                }
            }
        }
    });

    // 上行：stdin 数据（本协程）
    let mut stdin_tx = Some(stdin_tx);
    let mut total_stdin: u64 = 0;
    while let Some(msg) = stream.next().await {
        match msg {
            Ok(Message::Binary(data)) => {
                let Some(tx) = stdin_tx.as_ref() else { continue }; // 子进程 stdin 已关闭：忽略后续数据
                if data.len() > MAX_STDIN_FRAME {
                    // 超限帧丢弃而非断开会话（正常客户端按 4KB 分块，异常客户端不应殃及会话）
                    macro_log::w!("WS 会话 stdin 帧超限已丢弃: {} ({} bytes)", program, data.len());
                    continue;
                }
                total_stdin += data.len() as u64;
                if total_stdin > MAX_STDIN_TOTAL {
                    macro_log::w!("WS 会话 stdin 超限，断开: {}", program);
                    break;
                }
                if tx.send(StdinMsg::Data(data.to_vec())).await.is_err() {
                    stdin_tx = None; // 子进程 stdin 已关闭，忽略后续数据
                }
            }
            Ok(Message::Text(t)) => {
                let v = serde_json::from_str::<serde_json::Value>(&t).ok();
                match v.as_ref().and_then(|v| v["type"].as_str()) {
                    Some("stdin_close") => {
                        stdin_tx = None; // drop = EOF
                    }
                    Some("resize") => {
                        let cols = v.as_ref().and_then(|v| v["cols"].as_u64()).unwrap_or(0);
                        let rows = v.as_ref().and_then(|v| v["rows"].as_u64()).unwrap_or(0);
                        // 与 init 帧一致的范围校验（防 u16 截断成异常小值）
                        if cols > 0 && rows > 0 && cols <= 1024 && rows <= 1024 {
                            let _ = stdin_tx.as_ref().map(|tx| {
                                tx.try_send(StdinMsg::Resize {
                                    cols: cols as u16,
                                    rows: rows as u16,
                                })
                            });
                        }
                    }
                    _ => {}
                }
            }
            Ok(Message::Close(_)) | Err(_) => break,
            _ => {}
        }
    }

    // 上行结束（客户端断连）：关闭 stdin 通道并触发 kill → 执行器终止子进程后发送 Exit 终态，
    // downlink 完成审计/历史写入后自然退出（原先直接 abort 下行会丢断连会话的 exec.finish）。
    drop(stdin_tx);
    let _ = kill_tx.send(true);
    // 兜底：执行器各退出路径均会发送 Exit；限时等待防止许可被意外长期占用
    let _ = tokio::time::timeout(Duration::from_secs(15), downlink).await;
}

async fn read_init(socket: &mut WebSocket) -> Result<WsInit, String> {
    loop {
        match socket.recv().await {
            Some(Ok(Message::Text(t))) => {
                return serde_json::from_str::<WsInit>(&t).map_err(|e| format!("init 帧无效: {e}"));
            }
            Some(Ok(_)) => return Err("首帧必须为 init".into()),
            Some(Err(e)) => return Err(format!("连接错误: {e}")),
            None => return Err("连接已关闭".into()),
        }
    }
}

/// exec 事件 → WS 帧：stdout=0x01 / stderr=0x02 / 截断标记=0x03；Exit 为 Text JSON
fn event_message(ev: &ExecEvent) -> Message {
    match ev {
        ExecEvent::Output { stream, data } => {
            if *stream == "__truncated__" {
                return Message::Binary(vec![3].into());
            }
            let tag: u8 = if *stream == "stderr" { 2 } else { 1 };
            let mut frame = Vec::with_capacity(1 + data.len());
            frame.push(tag);
            frame.extend_from_slice(data);
            Message::Binary(frame.into())
        }
        ExecEvent::Exit { .. } => Message::Text(serde_json::to_string(ev).unwrap_or_default().into()),
    }
}

#[allow(clippy::too_many_arguments)]
fn write_history(
    audit: &crate::server::store::Audit,
    client_id: &str,
    hostname: &str,
    program: &str,
    args: &[String],
    exit_code: Option<i32>,
    timeout: bool,
    duration_ms: u64,
    truncated: bool,
) {
    audit.log_exec(&crate::server::store::ExecHistory {
        ts: chrono::Utc::now().to_rfc3339(),
        client_id: client_id.to_string(),
        hostname: hostname.to_string(),
        program: program.to_string(),
        args: args.to_vec(),
        exit_code,
        duration_ms,
        timeout,
        truncated,
    });
}
