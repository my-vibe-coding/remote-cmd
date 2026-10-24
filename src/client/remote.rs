use anyhow::Result;
use base64::Engine;
use futures::StreamExt;
use serde_json::Value;

/// 服务端专用 HTTP 客户端（接受自签证书）
pub fn http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .expect("构建 HTTP 客户端失败")
}

/// 无超时客户端（SSE 长连接用）
pub fn http_client_streaming() -> reqwest::Client {
    reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .build()
        .expect("构建 HTTP 客户端失败")
}

/// SSE 事件
#[derive(Debug, Clone)]
pub enum SseEvent {
    Output {
        stream: String,
        data_b64: String,
    },
    Exit {
        #[allow(dead_code)]
        exit_code: Option<i32>,
        timeout: bool,
        #[allow(dead_code)]
        duration_ms: u64,
        #[allow(dead_code)]
        truncated: bool,
    },
}

impl SseEvent {
    pub fn decode_output(&self) -> Vec<u8> {
        match self {
            SseEvent::Output { data_b64, .. } => {
                base64::engine::general_purpose::STANDARD.decode(data_b64).unwrap_or_default()
            }
            _ => vec![],
        }
    }
}

/// 解析服务端 SSE 流，逐事件回调；返回最终 exit 信息
pub async fn consume_sse(
    response: reqwest::Response,
    mut on_event: impl FnMut(SseEvent) -> Result<()>,
) -> Result<Option<i32>> {
    let mut stream = response.bytes_stream();
    let mut buffer: Vec<u8> = vec![];
    let mut current_event = String::new();
    let mut exit_code: Option<i32> = None;

    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        buffer.extend_from_slice(&chunk);
        // SSE 以空行分隔事件
        while let Some(pos) = find_double_newline(&buffer) {
            let block: Vec<u8> = buffer.drain(..pos).collect();
            let text = String::from_utf8_lossy(&block);
            for line in text.lines() {
                if let Some(val) = line.strip_prefix("event:") {
                    current_event = val.trim().to_string();
                } else if let Some(val) = line.strip_prefix("data:") {
                    let val = val.trim();
                    match current_event.as_str() {
                        "output" => {
                            if let Ok(v) = serde_json::from_str::<Value>(val) {
                                let stream = v["stream"].as_str().unwrap_or("stdout").to_string();
                                let data_b64 = v["data_b64"].as_str().unwrap_or("").to_string();
                                on_event(SseEvent::Output { stream, data_b64 })?;
                            }
                        }
                        "exit" => {
                            if let Ok(v) = serde_json::from_str::<Value>(val) {
                                exit_code = v["exit_code"].as_i64().map(|c| c as i32);
                                on_event(SseEvent::Exit {
                                    exit_code,
                                    timeout: v["timeout"].as_bool().unwrap_or(false),
                                    duration_ms: v["duration_ms"].as_u64().unwrap_or(0),
                                    truncated: v["truncated"].as_bool().unwrap_or(false),
                                })?;
                            }
                        }
                        _ => {}
                    }
                }
            }
            current_event.clear();
        }
    }
    Ok(exit_code)
}

fn find_double_newline(buf: &[u8]) -> Option<usize> {
    const SEP: &[u8] = b"\n\n";
    const SEP2: &[u8] = b"\r\n\r\n";
    let a = buf.windows(2).position(|w| w == SEP);
    let b = buf.windows(4).position(|w| w == SEP2);
    match (a, b) {
        (Some(x), Some(y)) => Some(x.min(y) + if x.min(y) == y { 4 } else { 2 }),
        (Some(x), None) => Some(x + 2),
        (None, Some(y)) => Some(y + 4),
        (None, None) => None,
    }
}

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

/// POST /api/exec，返回 SSE Response 供消费
pub async fn exec_sse(server_url: &str, token: &str, body: &Value) -> Result<reqwest::Response> {
    let resp = http_client_streaming()
        .post(format!("{server_url}/api/exec"))
        .header("X-Client-Token", token)
        .json(body)
        .send()
        .await?
        .error_for_status()?;
    Ok(resp)
}
