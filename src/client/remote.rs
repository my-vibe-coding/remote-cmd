use std::sync::{Arc, OnceLock};

use anyhow::Result;
use futures::{SinkExt, StreamExt};
use serde_json::Value;
use sha2::{Digest, Sha256};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Bytes;
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tokio_tungstenite::{
    connect_async_tls_with_config, Connector, MaybeTlsStream, WebSocketStream,
};

/// 服务端专用 HTTP 客户端（接受自签证书）。
/// 仅用于注册期无指纹的首次接触（TOFU）；携带指纹的连接见 tls_connector。
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

/// resize 文本帧：请求调整远端 PTY 尺寸
pub fn resize_msg(cols: u16, rows: u16) -> WsMessage {
    WsMessage::Text(
        serde_json::json!({ "type": "resize", "cols": cols, "rows": rows })
            .to_string()
            .into(),
    )
}

/// 拆分后的执行会话：sink 用于 stdin，stream 用于读取输出
pub struct ExecWs {
    pub sink:
        futures::stream::SplitSink<WsStream, WsMessage>,
    pub stream: futures::stream::SplitStream<WsStream>,
}

/// 服务端 → 客户端的执行事件帧
#[derive(Debug)]
pub enum ExecFrame {
    Output { stream: &'static str, data: Bytes },
    Exit { exit_code: Option<i32>, timeout: bool, duration_ms: u64, truncated: bool },
}

/// 自签证书 TLS 配置：跳过证书链/主机名校验，改为**证书指纹 pin 校验**。
/// - `pin` 为 Some 时：对端证书 SHA256 必须与 pin 一致，否则握手失败（防中间人窃取凭证）
/// - `pin` 为 None 时：放行（TOFU 首次接触 / 兼容旧配置）
/// - `record` 为 Some 时：将对端证书 DER 记录到槽位（注册期采集指纹用）
fn tls_connector(pin: Option<[u8; 32]>, record: Option<Arc<OnceLock<Vec<u8>>>>) -> Connector {
    #[derive(Debug)]
    struct PinVerifier {
        pin: Option<[u8; 32]>,
        record: Option<Arc<OnceLock<Vec<u8>>>>,
        provider: Arc<rustls::crypto::CryptoProvider>,
    }
    impl PinVerifier {
        fn algorithms(&self) -> &rustls::crypto::CryptoProvider {
            &self.provider
        }
    }
    impl rustls::client::danger::ServerCertVerifier for PinVerifier {
        fn verify_server_cert(
            &self,
            end_entity: &rustls::pki_types::CertificateDer<'_>,
            _intermediates: &[rustls::pki_types::CertificateDer<'_>],
            _server_name: &rustls::pki_types::ServerName<'_>,
            _ocsp_response: &[u8],
            _now: rustls::pki_types::UnixTime,
        ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
            if let Some(slot) = &self.record {
                let _ = slot.set(end_entity.as_ref().to_vec());
            }
            if let Some(pin) = &self.pin {
                let digest: [u8; 32] = Sha256::digest(end_entity.as_ref()).into();
                if digest != *pin {
                    return Err(rustls::Error::General(
                        "服务端证书指纹不匹配，连接可能被中间人劫持".into(),
                    ));
                }
            }
            Ok(rustls::client::danger::ServerCertVerified::assertion())
        }
        fn verify_tls12_signature(
            &self,
            message: &[u8],
            cert: &rustls::pki_types::CertificateDer<'_>,
            dss: &rustls::DigitallySignedStruct,
        ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
            rustls::crypto::verify_tls12_signature(message, cert, dss, &self.algorithms().signature_verification_algorithms)
        }
        fn verify_tls13_signature(
            &self,
            message: &[u8],
            cert: &rustls::pki_types::CertificateDer<'_>,
            dss: &rustls::DigitallySignedStruct,
        ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
            rustls::crypto::verify_tls13_signature(message, cert, dss, &self.algorithms().signature_verification_algorithms)
        }
        fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
            self.algorithms().signature_verification_algorithms.supported_schemes()
        }
    }

    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut config = rustls::ClientConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()
        .expect("TLS 协议版本配置失败")
        .with_root_certificates(rustls::RootCertStore::empty())
        .with_no_client_auth();
    config.dangerous().set_certificate_verifier(Arc::new(PinVerifier { pin, record, provider }));
    Connector::Rustls(Arc::new(config))
}

/// sha256 hex 编码（64 字符小写）
fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut s = String::with_capacity(64);
    for b in digest {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// hex → 32 字节指纹；长度/字符非法返回 None
fn parse_pin(hex: &str) -> Option<[u8; 32]> {
    let hex = hex.trim();
    if hex.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, b) in out.iter_mut().enumerate() {
        *b = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some(out)
}

/// 首次接触服务器时采集其证书 SHA256 指纹（TOFU 信任锚）。
/// 仅需 TLS 握手完成即可拿到证书；之后的 HTTP 响应/WS 升级结果不影响采集。
pub async fn probe_cert_sha256(server_url: &str) -> Result<String> {
    let url = server_url
        .replacen("https://", "wss://", 1)
        .replacen("http://", "ws://", 1);
    let request = format!("{url}/").into_client_request()?;
    let slot = Arc::new(OnceLock::new());
    let connector = tls_connector(None, Some(Arc::clone(&slot)));
    let _ = connect_async_tls_with_config(request, None, false, Some(connector)).await;
    let der = slot
        .get()
        .ok_or_else(|| anyhow::anyhow!("TLS 握手未完成，无法获取服务端证书"))?;
    Ok(sha256_hex(der))
}

/// 建立执行 WS 会话：连接 → 认证 → init 握手（等待 started / rejected）
/// tty_size：PTY 会话的本地终端尺寸（cols, rows），随 init 帧上报避免服务端 80x24 初始竞态
/// cert_sha256：服务端证书指纹（hex）。Some 时强校验对端证书，防中间人窃取凭证
pub async fn ws_exec(
    server_url: &str,
    token: &str,
    program: &str,
    args: &[String],
    timeout_secs: Option<u64>,
    tty: bool,
    tty_size: Option<(u16, u16)>,
    cert_sha256: Option<&str>,
) -> Result<ExecWs> {
    let url = server_url
        .replacen("https://", "wss://", 1)
        .replacen("http://", "ws://", 1);
    let mut request = format!("{url}/api/exec/ws").into_client_request()?;
    request
        .headers_mut()
        .insert("X-Client-Token", token.parse().map_err(|_| anyhow::anyhow!("token 含非法字符"))?);

    let (ws, _) = connect_async_tls_with_config(
        request,
        None,
        false,
        Some(tls_connector(cert_sha256.and_then(parse_pin), None)),
    )
    .await?;
    let (mut sink, mut stream) = ws.split();

    let (cols, rows) = tty_size.unwrap_or((0, 0));
    crate::dlog!(
        "[ws] 客户端发起执行会话 {} program={} args={:?} tty={} size={}x{}",
        server_url,
        program,
        args,
        tty,
        cols,
        rows
    );
    let init = serde_json::json!({
        "type": "init",
        "program": program,
        "args": args,
        "timeout_secs": timeout_secs,
        "tty": tty,
        "cols": cols,
        "rows": rows,
    });
    sink.send(WsMessage::Text(init.to_string().into())).await?;

    loop {
        match stream.next().await {
            Some(Ok(WsMessage::Text(t))) => {
                let v: Value = serde_json::from_str(&t)?;
                match v["type"].as_str() {
                    Some("started") => {
                        crate::dlog!("[ws] 服务端已确认会话启动（started）");
                        return Ok(ExecWs { sink, stream });
                    }
                    Some("rejected") => {
                        crate::dlog!("[ws] 服务端拒绝会话: {}", v["reason"].as_str().unwrap_or(""));
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

/// WS 帧 → 执行事件；无法识别的帧返回 None。
/// Binary 帧用 Bytes 零拷贝切片（去除 tag 字节），避免每帧一次 to_vec 拷贝。
pub fn parse_frame(msg: WsMessage) -> Option<ExecFrame> {
    match msg {
        WsMessage::Binary(data) if !data.is_empty() => match data[0] {
            1 => Some(ExecFrame::Output { stream: "stdout", data: data.slice(1..) }),
            2 => Some(ExecFrame::Output { stream: "stderr", data: data.slice(1..) }),
            3 => Some(ExecFrame::Output { stream: "__truncated__", data: data.slice(1..) }),
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
