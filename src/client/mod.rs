pub mod config;
pub mod remote;
pub mod routes;
pub mod run_cmd;
pub mod script;

use std::net::SocketAddr;

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

    let app = routes::router();
    macro_log::i!("remote-cmd 客户端已启动: {}", url);
    let _ = open::that(url);

    axum::serve(listener, app).await?;
    Ok(())
}
