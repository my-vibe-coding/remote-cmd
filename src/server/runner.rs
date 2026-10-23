use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use base64::Engine;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::mpsc;

const MAX_OUTPUT_BYTES: u64 = 1024 * 1024;
pub const DEFAULT_TIMEOUT_SECS: u64 = 60;
pub const MAX_TIMEOUT_SECS: u64 = 600;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecRequest {
    pub program: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub timeout_secs: Option<u64>,
}

/// SSE 事件负载
#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ExecEvent {
    Output {
        stream: &'static str,
        data_b64: String,
    },
    Exit {
        exit_code: Option<i32>,
        timeout: bool,
        duration_ms: u64,
        truncated: bool,
    },
}

/// 启动子进程并流式推送输出事件；返回的 Receiver 断开（客户端断连）时子进程被 kill。
pub fn spawn_exec(req: &ExecRequest) -> mpsc::Receiver<ExecEvent> {
    let (tx, rx) = mpsc::channel::<ExecEvent>(256);
    let req = ExecRequest {
        program: req.program.clone(),
        args: req.args.clone(),
        timeout_secs: req.timeout_secs,
    };
    tokio::spawn(async move {
        run(req, tx).await;
    });
    rx
}

async fn run(req: ExecRequest, tx: mpsc::Sender<ExecEvent>) {
    let started = Instant::now();
    let truncated = Arc::new(AtomicBool::new(false));

    let mut child = match Command::new(&req.program)
        .args(&req.args)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
    {
        Ok(c) => c,
        Err(err) => {
            // 找不到程序等启动失败：以 exit 事件返回
            let _ = tx
                .send(ExecEvent::Exit {
                    exit_code: None,
                    timeout: false,
                    duration_ms: started.elapsed().as_millis() as u64,
                    truncated: false,
                })
                .await;
            macro_log::w!("启动进程失败 {}: {err}", req.program);
            return;
        }
    };

    let mut stdout = child.stdout.take().map(BufReader::new);
    let mut stderr = child.stderr.take().map(BufReader::new);
    let total = Arc::new(AtomicU64::new(0));

    let mut readers = Vec::new();
    if let Some(reader) = stdout.take() {
        readers.push(tokio::spawn(pump(
            reader,
            "stdout",
            tx.clone(),
            total.clone(),
            truncated.clone(),
        )));
    }
    if let Some(reader) = stderr.take() {
        readers.push(tokio::spawn(pump(
            reader,
            "stderr",
            tx.clone(),
            total.clone(),
            truncated.clone(),
        )));
    }

    let timeout_secs = req
        .timeout_secs
        .unwrap_or(DEFAULT_TIMEOUT_SECS)
        .clamp(1, MAX_TIMEOUT_SECS);
    let wait = tokio::time::timeout(Duration::from_secs(timeout_secs), child.wait()).await;

    let (exit_code, timed_out) = match wait {
        Ok(Ok(status)) => (status.code(), false),
        Ok(Err(_)) => (None, false),
        Err(_) => {
            // 超时：kill 后等待读取任务收尾
            let _ = child.kill().await;
            (None, true)
        }
    };

    for task in readers {
        let _ = task.await;
    }

    // 等待 stdout/stderr 句柄完全关闭（wait 后句柄仍可能残留缓冲）
    let _ = child;

    let _ = tx
        .send(ExecEvent::Exit {
            exit_code,
            timeout: timed_out,
            duration_ms: started.elapsed().as_millis() as u64,
            truncated: truncated.load(Ordering::Relaxed),
        })
        .await;
}

async fn pump<R: tokio::io::AsyncRead + Unpin>(
    mut reader: BufReader<R>,
    stream: &'static str,
    tx: mpsc::Sender<ExecEvent>,
    total: Arc<AtomicU64>,
    truncated: Arc<AtomicBool>,
) {
    let mut buf = [0u8; 4096];
    loop {
        match reader.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                let prev = total.fetch_add(n as u64, Ordering::Relaxed);
                if prev >= MAX_OUTPUT_BYTES {
                    // 超限：丢弃后续输出（继续读取以让进程正常结束）
                    if !truncated.swap(true, Ordering::Relaxed) {
                        let _ = tx
                            .send(ExecEvent::Output {
                                stream: "__truncated__",
                                data_b64: String::new(),
                            })
                            .await;
                    }
                    continue;
                }
                let b64 = base64::engine::general_purpose::STANDARD.encode(&buf[..n]);
                if tx
                    .send(ExecEvent::Output {
                        stream,
                        data_b64: b64,
                    })
                    .await
                    .is_err()
                {
                    // 接收端关闭（客户端断连）
                    break;
                }
            }
        }
    }
}

/// 白名单匹配：程序名（取文件名部分）与白名单条目不区分大小写比较
pub fn whitelist_matches(whitelist: &[String], program: &str) -> bool {
    let name = std::path::Path::new(program)
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| program.to_string());
    let name = name.to_lowercase();
    whitelist.iter().any(|entry| entry.trim().to_lowercase() == name)
}
