pub mod config;
pub mod dlog;
pub mod remote;
pub mod routes;
pub mod run_cmd;
pub mod script;

use std::net::SocketAddr;

use axum::extract::Request;
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

/// 客户端模式：启动本地 web 服务并自动打开浏览器
pub async fn start() -> anyhow::Result<()> {
    // 优先 3234，被占用则随机端口
    let addr = SocketAddr::from(([127, 0, 0, 1], 3234));
    let listener = match tokio::net::TcpListener::bind(addr).await {
        Ok(l) => l,
        Err(_) => {
            let fallback = SocketAddr::from(([127, 0, 0, 1], 0));
            tokio::net::TcpListener::bind(fallback).await?
        }
    };
    let local_addr = listener.local_addr()?;
    let url = format!("http://{local_addr}/");

    let port = local_addr.port();
    let app = routes::router().layer(axum::middleware::from_fn(
        move |req: Request, next: Next| host_allowlist(port, req, next),
    ));
    macro_log::i!("remote-cmd 客户端已启动: {}", url);
    let _ = open::that(url);

    axum::serve(listener, app).await?;
    Ok(())
}

/// Host 白名单：仅接受 127.0.0.1/localhost/[::1] 的本机端口请求。
/// 防 DNS rebinding——恶意域名解析到 127.0.0.1 时其 Host 头为攻击域名，直接拒绝。
async fn host_allowlist(port: u16, req: Request, next: Next) -> Response {
    let host = req
        .headers()
        .get(axum::http::header::HOST)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_lowercase();
    let allowed = [
        format!("127.0.0.1:{port}"),
        format!("localhost:{port}"),
        format!("[::1]:{port}"),
    ];
    if allowed.contains(&host) {
        next.run(req).await
    } else {
        (StatusCode::FORBIDDEN, "拒绝跨源请求").into_response()
    }
}
