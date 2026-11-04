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
    /// 服务端会话 id（随 started 帧下发）：上传 init 帧回传，服务端据此定位
    /// 本会话 shell 进程 cwd，使拖放上传落地 shell 工作目录
    pub session_id: String,
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

    let (ws, _) = match connect_async_tls_with_config(
        request,
        None,
        false,
        Some(tls_connector(cert_sha256.and_then(parse_pin), None)),
    )
    .await
    {
        Ok(v) => v,
        Err(e) => {
            crate::dlog!("[ws] TLS/WS 连接失败: {e}");
            anyhow::bail!("{e}");
        }
    };
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
                        let session_id = v["session_id"].as_str().unwrap_or_default().to_string();
                        crate::dlog!("[ws] 服务端已确认会话启动（started）session_id={session_id}");
                        return Ok(ExecWs { sink, stream, session_id });
                    }
                    Some("rejected") => {
                        crate::dlog!("[ws] 服务端拒绝会话: {}", v["reason"].as_str().unwrap_or(""));
                        anyhow::bail!("{}", v["reason"].as_str().unwrap_or("执行被拒绝"));
                    }
                    other => {
                        crate::dlog!("[ws] 握手期间收到未预期的文本帧: {other:?}");
                    }
                }
            }
            Some(Ok(frame)) => {
                crate::dlog!("[ws] 握手期间收到非文本帧（已忽略）");
                let _ = frame;
            }
            Some(Err(e)) => {
                crate::dlog!("[ws] 握手期间 WebSocket 错误: {e}");
                anyhow::bail!("WebSocket 错误: {e}");
            }
            None => {
                crate::dlog!("[ws] 握手期间连接被关闭");
                anyhow::bail!("连接已关闭");
            }
        }
    }
}

/// 建立裸上传 WS 通道（目录上传浏览器桥用）：连接 /api/upload/ws 并 split。
/// 业务帧由浏览器侧引擎直接透传（init{name,size,session,rel_path,resume,mkdir}
/// → ready/exists → Binary×N → upload_end → uploaded/rejected / mkdir_ok），
/// 本地桥只做通道转发，不解析业务帧。
pub async fn ws_upload_socket(
    server_url: &str,
    token: &str,
    cert_sha256: Option<&str>,
) -> Result<(
    futures::stream::SplitSink<WsStream, WsMessage>,
    futures::stream::SplitStream<WsStream>,
)> {
    let url = server_url
        .replacen("https://", "wss://", 1)
        .replacen("http://", "ws://", 1);
    let mut request = format!("{url}/api/upload/ws").into_client_request()?;
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
    Ok(ws.split())
}

/// 目录探测/创建结果
pub struct DirProbe {
    /// 目录在操作前是否已存在
    pub existed: bool,
    /// 服务端确认的目录绝对路径
    pub path: String,
    /// 实际相对路径（auto_rename 命中时为 root_N 形态）
    pub actual_rel: String,
    /// 服务端解析的基准目录（cwd / uploads）：目录批次钉定用，后续请求回传 base_dir
    pub base: String,
}

/// 目录探测/创建（协议见服务端 upload.rs mkdir 分支）：
/// - probe=true 且目录已存在 → 不创建，应答 dir_exists
/// - auto_rename=true 且目录已存在 → 自动改名 root_N 创建
/// 目录路径为 POSIX 分隔的相对路径（服务端逐段净化防穿越；含拖入目录的根名）
pub async fn ws_dir_setup(
    server_url: &str,
    token: &str,
    session: &str,
    rel_path: &str,
    probe: bool,
    auto_rename: bool,
    base_dir: &str,
    cert_sha256: Option<&str>,
) -> Result<DirProbe> {
    if rel_path.is_empty() {
        anyhow::bail!("目录路径为空");
    }
    let (mut sink, mut stream) = ws_upload_socket(server_url, token, cert_sha256).await?;
    let init = serde_json::json!({
        "type": "upload", "name": "", "session": session,
        "rel_path": rel_path, "mkdir": true, "probe": probe, "auto_rename": auto_rename,
        "base_dir": base_dir,
    });
    sink.send(WsMessage::Text(init.to_string().into())).await?;
    loop {
        match stream.next().await {
            Some(Ok(WsMessage::Text(t))) => {
                let v: Value = serde_json::from_str(&t)?;
                match v["type"].as_str() {
                    Some("mkdir_ok") => {
                        let path = v["path"].as_str().unwrap_or_default().to_string();
                        let actual_rel = v["name"].as_str().unwrap_or(rel_path).to_string();
                        let existed = v["existed"].as_bool().unwrap_or(false);
                        let base = v["base"].as_str().unwrap_or_default().to_string();
                        let _ = sink.close().await;
                        if path.is_empty() {
                            anyhow::bail!("服务端未返回目录路径");
                        }
                        return Ok(DirProbe { existed, path, actual_rel, base });
                    }
                    Some("dir_exists") => {
                        let path = v["path"].as_str().unwrap_or_default().to_string();
                        let base = v["base"].as_str().unwrap_or_default().to_string();
                        let _ = sink.close().await;
                        if path.is_empty() {
                            anyhow::bail!("服务端未返回目录路径");
                        }
                        return Ok(DirProbe {
                            existed: true,
                            path,
                            actual_rel: rel_path.to_string(),
                            base,
                        });
                    }
                    Some("ready") => {}
                    Some("rejected") => {
                        anyhow::bail!("{}", v["reason"].as_str().unwrap_or("目录创建被拒绝"));
                    }
                    _ => {
                        crate::dlog!("[upload] 目录创建收到未预期的文本帧: {t}");
                    }
                }
            }
            Some(Ok(_)) => {}
            Some(Err(e)) => anyhow::bail!("目录创建连接错误: {e}"),
            None => anyhow::bail!("目录创建连接被服务端关闭"),
        }
    }
}

/// 上传单个文件到服务端：独立 WS 连接（复用 exec 的 TLS pin 校验与 X-Client-Token 认证）。
/// 协议：init 文本帧（name/size/session）→ Binary 数据帧×N（64KB 分块）→ upload_end 文本帧
/// → 服务端应答 uploaded（含远端绝对路径）/ rejected 文本帧。
/// 落地目录：Unix 优先所传 exec 会话 shell 的 cwd，否则数据目录 uploads/ 下；
/// 同名自动加 _N 后缀，返回落地路径。
pub async fn ws_upload(
    server_url: &str,
    token: &str,
    name: &str,
    size: u64,
    file: &mut tokio::fs::File,
    session_id: &str,
    rel_path: &str,
    overwrite: bool,
    base_dir: &str,
    cert_sha256: Option<&str>,
) -> Result<String> {
    use tokio::io::AsyncReadExt as _;

    if name.is_empty() {
        anyhow::bail!("文件名为空");
    }
    let url = server_url
        .replacen("https://", "wss://", 1)
        .replacen("http://", "ws://", 1);
    let mut request = format!("{url}/api/upload/ws").into_client_request()?;
    request
        .headers_mut()
        .insert("X-Client-Token", token.parse().map_err(|_| anyhow::anyhow!("token 含非法字符"))?);

    let (ws, _) = match connect_async_tls_with_config(
        request,
        None,
        false,
        Some(tls_connector(cert_sha256.and_then(parse_pin), None)),
    )
    .await
    {
        Ok(v) => v,
        Err(e) => {
            crate::dlog!("[upload] TLS/WS 连接失败: {e}");
            anyhow::bail!("{e}");
        }
    };
    let (mut sink, mut stream) = ws.split();

    // session_id：服务端据此定位发起上传的 exec 会话 shell 进程 cwd（落地工作目录）；
    // rel_path：目录上传的相对路径（POSIX 分隔，服务端逐段净化），单文件上传为空；
    // overwrite：同名覆盖写（目录合并语义）/ false 时 _N 重命名（单文件默认）；
    // base_dir：目录批次钉定的基准目录（防 shell cd 导致文件漂移），空 = 逐请求解析
    let init = serde_json::json!({ "type": "upload", "name": name, "size": size, "session": session_id, "rel_path": rel_path, "overwrite": overwrite, "base_dir": base_dir });
    sink.send(WsMessage::Text(init.to_string().into())).await?;

    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        sink.send(WsMessage::Binary(buf[..n].to_vec().into())).await?;
    }
    sink.send(WsMessage::Text(serde_json::json!({ "type": "upload_end" }).to_string().into()))
        .await?;

    loop {
        match stream.next().await {
            Some(Ok(WsMessage::Text(t))) => {
                let v: Value = serde_json::from_str(&t)?;
                match v["type"].as_str() {
                    Some("uploaded") => {
                        let path = v["path"].as_str().unwrap_or_default().to_string();
                        if path.is_empty() {
                            anyhow::bail!("服务端未返回落地路径");
                        }
                        let _ = sink.close().await;
                        return Ok(path);
                    }
                    Some("ready") => {} // 目录上传协议的就绪应答，CLI 单文件流程忽略
                    Some("rejected") => {
                        anyhow::bail!("{}", v["reason"].as_str().unwrap_or("上传被拒绝"));
                    }
                    _ => {
                        crate::dlog!("[upload] 收到未预期的文本帧: {t}");
                    }
                }
            }
            Some(Ok(_)) => {} // 其它帧（Ping/Pong 等）忽略
            Some(Err(e)) => anyhow::bail!("上传连接错误: {e}"),
            None => anyhow::bail!("上传连接被服务端关闭"),
        }
    }
}

/// WS 帧类型 trace 日志（联调用）：控制帧（Ping/Pong/Close）逐帧记录，用于定位心跳与断连问题。
/// 返回是否为控制帧（控制帧不进入 parse_frame 的应用层解析，调用方据此区分"无法解析"）。
pub fn trace_frame(frame: &WsMessage) -> bool {
    match frame {
        WsMessage::Ping(_) => {
            crate::dlog!("[ws] 收到服务端 Ping 帧");
            true
        }
        WsMessage::Pong(_) => {
            crate::dlog!("[ws] 收到服务端 Pong 帧");
            true
        }
        WsMessage::Close(_) => {
            crate::dlog!("[ws] 收到服务端 Close 帧");
            true
        }
        _ => false,
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
