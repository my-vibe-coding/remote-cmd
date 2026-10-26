pub mod auth;
pub mod rate_limit;
pub mod routes;
pub mod runner;
pub mod state;
pub mod store;
pub mod tls;

use std::path::PathBuf;
use std::sync::Arc;

use axum_server::tls_rustls::RustlsConfig;

use crate::common::crypto;
use crate::common::json_file::{load_json, save_json_atomic};
use crate::common::paths::server_data_dir;

#[derive(serde::Serialize, serde::Deserialize)]
struct ServerConfig {
    admin_password_hash: String,
}

pub async fn serve(port: u16, password: Option<String>, data_dir: Option<String>) -> anyhow::Result<()> {
    // rustls crypto provider（reqwest 的 ring 与 axum-server 的 aws-lc-rs 冲突时手动指定）
    let _ = rustls::crypto::ring::default_provider().install_default();

    let dir: PathBuf = server_data_dir(data_dir.as_deref());
    std::fs::create_dir_all(&dir)?;

    // 管理员密码：命令行提供则重置，否则沿用已存密码，首次自动生成随机密码并打印
    let config_path = dir.join("config.json");
    let mut config: Option<ServerConfig> = load_json(&config_path)?;
    let mut printed_password: Option<String> = None;
    match (password, config.as_ref()) {
        (Some(p), _) => {
            config = Some(ServerConfig {
                admin_password_hash: crypto::hash_password(&p)?,
            });
        }
        (None, None) => {
            let pwd = crypto::random_password(16);
            macro_log::i!("========================================");
            macro_log::i!("自动生成管理员密码: {}", pwd);
            macro_log::i!("========================================");
            printed_password = Some(pwd.clone());
            config = Some(ServerConfig {
                admin_password_hash: crypto::hash_password(&pwd)?,
            });
        }
        (None, Some(_)) => {
            macro_log::i!("沿用已保存的管理员密码（忘记密码可用 --password 重置）");
        }
    }
    let config = config.unwrap();
    save_json_atomic(&config_path, &config)?;

    // TLS
    let tls_paths = tls::ensure_cert(&dir)?;
    let tls_config = RustlsConfig::from_pem_file(&tls_paths.cert, &tls_paths.key).await?;

    // 状态
    let store = store::Store::load(&dir)?;
    let audit = store::Audit::new(&dir);
    let state = Arc::new(state::AppState::new(dir.clone(), config.admin_password_hash, store, audit));

    let app = routes::router(state);

    let addr = std::net::SocketAddr::from(([0, 0, 0, 0], port));
    macro_log::i!("remote-cmd 服务端监听: {addr}");
    macro_log::i!("管理页面: https://127.0.0.1:{port}/admin");
    if let Some(pwd) = printed_password {
        macro_log::i!("（首次启动，请记录上方自动生成的管理员密码）");
        let _ = pwd;
    }

    // 公网地址后台获取（最多 5s 外部请求），不阻塞监听启动
    tokio::spawn(async move {
        if let Some(ip) = crate::common::net::fetch_public_ipv4().await {
            macro_log::i!("管理页面（公网）: https://{ip}:{port}/admin");
            macro_log::i!("注册地址（公网）: https://{ip}:{port}");
        }
    });

    axum_server::bind_rustls(addr, tls_config)
        .serve(app.into_make_service_with_connect_info::<std::net::SocketAddr>())
        .await?;
    Ok(())
}
