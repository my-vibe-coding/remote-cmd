use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;

use axum::extract::{ConnectInfo, State};
use axum::http::StatusCode;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::Json;
use futures::stream::Stream;
use tokio_stream::wrappers::ReceiverStream;

use crate::server::auth::ClientAuth;
use crate::server::runner::{self, ExecRequest, DEFAULT_TIMEOUT_SECS, MAX_TIMEOUT_SECS};
use crate::server::state::AppState;

/// POST /api/exec：凭证认证 → 并发限制 → 白名单校验 → SSE 流式执行
pub async fn exec(
    State(state): State<Arc<AppState>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    ClientAuth(client): ClientAuth,
    Json(req): Json<ExecRequest>,
) -> Result<Sse<impl Stream<Item = Result<Event, Infallible>>>, (StatusCode, String)> {
    if req.program.trim().is_empty() {
        return Err((StatusCode::BAD_REQUEST, "program 不能为空".into()));
    }
    let timeout_secs = req
        .timeout_secs
        .unwrap_or(DEFAULT_TIMEOUT_SECS)
        .clamp(1, MAX_TIMEOUT_SECS);

    // 白名单校验（程序名不区分大小写）
    if !runner::whitelist_matches(&client.whitelist, &req.program) {
        state.audit.log(
            "exec.denied",
            &client.hostname,
            Some(serde_json::json!({ "program": req.program, "args": req.args })),
        );
        return Err((StatusCode::FORBIDDEN, format!("命令未在白名单中: {}", req.program)));
    }

    // 并发限制
    let global = state.global_sem.clone();
    let per_client = state.per_client(&client.id);
    let (global_permit, client_permit) = match tokio::join!(
        async { global.acquire().await },
        async { per_client.acquire().await }
    ) {
        (Ok(g), Ok(c)) => (g, c),
        _ => return Err((StatusCode::SERVICE_UNAVAILABLE, "并发已达上限".into())),
    };
    // 持有信号量直到本次执行结束（permits 随 guard drop 释放）
    let _ = (global_permit, client_permit);

    state.audit.log(
        "exec.start",
        &client.hostname,
        Some(serde_json::json!({
            "program": req.program, "args": req.args,
            "timeout_secs": timeout_secs, "ip": addr.ip().to_string(),
        })),
    );

    let rx = runner::spawn_exec(&ExecRequest {
        program: req.program.clone(),
        args: req.args.clone(),
        timeout_secs: Some(timeout_secs),
    });

    let client_id = client.id.clone();
    let hostname = client.hostname.clone();
    let program = req.program.clone();
    let args = req.args.clone();
    let audit_ref = Arc::clone(&state.audit);

    let stream = ReceiverStream::new(rx);
    let mapped = futures::stream::unfold(
        (stream, client_id, hostname, program, args, audit_ref),
        |(mut stream, client_id, hostname, program, args, audit_ref)| async move {
            use tokio_stream::StreamExt;
            match stream.next().await {
                Some(event) => {
                    let is_exit = matches!(event, runner::ExecEvent::Exit { .. });
                    let (name, data) = sse_event(&event);
                    if is_exit {
                        write_history(&audit_ref, &client_id, &hostname, &program, &args, &event);
                        audit_ref.log(
                            "exec.finish",
                            &hostname,
                            Some(serde_json::json!({ "program": program })),
                        );
                    }
                    Some((
                        Ok(Event::default().event(name).data(data)),
                        (stream, client_id, hostname, program, args, audit_ref),
                    ))
                }
                None => None,
            }
        },
    );

    Ok(Sse::new(mapped).keep_alive(KeepAlive::default()))
}

fn sse_event(event: &runner::ExecEvent) -> (&'static str, String) {
    (
        match event {
            runner::ExecEvent::Output { .. } => "output",
            runner::ExecEvent::Exit { .. } => "exit",
        },
        serde_json::to_string(event).unwrap_or_default(),
    )
}

fn write_history(
    audit: &crate::server::store::Audit,
    client_id: &str,
    hostname: &str,
    program: &str,
    args: &[String],
    event: &runner::ExecEvent,
) {
    if let runner::ExecEvent::Exit {
        exit_code,
        timeout,
        duration_ms,
        truncated,
    } = event
    {
        audit.log_exec(&crate::server::store::ExecHistory {
            ts: chrono::Utc::now().to_rfc3339(),
            client_id: client_id.to_string(),
            hostname: hostname.to_string(),
            program: program.to_string(),
            args: args.to_vec(),
            exit_code: *exit_code,
            duration_ms: *duration_ms,
            timeout: *timeout,
            truncated: *truncated,
        });
    }
}
