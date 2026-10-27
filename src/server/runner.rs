use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

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
    /// PTY 会话：伪终端 + ANSI 原样透传，无 stdout/stderr 区分、无输出截断、无超时
    #[serde(default)]
    pub tty: bool,
}

/// SSE 事件负载
#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ExecEvent {
    Output {
        stream: &'static str,
        data: Vec<u8>,
    },
    Exit {
        exit_code: Option<i32>,
        timeout: bool,
        duration_ms: u64,
        truncated: bool,
    },
}

/// stdin 转发消息：Data 为数据块，Eof 表示关闭子进程 stdin（drop Sender 等价于 Eof），
/// Resize 请求调整 PTY 尺寸（非 PTY 会话中被忽略）
#[derive(Debug)]
pub enum StdinMsg {
    Data(Vec<u8>),
    #[allow(dead_code)] // 协议层保留：测试与未来调用方使用
    Eof,
    Resize { cols: u16, rows: u16 },
}

/// 启动子进程（stdin 已接管）：返回 (输出事件流, stdin 发送端, 终止信号)。
/// tty_size 为 PTY 初始尺寸（PTY 会话由客户端随 init 帧上报本地终端尺寸，避免 80x24 初始竞态）。
/// 终止：输出 Receiver 全部 drop、stdin 通道关闭或向 kill 发送 true（客户端断连）→ 子进程被 kill。
/// kill 通道独立于输出 Receiver 存在，保证"断连后仍需等待 Exit 终态写审计"的场景可用。
pub fn spawn_exec_stdin(
    req: ExecRequest,
    tty_size: Option<(u16, u16)>,
) -> (
    mpsc::Receiver<ExecEvent>,
    mpsc::Sender<StdinMsg>,
    tokio::sync::watch::Sender<bool>,
) {
    let (tx, rx) = mpsc::channel::<ExecEvent>(256);
    let (stx, srx) = mpsc::channel::<StdinMsg>(32);
    let (kill_tx, kill_rx) = tokio::sync::watch::channel(false);
    let stx_pty = stx.clone();
    tokio::spawn(async move {
        if req.tty {
            run_pty(req, tx, stx_pty, srx, tty_size, kill_rx).await;
        } else {
            run(req, tx, srx, kill_rx).await;
        }
    });
    (rx, stx, kill_tx)
}

async fn run(
    req: ExecRequest,
    tx: mpsc::Sender<ExecEvent>,
    mut stdin_rx: mpsc::Receiver<StdinMsg>,
    mut kill_rx: tokio::sync::watch::Receiver<bool>,
) {
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
                    StdinMsg::Resize { .. } => {} // 非 PTY 会话忽略 resize
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
        Killed,
    }
    let outcome = tokio::time::timeout(Duration::from_secs(timeout_secs), async {
        loop {
            tokio::select! {
                r = child.wait() => break WaitOut::Code(r.ok().and_then(|s| s.code())),
                // 所有事件接收端已断开（客户端断连）→ 终止子进程
                _ = tx.closed() => break WaitOut::Disconnected,
                // 会话层显式终止（断连后仍需发 Exit 终态供下行写审计）
                _ = kill_rx.changed() => break WaitOut::Killed,
            }
        }
    })
    .await;

    let (exit_code, timed_out) = match outcome {
        Ok(WaitOut::Code(code)) => (code, false),
        Ok(WaitOut::Disconnected) | Ok(WaitOut::Killed) => {
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
                    let _ = send_output(&tx, stream, tail.into_bytes()).await;
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
                                data: Vec::new(),
                            })
                            .await;
                    }
                    continue;
                }
                let text = transcoder.feed(&buf[..n], false);
                if !send_output(&tx, stream, text.into_bytes()).await {
                    // 接收端关闭（客户端断连）
                    break;
                }
            }
        }
    }
}

async fn send_output(tx: &mpsc::Sender<ExecEvent>, stream: &'static str, bytes: Vec<u8>) -> bool {
    tx.send(ExecEvent::Output { stream, data: bytes })
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

/// 白名单匹配：程序名与白名单条目不区分大小写**直接比较**。
/// 不取文件名部分——否则 `D:\evil\ping.exe` / `\\host\share\ping` 等任意路径下的
/// 同名二进制可绕过白名单；客户端应提交裸程序名。
pub fn whitelist_matches(whitelist: &[String], program: &str) -> bool {
    let program = program.to_lowercase();
    whitelist.iter().any(|entry| entry.trim().to_lowercase() == program)
}

/// 查询兜底应答：远端程序的终端查询（CPR/DA1/DA2）由服务端确定性应答。
/// 客户端已在输入侧过滤本地终端的应答回流（ConPTY 会抢答且时序不可控，透传必然
/// 产生迟到的垃圾输入），因此兜底是远端查询的唯一应答来源；同时覆盖服务端侧
/// ConPTY 的启动握手（Windows 服务端下 ConPTY 会向宿主终端发 ESC[6n，不应答会卡死会话）。
fn maybe_query_fallback(seq: &[u8], tx: &mpsc::Sender<StdinMsg>) {
    let reply: &[u8] = match seq {
        b"\x1b[6n" => b"\x1b[1;1R",                  // CPR：光标位置
        b"\x1b[c" => b"\x1b[?1;0c",                  // DA1：VT100 级保守应答
        b"\x1b[>c" | b"\x1b[>0c" => b"\x1b[>0;10;1c", // DA2：次级设备属性
        b"\x1b[?2026$p" => b"\x1b[?2026;2$y",        // DECRQM：同步输出模式
        b"\x1b[?2048$p" => b"\x1b[?2048;0$y",        // DECRQM：备用屏保护
        _ => return,
    };
    macro_log::d!(
        "[pty-query] 服务端检测到查询 {}，立即兜底应答 {}",
        crate::common::escape_bytes(seq),
        crate::common::escape_bytes(reply)
    );
    let reply = reply.to_vec();
    let tx = tx.clone();
    tokio::spawn(async move {
        // 立即发送：客户端已过滤本地终端的应答回流，不存在竞争来源；
        // 延迟会让慢到的兜底应答与后续查询交叉（DA2 查询收到 DA1 应答等串台现象）
        let _ = tx.send(StdinMsg::Data(reply)).await;
    });
}

/// OSC 查询兜底应答：vim 等通过 OSC 10/11 查询前后景颜色，未应答时程序以超时回退。
/// body 为 ESC] 与终结符之间的内容，支持批量查询（含多个 "10;?" / "11;?"），逐个应答。
fn maybe_osc_query_fallback(body: &[u8], tx: &mpsc::Sender<StdinMsg>) {
    if body.len() < 4 {
        return;
    }
    let mut reply = String::new();
    if body.windows(4).any(|w| w == b"10;?") {
        reply.push_str("\x1b]10;rgb:ffff/ffff/ffff\x1b\\");
    }
    if body.windows(4).any(|w| w == b"11;?") {
        reply.push_str("\x1b]11;rgb:0000/0000/0000\x1b\\");
    }
    if reply.is_empty() {
        return;
    }
    macro_log::d!(
        "[pty-query] 服务端检测到 OSC 查询 {}，立即兜底应答 {}",
        crate::common::escape_bytes(body),
        reply
    );
    let tx = tx.clone();
    tokio::spawn(async move {
        let _ = tx.send(StdinMsg::Data(reply.into_bytes())).await;
    });
}

/// 输出扫描状态机：识别 CSI（ESC[）与 OSC（ESC]）序列用于查询兜底
enum ScanState {
    Normal,
    Esc,
    Csi,
    Osc,
    OscEsc,
}

/// 判定完整 CSI 序列是否为终端查询（服务端从输出流中丢弃并兜底应答）
fn is_query_csi(seq: &[u8]) -> bool {
    matches!(
        seq,
        b"\x1b[6n" | b"\x1b[?6n"                       // CPR
        | b"\x1b[c" | b"\x1b[>c" | b"\x1b[>0c"          // DA1/DA2
        | b"\x1b[?996n"                                 // 配色变化通知能力
        | b"\x1b[14t" | b"\x1b[18t"                     // XTWINOPS 像素/单元格尺寸报告
        | b"\x1b[>q"                                    // 光标形状查询
        | b"\x1b[?u"                                    // kitty 键盘增强查询
    ) || (seq.ends_with(b"$p") && seq.starts_with(b"\x1b[?")) // DECRQM 模式查询
}

/// 判定完整 OSC 序列是否为颜色查询。兼容批量发送（vim 将多条 OSC 查询共享一个终结符：
/// `ESC]10;?ESC]11;?BEL`），内容中出现任一 "10;?" / "11;?" 即视为查询。
fn contains_color_query(body: &[u8]) -> bool {
    let pat10 = b"10;?";
    let pat11 = b"11;?";
    if body.len() < pat10.len() {
        return false;
    }
    (0..=body.len() - pat10.len()).any(|i| &body[i..i + pat10.len()] == pat10)
        || (0..=body.len() - pat11.len()).any(|i| &body[i..i + pat11.len()] == pat11)
}

/// PTY 会话：伪终端 + ANSI 透传（Windows ConPTY / Unix openpty）。
/// 无 stdout/stderr 区分（stream="pty"）、无输出截断、无超时；客户端断连时 kill 子进程。
/// 输出流中的终端查询（CPR/DA1/DA2/OSC/DECRQM/XTWINOPS/kitty）被丢弃并由本端确定性
/// 兜底应答（见 maybe_query_fallback / maybe_osc_query_fallback），查询不透传到客户端终端，
/// 从根本上避免宿主终端（WT/ConPTY）的应答回流远端造成乱码。
async fn run_pty(
    req: ExecRequest,
    tx: mpsc::Sender<ExecEvent>,
    stdin_tx: mpsc::Sender<StdinMsg>,
    mut stdin_rx: mpsc::Receiver<StdinMsg>,
    tty_size: Option<(u16, u16)>,
    mut kill_rx: tokio::sync::watch::Receiver<bool>,
) {
    use portable_pty::{CommandBuilder, PtySize};

    let started = Instant::now();
    let (init_cols, init_rows) = tty_size.unwrap_or((80, 24));
    let pty_system = portable_pty::native_pty_system();
    let pair = match pty_system.openpty(PtySize {
        rows: init_rows,
        cols: init_cols,
        pixel_width: 0,
        pixel_height: 0,
    }) {
        Ok(p) => p,
        Err(err) => {
            macro_log::w!("创建伪终端失败 {}: {err}", req.program);
            let _ = tx
                .send(ExecEvent::Exit {
                    exit_code: None,
                    timeout: false,
                    duration_ms: started.elapsed().as_millis() as u64,
                    truncated: false,
                })
                .await;
            return;
        }
    };

    let mut cmd = CommandBuilder::new(&req.program);
    cmd.args(&req.args);
    // 终端类型：Windows 子进程不使用；嵌套场景（docker exec 等）会转发给容器内程序，
    // 缺失会导致容器内 vim 等全屏程序无法初始化
    cmd.env("TERM", "xterm-256color");
    let mut child = match pair.slave.spawn_command(cmd) {
        Ok(c) => c,
        Err(err) => {
            macro_log::w!("PTY 启动进程失败 {}: {err}", req.program);
            let _ = tx
                .send(ExecEvent::Exit {
                    exit_code: None,
                    timeout: false,
                    duration_ms: started.elapsed().as_millis() as u64,
                    truncated: false,
                })
                .await;
            return;
        }
    };
    drop(pair.slave); // 父进程立即释放 slave 端
    macro_log::d!(
        "[pty] 服务端会话已启动 {} size={:?} 初次握手查询将由兜底应答",
        req.program,
        tty_size
    );

    let reader = match pair.master.try_clone_reader() {
        Ok(r) => r,
        Err(err) => {
            macro_log::w!("PTY 读取端获取失败 {}: {err}", req.program);
            let _ = child.clone_killer().kill();
            let _ = tx
                .send(ExecEvent::Exit {
                    exit_code: None,
                    timeout: false,
                    duration_ms: started.elapsed().as_millis() as u64,
                    truncated: false,
                })
                .await;
            return;
        }
    };
    let master = Arc::new(std::sync::Mutex::new(pair.master));

    // stdin 转发：专用阻塞写线程独占 PTY 写端（消除逐键 spawn_blocking 与双层锁竞争），
    // 异步任务经有界 std 通道投递；通道满（PTY 消费不动）时轮询让出执行权，不阻塞运行时线程。
    let writer_master = Arc::clone(&master);
    let tx_writer = tx.clone();
    let (wtx, wrx) = std::sync::mpsc::sync_channel::<Vec<u8>>(32);
    let _writer_task = tokio::spawn(async move {
        // 预取写端：此后写线程独占持有，无锁
        let taken = writer_master
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take_writer()
            .ok();
        let write_thread = std::thread::spawn(move || {
            let Some(mut w) = taken else { return; };
            while let Ok(data) = wrx.recv() {
                if w.write_all(&data).and_then(|_| w.flush()).is_err() {
                    break;
                }
            }
        });
        let mut closed = std::pin::pin!(tx_writer.closed());
        loop {
            let msg = tokio::select! {
                m = stdin_rx.recv() => match m {
                    Some(m) => m,
                    None => break,
                },
                _ = &mut closed => break, // 会话结束（输出事件接收端已 drop）
            };
            match msg {
                StdinMsg::Data(data) => {
                    macro_log::d!(
                        "[pty-stdin] 服务端向 PTY 写入 {} 字节: {}",
                        data.len(),
                        crate::common::escape_bytes(&data)
                    );
                    // 背压：写线程消费不动时让出重试（try_send 失败归还数据）
                    let mut data = data;
                    let delivered = loop {
                        match wtx.try_send(data) {
                            Ok(()) => break true,
                            Err(std::sync::mpsc::TrySendError::Full(d)) => {
                                data = d;
                                tokio::time::sleep(Duration::from_millis(10)).await;
                            }
                            Err(std::sync::mpsc::TrySendError::Disconnected(_)) => break false, // 写线程已退出（PTY 写失败）
                        }
                    };
                    if !delivered {
                        break;
                    }
                }
                StdinMsg::Eof => break,
                StdinMsg::Resize { cols, rows } => {
                    let _ = writer_master
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .resize(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 });
                }
            }
        }
        drop(wtx); // 通知写线程退出
        let _ = write_thread.join();
    });

    // 输出泵：透传输出流，但**丢弃终端查询序列**（CPR/DA1/DA2/OSC 颜色/DECRQM/XTWINOPS
    // 查询/kitty 键盘查询）并由本端确定性兜底应答。查询一旦透传到宿主终端（WT/ConPTY），
    // 其应答会以按键形式回流远端造成乱码，且应答类型不可穷举，必须在源头拦截。
    let tx_reader = tx.clone();
    let cpr_tx = stdin_tx;
    let reader_task = tokio::task::spawn_blocking(move || {
        use std::io::Read as _;
        let mut reader = reader;
        let mut buf = [0u8; 8192];
        let mut seq: Vec<u8> = Vec::new(); // 跨块收集的转义序列
        let mut pass: Vec<u8> = Vec::with_capacity(buf.len()); // 每块非查询字节
        let mut scan = ScanState::Normal;
        loop {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    for &b in &buf[..n] {
                        match scan {
                            ScanState::Normal => {
                                if b == 0x1b {
                                    seq.clear();
                                    seq.push(b);
                                    scan = ScanState::Esc;
                                } else {
                                    pass.push(b);
                                }
                            }
                            ScanState::Esc => {
                                seq.push(b);
                                match b {
                                    b'[' => scan = ScanState::Csi,
                                    b']' => scan = ScanState::Osc,
                                    0x1b => {
                                        seq.clear();
                                        seq.push(b);
                                    }
                                    _ => {
                                        pass.extend_from_slice(&seq);
                                        seq.clear();
                                        scan = ScanState::Normal;
                                    }
                                }
                            }
                            ScanState::Csi => {
                                seq.push(b);
                                if (0x40..=0x7e).contains(&b) {
                                    if is_query_csi(&seq) {
                                        macro_log::d!(
                                            "[pty-query] 丢弃输出流查询 {} 并兜底应答",
                                            crate::common::escape_bytes(&seq)
                                        );
                                        maybe_query_fallback(&seq, &cpr_tx);
                                    } else {
                                        pass.extend_from_slice(&seq);
                                    }
                                    seq.clear();
                                    scan = ScanState::Normal;
                                } else if seq.len() >= 32 {
                                    pass.extend_from_slice(&seq);
                                    seq.clear(); // 异常序列，放弃
                                    scan = ScanState::Normal;
                                }
                            }
                            ScanState::Osc => match b {
                                0x07 => {
                                    maybe_osc_query_fallback(&seq[2..seq.len() - 1], &cpr_tx);
                                    seq.clear();
                                    scan = ScanState::Normal;
                                }
                                0x1b => {
                                    seq.push(b);
                                    scan = ScanState::OscEsc;
                                }
                                _ => {
                                    seq.push(b);
                                    if seq.len() >= 128 {
                                        pass.extend_from_slice(&seq);
                                        seq.clear(); // 异常序列，放弃
                                        scan = ScanState::Normal;
                                    }
                                }
                            },
                            ScanState::OscEsc => {
                                // ESC 后非 '\\'：前一条 OSC 未终结（vim 批量查询共享终结符）
                                // 先按无终结符处理已收集内容，再以当前 ESC 重新锚定新序列
                                maybe_osc_query_fallback(&seq[2..], &cpr_tx);
                                seq.clear();
                                seq.push(0x1b);
                                seq.push(b);
                                match b {
                                    b'[' => scan = ScanState::Csi,
                                    b']' => scan = ScanState::Osc,
                                    0x1b => scan = ScanState::Esc,
                                    _ => {
                                        pass.extend_from_slice(&seq);
                                        seq.clear();
                                        scan = ScanState::Normal;
                                    }
                                }
                            }
                        }
                    }
                    if !pass.is_empty() {
                        // take 出整块数据发送，重建缓冲保留容量
                        let out = std::mem::take(&mut pass);
                        if tx_reader
                            .blocking_send(ExecEvent::Output { stream: "pty", data: out })
                            .is_err()
                        {
                            break;
                        }
                        pass = Vec::with_capacity(buf.len());
                    }
                }
            }
        }
    });

    // 等待退出；客户端断连（tx.closed 或会话层 kill 信号）→ kill。PTY 会话无超时。
    let mut killer = child.clone_killer();
    let mut wait_task = tokio::task::spawn_blocking(move || child.wait());
    let mut exit_code: Option<i32> = None;
    tokio::select! {
        st = &mut wait_task => {
            exit_code = st.ok().and_then(|r| r.ok()).map(|s| s.exit_code() as i32);
        }
        _ = tx.closed() => {
            let _ = killer.kill();
            let _ = tokio::time::timeout(Duration::from_secs(5), &mut wait_task).await;
        }
        _ = kill_rx.changed() => {
            // 会话层显式终止（断连后仍需发 Exit 终态供下行写审计）
            let _ = killer.kill();
            let _ = tokio::time::timeout(Duration::from_secs(5), &mut wait_task).await;
        }
    }

    // 限时排空残余输出（孙进程持有 tty 时可能永不 EOF，超时放弃）
    let _ = tokio::time::timeout(Duration::from_secs(2), reader_task).await;

    let _ = tx
        .send(ExecEvent::Exit {
            exit_code,
            timeout: false,
            duration_ms: started.elapsed().as_millis() as u64,
            truncated: false,
        })
        .await;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 白名单：程序名直接比较，任意路径下的同名二进制不得绕过
    #[test]
    fn whitelist_rejects_path_prefixed_program() {
        let wl = ["ping".to_string(), "git".to_string()];
        assert!(whitelist_matches(&wl, "ping"));
        assert!(whitelist_matches(&wl, "GIT"));
        // 含路径分隔符的同名程序：即使文件名在白名单内也必须拒绝
        assert!(!whitelist_matches(&wl, "D:\\evil\\ping.exe"));
        assert!(!whitelist_matches(&wl, "\\\\host\\share\\ping"));
        assert!(!whitelist_matches(&wl, "/tmp/git"));
        assert!(!whitelist_matches(&wl, ""));
    }

    /// CPR/DA1/DA2/DECRQM 查询识别
    #[test]
    fn query_csi_detection() {
        assert!(is_query_csi(b"\x1b[6n"));
        assert!(is_query_csi(b"\x1b[?6n"));
        assert!(is_query_csi(b"\x1b[c"));
        assert!(is_query_csi(b"\x1b[>c"));
        assert!(is_query_csi(b"\x1b[?996n"));
        assert!(is_query_csi(b"\x1b[14t"));
        assert!(is_query_csi(b"\x1b[18t"));
        assert!(is_query_csi(b"\x1b[>q"));
        assert!(is_query_csi(b"\x1b[?2026$p"));
        assert!(is_query_csi(b"\x1b[?2048$p"));
        // 非查询（输出命令）不得误判
        assert!(!is_query_csi(b"\x1b[2J"));
        assert!(!is_query_csi(b"\x1b[22;0;0t")); // 压栈标题命令
        assert!(!is_query_csi(b"\x1b[38;2;17;1m"));
        assert!(!is_query_csi(b"\x1b[?25h"));
    }

    /// OSC 颜色查询识别（含批量共享终结符）
    #[test]
    fn osc_query_detection() {
        assert!(contains_color_query(b"10;?"));
        assert!(contains_color_query(b"11;?"));
        assert!(contains_color_query(b"10;?]11;?")); // vim 批量共享终结符
        assert!(!contains_color_query(b"0;title"));
        assert!(!contains_color_query(b"2;other"));
    }

    /// CPR/DA1 查询应触发兜底应答，非查询序列不应答
    #[tokio::test]
    async fn query_fallback_replies_to_cpr_and_da1() {
        let (tx, mut rx) = mpsc::channel(8);
        maybe_query_fallback(b"\x1b[6n", &tx);
        maybe_query_fallback(b"\x1b[c", &tx);
        maybe_query_fallback(b"\x1b[?25h", &tx); // 非查询，不应答

        let first = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("CPR 兜底超时");
        let second = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("DA1 兜底超时");
        match (first, second) {
            (Some(StdinMsg::Data(a)), Some(StdinMsg::Data(b))) => {
                assert_eq!(a, b"\x1b[1;1R");
                assert_eq!(b, b"\x1b[?1;0c");
            }
            _ => panic!("兜底应答类型错误"),
        }
        // 无第三条消息（非查询未应答）
        let third = tokio::time::timeout(Duration::from_millis(400), rx.recv()).await;
        assert!(third.is_err(), "非查询序列不应触发应答");
    }

    /// stdin 数据经 WS 会话转发后应到达子进程（Windows: more 回显；Unix: cat 回显）
    #[tokio::test]
    async fn stdin_pipe_forwarded_to_child() {
        #[cfg(windows)]
        let req = ExecRequest {
            program: "cmd".into(),
            args: ["/C".to_string(), "more".to_string()].to_vec(),
            timeout_secs: Some(15),
            tty: false,
        };
        #[cfg(not(windows))]
        let req = ExecRequest {
            program: "cat".into(),
            args: vec![],
            timeout_secs: Some(15),
            tty: false,
        };

        let (mut rx, tx, _kill) = spawn_exec_stdin(req, None);
        tx.send(StdinMsg::Data(b"hello stdin\n".to_vec())).await.unwrap();
        tx.send(StdinMsg::Eof).await.unwrap();
        drop(tx);

        let mut stdout = String::new();
        let mut exit_code = None;
        while let Some(ev) = rx.recv().await {
            match ev {
                ExecEvent::Output { stream, data } if stream == "stdout" => {
                    stdout.push_str(&String::from_utf8_lossy(&data));
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

    /// PTY 会话冒烟：子进程经伪终端执行，输出透传，退出码正确
    #[tokio::test]
    async fn pty_session_runs_and_exits() {
        #[cfg(windows)]
        let req = ExecRequest {
            program: "cmd".into(),
            args: ["/C".to_string(), "echo pty-hello".to_string()].to_vec(),
            timeout_secs: None,
            tty: true,
        };
        #[cfg(not(windows))]
        let req = ExecRequest {
            program: "sh".into(),
            args: ["-c".to_string(), "echo pty-hello".to_string()].to_vec(),
            timeout_secs: None,
            tty: true,
        };

        let (mut rx, _tx, _kill) = spawn_exec_stdin(req, None);
        let mut output = String::new();
        let mut exit_code = None;
        let completed = tokio::time::timeout(std::time::Duration::from_secs(10), async {
            while let Some(ev) = rx.recv().await {
                match ev {
                    ExecEvent::Output { data, .. } => {
                        output.push_str(&String::from_utf8_lossy(&data));
                    }
                    ExecEvent::Exit { exit_code: c, .. } => {
                        exit_code = c;
                        break;
                    }
                }
            }
        })
        .await;
        assert!(completed.is_ok(), "PTY 会话未在时限内退出，output = {output:?}");
        assert_eq!(exit_code, Some(0));
        // ConPTY 渲染可能把回显内容拆分成屏幕差异片段，放宽为关键字匹配
        assert!(output.contains("hello"), "pty output = {output:?}");
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
