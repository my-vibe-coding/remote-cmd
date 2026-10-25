use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use base64::Engine;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};
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

/// stdin 转发消息：Data 为数据块，Eof 表示关闭子进程 stdin（drop Sender 等价于 Eof）
#[derive(Debug)]
pub enum StdinMsg {
    Data(Vec<u8>),
    #[allow(dead_code)] // 协议层保留：测试与未来调用方使用
    Eof,
}

/// 启动子进程（stdin 已接管）：返回 (输出事件流, stdin 发送端)。
/// Sender drop 与发送 Eof 等价（关闭子进程 stdin）；输出 Receiver 被 drop（客户端断连）时子进程被 kill。
pub fn spawn_exec_stdin(req: &ExecRequest) -> (mpsc::Receiver<ExecEvent>, mpsc::Sender<StdinMsg>) {
    let (tx, rx) = mpsc::channel::<ExecEvent>(256);
    let (stx, srx) = mpsc::channel::<StdinMsg>(32);
    let req = ExecRequest {
        program: req.program.clone(),
        args: req.args.clone(),
        timeout_secs: req.timeout_secs,
    };
    tokio::spawn(async move {
        run(req, tx, srx).await;
    });
    (rx, stx)
}

async fn run(req: ExecRequest, tx: mpsc::Sender<ExecEvent>, mut stdin_rx: mpsc::Receiver<StdinMsg>) {
    let started = Instant::now();
    let truncated = Arc::new(AtomicBool::new(false));

    let mut child = match Command::new(&req.program)
        .args(&req.args)
        .stdin(std::process::Stdio::piped())
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

    // stdin writer 任务：消费 StdinMsg → 写入子进程；EOF 后 shutdown 产生 EOF 并排空后续消息
    let mut child_stdin = child.stdin.take();
    tokio::spawn(async move {
        if let Some(mut sin) = child_stdin.take() {
            while let Some(msg) = stdin_rx.recv().await {
                match msg {
                    StdinMsg::Data(data) => {
                        if sin.write_all(&data).await.is_err() {
                            break;
                        }
                    }
                    StdinMsg::Eof => break,
                }
            }
            let _ = sin.shutdown().await;
        }
        // 排空残余消息，避免发送端阻塞
        while stdin_rx.recv().await.is_some() {}
    });

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
    enum WaitOut {
        Code(Option<i32>),
        Disconnected,
    }
    let outcome = tokio::time::timeout(Duration::from_secs(timeout_secs), async {
        loop {
            tokio::select! {
                r = child.wait() => break WaitOut::Code(r.ok().and_then(|s| s.code())),
                // 所有事件接收端已断开（客户端断连）→ 终止子进程
                _ = tx.closed() => break WaitOut::Disconnected,
            }
        }
    })
    .await;

    let (exit_code, timed_out) = match outcome {
        Ok(WaitOut::Code(code)) => (code, false),
        Ok(WaitOut::Disconnected) => {
            let _ = child.kill().await;
            let _ = child.wait().await;
            (None, false)
        }
        Err(_) => {
            // 超时：kill 后等待读取任务收尾
            let _ = child.kill().await;
            let _ = child.wait().await;
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
    let mut transcoder = OutputTranscoder::new();
    loop {
        match reader.read(&mut buf).await {
            Ok(0) | Err(_) => {
                // 冲刷转码器尾部（未完成的多字节序列）
                let tail = transcoder.feed(&[], true);
                if !tail.is_empty() {
                    let _ = send_output(&tx, stream, tail.as_bytes()).await;
                }
                break;
            }
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
                let text = transcoder.feed(&buf[..n], false);
                if !send_output(&tx, stream, text.as_bytes()).await {
                    // 接收端关闭（客户端断连）
                    break;
                }
            }
        }
    }
}

async fn send_output(
    tx: &mpsc::Sender<ExecEvent>,
    stream: &'static str,
    bytes: &[u8],
) -> bool {
    let b64 = base64::engine::general_purpose::STANDARD.encode(bytes);
    tx.send(ExecEvent::Output {
        stream,
        data_b64: b64,
    })
    .await
    .is_ok()
}

/// 子进程输出转码器：Windows 控制台程序多以 OEM 代码页（中文系统为 GBK/cp936）输出，
/// 而 UTF-8 程序（node/git/python 等）输出 UTF-8。首块按严格 UTF-8 校验判定编码，
/// 之后用有状态解码器处理跨块的多字节序列，统一转为 UTF-8。
struct OutputTranscoder {
    dec: Option<encoding_rs::Decoder>,
    legacy: Option<&'static encoding_rs::Encoding>,
}

impl OutputTranscoder {
    fn new() -> Self {
        OutputTranscoder { dec: None, legacy: oem_encoding() }
    }

    fn feed(&mut self, bytes: &[u8], last: bool) -> String {
        if self.dec.is_none() {
            if bytes.is_empty() {
                return String::new();
            }
            // error_len() == None 表示仅尾部多字节序列被截断，仍视为 UTF-8
            let is_utf8 = match std::str::from_utf8(bytes) {
                Ok(_) => true,
                Err(e) => e.error_len().is_none(),
            };
            let enc = if is_utf8 {
                encoding_rs::UTF_8
            } else {
                self.legacy.unwrap_or(encoding_rs::UTF_8)
            };
            self.dec = Some(enc.new_decoder());
        }
        let mut dec = self.dec.take().unwrap();
        let mut out = String::new();
        let src = bytes;
        loop {
            out.reserve(src.len() * 3 + 16);
            match dec.decode_to_string(src, &mut out, last).0 {
                encoding_rs::CoderResult::InputEmpty => break,
                encoding_rs::CoderResult::OutputFull => continue,
            }
        }
        self.dec = Some(dec);
        out
    }
}

/// Windows OEM 代码页 → encoding_rs；非 Windows 或未知代码页返回 None（按 UTF-8）
#[cfg(windows)]
fn oem_encoding() -> Option<&'static encoding_rs::Encoding> {
    let cp = unsafe { windows_sys::Win32::Globalization::GetOEMCP() };
    Some(match cp {
        936 => encoding_rs::GBK,
        950 => encoding_rs::BIG5,
        932 => encoding_rs::SHIFT_JIS,
        949 => encoding_rs::EUC_KR,
        1252 => encoding_rs::WINDOWS_1252,
        _ => return None,
    })
}

#[cfg(not(windows))]
fn oem_encoding() -> Option<&'static encoding_rs::Encoding> {
    None
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

#[cfg(test)]
mod tests {
    use super::*;

    /// stdin 数据经 WS 会话转发后应到达子进程（Windows: more 回显；Unix: cat 回显）
    #[tokio::test]
    async fn stdin_pipe_forwarded_to_child() {
        #[cfg(windows)]
        let req = ExecRequest {
            program: "cmd".into(),
            args: ["/C".to_string(), "more".to_string()].to_vec(),
            timeout_secs: Some(15),
        };
        #[cfg(not(windows))]
        let req = ExecRequest {
            program: "cat".into(),
            args: vec![],
            timeout_secs: Some(15),
        };

        let (mut rx, tx) = spawn_exec_stdin(&req);
        tx.send(StdinMsg::Data(b"hello stdin\n".to_vec())).await.unwrap();
        tx.send(StdinMsg::Eof).await.unwrap();
        drop(tx);

        let mut stdout = String::new();
        let mut exit_code = None;
        while let Some(ev) = rx.recv().await {
            match ev {
                ExecEvent::Output { stream, data_b64 } if stream == "stdout" => {
                    stdout.push_str(&String::from_utf8_lossy(
                        &base64::engine::general_purpose::STANDARD.decode(&data_b64).unwrap_or_default(),
                    ));
                }
                ExecEvent::Exit { exit_code: c, .. } => {
                    exit_code = c;
                    break;
                }
                _ => {}
            }
        }
        assert!(stdout.contains("hello stdin"), "stdout = {stdout:?}");
        assert_eq!(exit_code, Some(0));
    }

    #[test]
    fn transcodes_gbk_split_across_chunks() {
        // "测试中文GBK输出" 的 GBK 编码（cp936）
        let gbk: &[u8] = &[
            0xB2, 0xE2, 0xCA, 0xD4, 0xD6, 0xD0, 0xCE, 0xC4, // 测试中文
            b'G', b'B', b'K',
            0xCA, 0xE4, 0xB3, 0xF6, // 输出
        ];
        let mut t = OutputTranscoder { dec: None, legacy: Some(encoding_rs::GBK) };
        // 在多字节序列中间切开，模拟管道读取的任意分块
        let mid = 5;
        let mut out = t.feed(&gbk[..mid], false);
        out.push_str(&t.feed(&gbk[mid..], true));
        assert_eq!(out, "测试中文GBK输出");
    }

    #[test]
    fn passes_utf8_through_and_joins_split_sequence() {
        let utf8 = "hello 你好 world".as_bytes();
        let mut t = OutputTranscoder { dec: None, legacy: Some(encoding_rs::GBK) };
        let mid = "hello 你".len(); // 切在 UTF-8 多字节序列中间
        let mut out = t.feed(&utf8[..mid], false);
        out.push_str(&t.feed(&utf8[mid..], true));
        assert_eq!(out, "hello 你好 world");
    }

    #[test]
    fn utf8_detected_even_with_legacy_available() {
        // UTF-8 程序（node/git）输出不应被误判为 GBK
        let utf8 = "中文 utf-8 content".as_bytes();
        let mut t = OutputTranscoder { dec: None, legacy: Some(encoding_rs::GBK) };
        assert_eq!(t.feed(utf8, true), "中文 utf-8 content");
    }

    #[test]
    fn gbk_detected_when_invalid_utf8() {
        // GBK 字节不是合法 UTF-8，应走 legacy 解码
        let gbk: &[u8] = &[0xD6, 0xD0, 0xCE, 0xC4]; // 中文
        let mut t = OutputTranscoder { dec: None, legacy: Some(encoding_rs::GBK) };
        assert_eq!(t.feed(gbk, true), "中文");
    }

    #[cfg(windows)]
    #[test]
    fn oem_encoding_resolved_on_windows() {
        // 中文 Windows 上 OEM CP 为 936（GBK）
        let enc = oem_encoding();
        assert!(enc.is_some());
        assert_eq!(enc.unwrap().name(), encoding_rs::GBK.name());
    }
}
