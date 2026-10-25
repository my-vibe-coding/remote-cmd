use std::sync::OnceLock;

use anyhow::Result;
use futures::{SinkExt, StreamExt};
use serde_json::Value;
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tokio_tungstenite::{
    connect_async_tls_with_config, Connector, MaybeTlsStream, WebSocketStream,
};

/// 服务端专用 HTTP 客户端（接受自签证书）
pub fn http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .expect("构建 HTTP 客户端失败")
}

// ---- WS 执行通道 ----

pub type WsStream = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// stdin EOF 文本帧（协议见 .trae/documents/stdin-implementation-plan.md）
pub const STDIN_CLOSE: &str = "{\"type\":\"stdin_close\"}";

/// 拆分后的执行会话：sink 用于 stdin，stream 用于读取输出
pub struct ExecWs {
    pub sink:
        futures::stream::SplitSink<WsStream, WsMessage>,
    pub stream: futures::stream::SplitStream<WsStream>,
}

/// 服务端 → 客户端的执行事件帧
#[derive(Debug)]
pub enum ExecFrame {
    Output { stream: &'static str, data: Vec<u8> },
    Exit { exit_code: Option<i32>, timeout: bool, duration_ms: u64, truncated: bool },
}

/// 自签证书放行的 TLS 配置（仅跳过证书链校验，仍校验签名），与 reqwest danger_accept_invalid_certs 策略一致
fn tls_connector() -> Connector {
    static CONNECTOR: OnceLock<Connector> = OnceLock::new();
    CONNECTOR
        .get_or_init(|| {
            #[derive(Debug)]
            struct AcceptAnyServer(Arc<rustls::crypto::CryptoProvider>);
            use std::sync::Arc;

            impl rustls::client::danger::ServerCertVerifier for AcceptAnyServer {
                fn verify_server_cert(
                    &self,
                    _end_entity: &rustls::pki_types::CertificateDer<'_>,
                    _intermediates: &[rustls::pki_types::CertificateDer<'_>],
                    _server_name: &rustls::pki_types::ServerName<'_>,
                    _ocsp_response: &[u8],
                    _now: rustls::pki_types::UnixTime,
                ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
                    Ok(rustls::client::danger::ServerCertVerified::assertion())
                }
                fn verify_tls12_signature(
                    &self,
                    message: &[u8],
                    cert: &rustls::pki_types::CertificateDer<'_>,
                    dss: &rustls::DigitallySignedStruct,
                ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
                    rustls::crypto::verify_tls12_signature(message, cert, dss, &self.0.signature_verification_algorithms)
                }
                fn verify_tls13_signature(
                    &self,
                    message: &[u8],
                    cert: &rustls::pki_types::CertificateDer<'_>,
                    dss: &rustls::DigitallySignedStruct,
                ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
                    rustls::crypto::verify_tls13_signature(message, cert, dss, &self.0.signature_verification_algorithms)
                }
                fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
                    self.0.signature_verification_algorithms.supported_schemes()
                }
            }

            let provider = Arc::new(rustls::crypto::ring::default_provider());
            let mut config = rustls::ClientConfig::builder_with_provider(provider.clone())
                .with_safe_default_protocol_versions()
                .expect("TLS 协议版本配置失败")
                .with_root_certificates(rustls::RootCertStore::empty())
                .with_no_client_auth();
            config
                .dangerous()
                .set_certificate_verifier(Arc::new(AcceptAnyServer(provider)));
            Connector::Rustls(Arc::new(config))
        })
        .clone()
}

/// 建立执行 WS 会话：连接 → 认证 → init 握手（等待 started / rejected）
pub async fn ws_exec(
    server_url: &str,
    token: &str,
    program: &str,
    args: &[String],
    timeout_secs: Option<u64>,
) -> Result<ExecWs> {
    let url = server_url
        .replacen("https://", "wss://", 1)
        .replacen("http://", "ws://", 1);
    let mut request = format!("{url}/api/exec/ws").into_client_request()?;
    request
        .headers_mut()
        .insert("X-Client-Token", token.parse().map_err(|_| anyhow::anyhow!("token 含非法字符"))?);

    let (ws, _) = connect_async_tls_with_config(request, None, false, Some(tls_connector())).await?;
    let (mut sink, mut stream) = ws.split();

    let init = serde_json::json!({
        "type": "init",
        "program": program,
        "args": args,
        "timeout_secs": timeout_secs,
    });
    sink.send(WsMessage::Text(init.to_string().into())).await?;

    loop {
        match stream.next().await {
            Some(Ok(WsMessage::Text(t))) => {
                let v: Value = serde_json::from_str(&t)?;
                match v["type"].as_str() {
                    Some("started") => {
                        return Ok(ExecWs { sink, stream });
                    }
                    Some("rejected") => {
                        anyhow::bail!("{}", v["reason"].as_str().unwrap_or("执行被拒绝"));
                    }
                    _ => {}
                }
            }
            Some(Ok(_)) => {}
            Some(Err(e)) => anyhow::bail!("WebSocket 错误: {e}"),
            None => anyhow::bail!("连接已关闭"),
        }
    }
}

/// WS 帧 → 执行事件；无法识别的帧返回 None
pub fn parse_frame(msg: WsMessage) -> Option<ExecFrame> {
    match msg {
        WsMessage::Binary(data) if !data.is_empty() => match data[0] {
            1 => Some(ExecFrame::Output { stream: "stdout", data: data[1..].to_vec() }),
            2 => Some(ExecFrame::Output { stream: "stderr", data: data[1..].to_vec() }),
            3 => Some(ExecFrame::Output { stream: "__truncated__", data: vec![] }),
            _ => None,
        },
        WsMessage::Text(t) => {
            let v: Value = serde_json::from_str(&t).ok()?;
            if v["type"].as_str()? != "exit" {
                return None;
            }
            Some(ExecFrame::Exit {
                exit_code: v["exit_code"].as_i64().map(|c| c as i32),
                timeout: v["timeout"].as_bool().unwrap_or(false),
                duration_ms: v["duration_ms"].as_u64().unwrap_or(0),
                truncated: v["truncated"].as_bool().unwrap_or(false),
            })
        }
        _ => None,
    }
}

// ---- 注册等 HTTP API（保持不变） ----

/// POST /api/register
pub async fn register(
    server_url: &str,
    body: &crate::common::fingerprint::MachineInfo,
) -> Result<Value> {
    let resp = http_client()
        .post(format!("{server_url}/api/register"))
        .json(body)
        .send()
        .await?
        .error_for_status()?;
    Ok(resp.json().await?)
}

/// GET /api/register/status/{client_id}
pub async fn register_status(server_url: &str, client_id: &str) -> Result<Value> {
    let resp = http_client()
        .get(format!("{server_url}/api/register/status/{client_id}"))
        .send()
        .await?
        .error_for_status()?;
    Ok(resp.json().await?)
}

/// DELETE /api/register：客户端凭 token 自助注销
pub async fn unregister(server_url: &str, token: &str) -> Result<()> {
    http_client()
        .delete(format!("{server_url}/api/register"))
        .header("X-Client-Token", token)
        .send()
        .await?
        .error_for_status()?;
    Ok(())
}
