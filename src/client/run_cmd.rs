use std::io::{IsTerminal, Read as _, Write as _};

use anyhow::{anyhow, Context, Result};
use futures::{SinkExt, StreamExt};
use serde_json::json;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message as WsMessage;

use super::config::{load_aliases, render_template, ClientConfig, ServerReg};
use super::remote::{
    parse_frame, register, resize_msg, trace_frame, ws_dir_setup, ws_exec, ws_upload, ExecFrame,
    ExecWs, STDIN_CLOSE,
};
use crate::common::fingerprint::MachineInfo;

/// WS 心跳间隔：空闲会话周期发 Ping，防 NAT/代理静默掐断长连接
const PING_INTERVAL: std::time::Duration = std::time::Duration::from_secs(30);

/// 失焦后突发输入的静默判定窗：拖放/粘贴的路径字节间隔 <1ms，静默 10ms 视为输入完毕
const BURST_QUIET: std::time::Duration = std::time::Duration::from_millis(10);

/// 应答判定滞留字节的空闲冲刷窗：应答序列逐字节间隔 <1ms，50ms 无后续即可判定
/// 滞留字节（孤立的裸 `[` / ESC）为普通输入，冲刷转发避免其回显延迟到下一键
const REPLY_FLUSH_IDLE: std::time::Duration = std::time::Duration::from_millis(50);

/// `remote-cmd run <别名> [args...]`：读取本地配置 → 模板渲染 → WS 会话 → stdin 转发 + 流式输出 → 透传退出码。
/// `verbose=false`（别名 shim 模式）时抑制 stderr 提示信息，避免污染包装脚本的输出处理。
pub async fn run(alias: &str, args: &[String], verbose: bool) -> Result<()> {
    let (server, body) = resolve_exec_target(alias, args)?;
    let program = body["program"].as_str().unwrap_or_default().to_string();
    let cmd_args: Vec<String> = serde_json::from_value(body["args"].clone())?;

    // 本地 stdin 为终端 → PTY 附加会话（raw mode + resize 同步，无超时）；
    // 管道 stdin → 字节流转发 + 流式输出（服务端默认超时）
    let pty = std::io::stdin().is_terminal();
    // PTY 会话：本地尺寸随 init 帧上报（服务端按此创建 PTY，避免 80x24 初始竞态）
    let tty_size = if pty { crossterm::terminal::size().ok() } else { None };
    let ExecWs { sink, stream, session_id } = match ws_exec_recovered(&server, &program, &cmd_args, pty, tty_size)
        .await
    {
        Ok(w) => w,
        Err(e) => anyhow::bail!("请求服务器失败: {e}"),
    };

    if pty {
        return run_pty_session(sink, stream, verbose, server, session_id).await;
    }

    // 管道 stdin → 转发原始字节，EOF 时发 stdin_close
    let stdin_task = spawn_stdin_pump(sink);

    // 输出泵：Binary 按 tag 分流；Exit 取退出码（会话级复用句柄，避免每帧新建）
    let mut exit_code: Option<i32> = None;
    let mut stdout = std::io::stdout();
    let mut stderr = std::io::stderr();
    let mut stream = stream;
    while let Some(msg) = stream.next().await {
        match msg {
            Ok(frame) => {
                let control = trace_frame(&frame);
                match parse_frame(frame) {
                    Some(ExecFrame::Output { stream, data }) => {
                        if stream == "stderr" {
                            let _ = stderr.write_all(&data);
                            let _ = stderr.flush();
                        } else {
                            let _ = stdout.write_all(&data);
                            let _ = stdout.flush();
                        }
                    }
                    Some(ExecFrame::Exit { exit_code: code, timeout, .. }) => {
                        crate::dlog!("[run] 收到 Exit 终态 exit_code={code:?} timeout={timeout}");
                        if timeout {
                            crate::verbose!(verbose, "[remote-cmd] 远程命令执行超时");
                        }
                        exit_code = code;
                        break;
                    }
                    None => {
                        if !control {
                            crate::dlog!("[run] 收到无法解析的 WS 帧（未知 Binary 标签或非 exit JSON）");
                        }
                    }
                }
            }
            Err(e) => {
                crate::dlog!("[run] WS 连接错误: {e}");
                crate::verbose!(verbose, "[remote-cmd] 连接中断: {e}");
                break;
            }
        }
    }
    if exit_code.is_none() {
        crate::dlog!("[run] 输出流结束且未见 Exit 终态（连接被服务端关闭）");
    }

    stdin_task.abort();

    match exit_code {
        Some(code) => {
            crate::dlog!("[run] 进程退出 exit_code={code}");
            std::process::exit(code)
        }
        None => {
            // 进程被 kill / 启动失败 / 连接中断
            crate::dlog!("[run] 进程异常退出（无 Exit 终态），exit 1");
            crate::verbose!(verbose, "[remote-cmd] 远程命令异常退出");
            std::process::exit(1);
        }
    }
}

/// stdin 泵：阻塞读本地 stdin（管道字节流）→ WS Binary 帧 → 本地 EOF 时发 stdin_close
fn spawn_stdin_pump(
    mut sink: futures::stream::SplitSink<super::remote::WsStream, WsMessage>,
) -> tokio::task::JoinHandle<()> {
    let (tx, mut rx) = mpsc::channel::<Vec<u8>>(16);
    let reader = tokio::task::spawn_blocking(move || {
        let mut stdin = std::io::stdin();
        let mut buf = [0u8; 4096];
        loop {
            match stdin.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if tx.blocking_send(buf[..n].to_vec()).is_err() {
                        break;
                    }
                }
            }
        }
    });
    tokio::spawn(async move {
        let mut ping = tokio::time::interval(PING_INTERVAL);
        loop {
            let msg = tokio::select! {
                data = rx.recv() => match data {
                    Some(data) => WsMessage::Binary(data.into()),
                    None => {
                        crate::dlog!("[stdin] 本地 stdin EOF，发送 stdin_close");
                        // 本地 EOF → 通知服务端关闭子进程 stdin
                        let _ = sink.send(WsMessage::Text(STDIN_CLOSE.into())).await;
                        let _ = reader.await;
                        crate::dlog!("[stdin] stdin 泵结束");
                        return;
                    }
                },
                _ = ping.tick() => {
                    crate::dlog!("[ws] 空闲心跳：发送 Ping");
                    WsMessage::Ping(vec![].into())
                }
            };
            if sink.send(msg).await.is_err() {
                crate::dlog!("[stdin] WS 发送失败，stdin 泵终止");
                return;
            }
        }
    })
}

/// PTY 交互会话：本地终端 raw mode，按键即时转发、resize 同步、ANSI 原样渲染。
/// Ctrl+] 脱离会话（断开连接，远端子进程被服务端终止）。
/// 失焦后的首次输入（拖放/粘贴的文件路径）被缓冲判定：解析为全部存在的本地文件
/// 时上传到服务端并把远端路径注入 shell，否则原文透传（见 InputAction::Upload）。
async fn run_pty_session(
    mut sink: futures::stream::SplitSink<super::remote::WsStream, WsMessage>,
    mut stream: futures::stream::SplitStream<super::remote::WsStream>,
    verbose: bool,
    server: ServerReg,
    session_id: String,
) -> Result<()> {
    use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers};

    crossterm::terminal::enable_raw_mode().map_err(|e| anyhow!("进入 raw mode 失败: {e}"))?;
    // 焦点上报：失焦事件武装"拖放/粘贴路径 → 上传"检测（Windows FOCUS_EVENT / Unix 1004 模式）
    let _ = crossterm::execute!(std::io::stdout(), crossterm::event::EnableFocusChange);
    let _raw = RawModeGuard::new(); // Drop 兜底恢复（显式退出路径已在 exit 前手动关闭）

    enum InputAction {
        Data(Vec<u8>),
        /// 失焦后突发输入被判定为本地文件（拖放/粘贴路径）：上传远端后注入远端路径；
        /// raw 为原始字节，任一文件上传失败时回退原文透传
        Upload { files: Vec<std::path::PathBuf>, raw: Vec<u8> },
        Resize(u16, u16),
        Detach,
    }

    // 本地输入线程：poll 循环读控制台事件 → 通道。
    // 拖放文件到命令行窗口必先失焦，路径文本随后写入（字节间隔 <1ms）；焦点恢复与
    // 输入到达的顺序不定（失焦→输入→获焦 / 失焦→获焦→输入），因此仅以失焦事件武装、
    // 首次输入触发突发收集后立即解除武装，不依赖焦点恢复时序。
    let (itx, mut irx) = mpsc::channel::<InputAction>(64);
    let _input_thread = std::thread::spawn(move || {
        let mut armed = false;
        loop {
            // 非阻塞读取事件，避免阻塞主循环。
            // 仅在有事件时才处理，避免空轮询；无事件时，继续等待下一次轮询；事件处理失败时，退出循环。
            // 代价：空闲时每秒 5 次的 poll 空转，对性能影响较小。
            match crossterm::event::poll(std::time::Duration::from_millis(200)) {
                Ok(true) => {}
                Ok(false) => continue,
                Err(_) => break,
            }
            // 此函数会阻塞，直到有可用的事件为止
            // 将其与poll函数结合使用，即可实现非阻塞读取
            match crossterm::event::read() {
                Ok(Event::FocusLost) => armed = true,
                Ok(Event::Key(k)) if k.kind == KeyEventKind::Press => {
                    if k.modifiers.contains(KeyModifiers::CONTROL) && k.code == KeyCode::Char(']') {
                        let _ = itx.blocking_send(InputAction::Detach);
                        break;
                    }
                    let Some(bytes) = key_to_bytes(k) else { continue };
                    if armed {
                        let Some(burst) = collect_burst(bytes) else { break };
                        armed = false; // 仅失焦后的首次输入参与判定，无论结果如何解除武装
                        let action = match upload_paths(&burst) {
                            Some(files) => InputAction::Upload { files, raw: burst },
                            None => InputAction::Data(burst),
                        };
                        if itx.blocking_send(action).is_err() {
                            break;
                        }
                    } else if itx.blocking_send(InputAction::Data(bytes)).is_err() {
                        break;
                    }
                }
                Ok(Event::Resize(cols, rows)) => {
                    let _ = itx.blocking_send(InputAction::Resize(cols, rows));
                }
                Ok(_) => {} // 焦点恢复等事件不解除武装
                Err(_) => break,
            }
        }
    });

    // 输入转发任务：本地事件 → WS 帧（经 ReplyFilter 吞掉终端应答序列）；Detach 时通知主循环断开。
    // 空闲时周期发 Ping 心跳，防 NAT/代理静默掐断空闲 PTY 会话（axum 服务端自动回 Pong）。
    let (detach_tx, mut detach_rx) = mpsc::channel::<()>(1);
    let mut reply_filter = ReplyFilter::new();
    let upload_url = server.server_url.clone();
    let upload_token = server.token.clone().unwrap_or_default();
    let upload_cert = server.cert_sha256.clone();
    // 上传 init 帧回传 session_id：服务端据此定位本会话 shell 进程 cwd（落地工作目录）
    let upload_session = session_id;
    let forward = tokio::spawn(async move {
        let mut ping = tokio::time::interval(PING_INTERVAL);
        loop {
            let msg = tokio::select! {
                // 空闲冲刷：应答判定滞留字节（裸 `[` / ESC）超时无后续 → 按普通输入放行。
                // 应答序列逐字节间隔 <1ms，50ms 静默后到达的字节不可能是其残余。
                a = tokio::time::timeout(REPLY_FLUSH_IDLE, irx.recv()) => match a {
                    Err(_) => {
                        let pending = reply_filter.take_pending();
                        if pending.is_empty() {
                            continue;
                        }
                        crate::dlog!(
                            "[pty-in] 空闲冲刷滞留判定字节 {} 字节: {}",
                            pending.len(),
                            crate::common::escape_bytes(&pending)
                        );
                        WsMessage::Binary(pending.into())
                    }
                    Ok(None) => break,
                    Ok(Some(a)) => match a {
                        InputAction::Data(d) => match reply_filter.feed(&d) {
                            filtered if filtered.is_empty() && !d.is_empty() => {
                                crate::dlog!(
                                    "[pty-in] 客户端过滤器吞掉本地应答 {} 字节: {}",
                                    d.len(),
                                    crate::common::escape_bytes(&d)
                                );
                                continue; // 终端应答被吞掉，不转发远端
                            }
                            filtered => {
                                if !d.is_empty() {
                                    crate::dlog!(
                                        "[pty-in] 客户端输入 {} 字节 -> 转发 {} 字节: {}",
                                        d.len(),
                                        filtered.len(),
                                        crate::common::escape_bytes(&filtered)
                                    );
                                }
                                WsMessage::Binary(filtered.into())
                            }
                        },
                        InputAction::Upload { files, raw } => {
                            // 逐目标上传（文件 / 目录混拖支持，独立 WS 连接）：全部成功 →
                            // 把远端路径注入远端 shell（等价于用户输入路径参数），
                            // 任一目标整体失败 → 回退原文透传。
                            let mut remote = Vec::with_capacity(files.len());
                            let mut failed = false;
                            for f in &files {
                                if f.is_file() {
                                    let name = f
                                        .file_name()
                                        .map(|s| s.to_string_lossy().into_owned())
                                        .unwrap_or_default();
                                    let size = std::fs::metadata(f).map(|m| m.len()).unwrap_or(0);
                                    let res = match tokio::fs::File::open(f).await {
                                        Ok(mut file) => {
                                            ws_upload(&upload_url, &upload_token, &name, size, &mut file, &upload_session, "", false, upload_cert.as_deref())
                                                .await
                                        }
                                        Err(e) => Err(anyhow!("本地文件打开失败: {e}")),
                                    };
                                    match res {
                                        Ok(p) => remote.push(p),
                                        Err(e) => {
                                            crate::dlog!("[upload] {name} 上传失败: {e:#}");
                                            failed = true;
                                        }
                                    }
                                } else {
                                    // 目录：本地收集树（限深，匹配服务端 rel_path ≤16 段）→
                                    // mkdir 还原空目录 → 逐文件带 rel_path 上传
                                    match collect_dir_tree(f) {
                                        Ok(tree) => {
                                            match upload_dir_tree(&upload_url, &upload_token, &upload_session, upload_cert.as_deref(), &tree).await {
                                                Some(p) => remote.push(p),
                                                None => {
                                                    crate::dlog!("[upload] 目录 {} 上传失败（无成功文件）", f.display());
                                                    failed = true;
                                                }
                                            }
                                        }
                                        Err(e) => {
                                            crate::dlog!("[upload] 目录 {} 收集失败: {e:#}", f.display());
                                            failed = true;
                                        }
                                    }
                                }
                            }
                            let text = if failed || remote.is_empty() {
                                String::from_utf8_lossy(&raw).into_owned()
                            } else {
                                crate::dlog!("[upload] 已上传 {} 个目标: {}", remote.len(), remote.join(" "));
                                remote.iter().map(|p| quote_for_shell(p)).collect::<Vec<_>>().join(" ")
                            };
                            let filtered = reply_filter.feed(text.as_bytes());
                            if sink.send(WsMessage::Binary(filtered.into())).await.is_err() {
                                crate::dlog!("[pty-in] WS 发送失败（上传结果注入），输入转发终止");
                                break;
                            }
                            continue;
                        }
                        InputAction::Resize(c, r) => {
                            crate::dlog!("[pty-in] 客户端上报终端尺寸 {}x{}", c, r);
                            resize_msg(c, r)
                        }
                        InputAction::Detach => {
                            crate::dlog!("[pty-in] 客户端脱离会话（Ctrl+]）");
                            let _ = detach_tx.send(()).await;
                            // 先发 WS Close 帧让服务端感知正常关闭（不等待回复，随后立即退出）
                            let _ = sink.close().await;
                            break;
                        }
                    }
                },
                _ = ping.tick() => {
                    crate::dlog!("[ws] 空闲心跳：发送 Ping");
                    WsMessage::Ping(vec![].into())
                }
            };
            if sink.send(msg).await.is_err() {
                crate::dlog!("[ws] 输入帧发送失败，输入转发终止（连接可能已断开）");
                break;
            }
        }
    });

    // 输出泵：PTY 输出为原始 ANSI 字节，直接写本地终端（不做任何过滤）。
    // 远端程序的终端查询（CPR/DA/OSC）随之透传到本地终端显示；但本地终端的应答
    // 会在输入侧被 ReplyFilter 吞掉，远端查询由服务端确定性兜底应答（见 runner.rs）。
    // 不透传应答的原因：ConPTY 会抢答查询（conhost 内置 DA/CPR 应答），且其启动
    // 握手应答也以按键事件回流，原样转发会在查询方放弃等待后污染远端输入缓冲。
    let mut exit_code: Option<i32> = None;
    let mut detached = false;
    let mut out = std::io::stdout();
    crate::dlog!("[pty] 进入交互会话循环");
    loop {
        tokio::select! {
            maybe = stream.next() => match maybe {
                Some(Ok(frame)) => {
                    let control = trace_frame(&frame);
                    match parse_frame(frame) {
                        Some(ExecFrame::Output { data, .. }) => {
                            if crate::common::looks_like_query(&data) {
                                crate::dlog!(
                                    "[pty-out] 客户端收到远端查询 {} 字节: {}",
                                    data.len(),
                                    crate::common::escape_bytes(&data)
                                );
                            }
                            let _ = out.write_all(&data);
                            let _ = out.flush();
                        }
                        Some(ExecFrame::Exit { exit_code: code, timeout, duration_ms, truncated }) => {
                            crate::dlog!(
                                "[pty] 收到 Exit 终态 exit_code={code:?} timeout={timeout} duration_ms={duration_ms} truncated={truncated}"
                            );
                            exit_code = code;
                            break;
                        }
                        None => {
                            if !control {
                                crate::dlog!("[pty-out] 收到无法解析的 WS 帧（未知 Binary 标签或非 exit JSON）");
                            }
                        }
                    }
                }
                Some(Err(e)) => {
                    crate::dlog!("[pty] WS 连接错误: {e}");
                    crate::verbose!(verbose, "[remote-cmd] 连接中断: {e}");
                    break;
                }
                None => {
                    crate::dlog!("[pty] 输出流结束且未见 Exit/Close（连接被服务端静默关闭）");
                    break;
                }
            },
            _ = detach_rx.recv() => {
                detached = true;
                break;
            }
        }
    }
    crate::dlog!("[pty] 会话循环结束 detached={detached} exit_code={exit_code:?}");

    forward.abort();
    let _ = crossterm::terminal::disable_raw_mode();
    let _ = crossterm::execute!(std::io::stdout(), crossterm::event::DisableFocusChange);
    drop(_raw); // 避免 Drop 重复 disable（disable 幂等，仅语义清晰）

    if detached {
        crate::verbose!(verbose, "[remote-cmd] 已脱离会话，远端进程将被终止");
        std::process::exit(0); // 不经 Drop（已手动恢复终端），立即断开
    }

    match exit_code {
        Some(code) => {
            crate::dlog!("[pty] 进程退出 exit_code={code}");
            // 交互会话：远端退出（含容器重启杀 exec 等场景）必须给出可见提示，
            // 否则表现为"无任何提示的自动退出"（退出码本身仍透传）
            crate::verbose!(verbose, "[remote-cmd] 远程会话已结束，退出码 {code}");
            std::process::exit(code)
        }
        None => {
            crate::dlog!("[pty] 进程异常退出（无 Exit 终态），exit 1");
            crate::verbose!(verbose, "[remote-cmd] 远程命令异常退出");
            std::process::exit(1);
        }
    }
}

/// 收集失焦后的首次输入突发：新输入不断续期，静默 BURST_QUIET（10ms）视为输入完毕。
/// 拖放/粘贴的路径字节流间隔 <1ms 会合成一个突发，而正常打键间隔远大于 10ms，
/// 每个按键单独成突发、逐个透传，不产生可感知延迟。
/// 返回 None 表示输入源出错（放弃收集，且不应继续读输入线程循环）。
fn collect_burst(first: Vec<u8>) -> Option<Vec<u8>> {
    use crossterm::event::{Event, KeyEventKind};
    let mut burst = first;
    loop {
        match crossterm::event::poll(BURST_QUIET) {
            Ok(true) => {}
            Ok(false) => return Some(burst), // 静默超时：输入完毕
            Err(_) => return None,
        }
        match crossterm::event::read() {
            Ok(Event::Key(k)) if k.kind == KeyEventKind::Press => {
                if let Some(b) = key_to_bytes(k) {
                    burst.extend_from_slice(&b);
                }
            }
            Ok(_) => {} // 焦点/Resize 等事件不打断收集
            Err(_) => return None,
        }
    }
}

/// 判定突发输入是否为"全部存在的本地路径"（拖放/粘贴上传触发条件，文件与目录均可）：
/// 优先按引号语义解析（资源管理器拖放/规范粘贴，空格分隔、含空格路径 "" 包裹），
/// 全部 token 均存在 → 触发上传（文件走单文件上传，目录走目录树上传）；
/// 否则若整串不含引号，视为**单个**无引号路径（含空格），整串查存在性；
/// 都不满足 → 返回 None，原始字节按普通输入透传给远端。
fn upload_paths(burst: &[u8]) -> Option<Vec<std::path::PathBuf>> {
    let text = std::str::from_utf8(burst).ok()?;
    let text = text.trim_matches(['\r', '\n', ' ']);
    if text.is_empty() {
        return None;
    }

    // 引号语义：空格分隔，含空格路径以 "" 包裹
    let paths = parse_paths(text);
    if !paths.is_empty() {
        let files: Vec<std::path::PathBuf> = paths.into_iter().map(std::path::PathBuf::from).collect();
        if files.iter().all(|p| p.exists()) {
            return Some(files);
        }
    }

    // 无引号含空格：只可能是单个路径，整串当路径查（存在即为单路径，否则透传）
    if !text.contains('"') {
        let p = std::path::PathBuf::from(text);
        if p.exists() {
            return Some(vec![p]);
        }
    }
    None
}

/// 解析拖放/粘贴的路径串：空格分隔；含空格的路径用双引号包裹（资源管理器拖放行为）。
/// 采用 Win32 CommandLineToArgvW 语义：引号切换引内模式，引内 `""` 为字面引号，
/// 反斜杠仅在紧邻引号前成对转义（奇数个时该引号字面化）——同时兼容含双引号的
/// Unix 路径（如 `"/tmp/a""b.txt"` → `/tmp/a"b.txt"`）。
fn parse_paths(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_quotes = false;
    let mut has_token = false;
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            ' ' | '\t' if !in_quotes => {
                if has_token {
                    out.push(std::mem::take(&mut cur));
                    has_token = false;
                }
            }
            '"' => {
                // 收集紧邻的反斜杠：奇数个时引号被转义为字面，偶数个（含 0）时切换模式
                let mut bs = 0usize;
                while cur.ends_with('\\') {
                    bs += 1;
                    cur.pop();
                }
                if bs % 2 == 1 {
                    for _ in 0..bs / 2 {
                        cur.push('\\');
                    }
                    cur.push('"');
                } else {
                    for _ in 0..bs / 2 {
                        cur.push('\\');
                    }
                    if in_quotes && chars.peek() == Some(&'"') {
                        chars.next(); // 引内 "" → 字面引号
                        cur.push('"');
                    } else {
                        in_quotes = !in_quotes;
                    }
                }
                has_token = true;
            }
            _ => {
                cur.push(c);
                has_token = true;
            }
        }
    }
    if has_token {
        out.push(cur);
    }
    out
}

/// 远端路径 → shell 参数文本：统一 POSIX 单引号字面量语义（`$`、反引号、`"` 均不展开，
/// 防止不可信文件名在远端 shell 触发命令替换），内部单引号以 `'\''` 转义
fn quote_for_shell(p: &str) -> String {
    format!("'{}'", p.replace('\'', "'\\''"))
}

// ---------- CLI 目录上传（拖放目录到终端） ----------

/// 目录树收集结果：文件 (本地路径, rel) 与空目录 rel（rel 含拖入目录根名，POSIX 分隔）
struct DirTree {
    root_name: String,
    files: Vec<(std::path::PathBuf, String)>,
    empty_dirs: Vec<String>,
    /// 因非法名称/超深被跳过的条目数
    skipped: usize,
}

/// 路径组件校验（与服务端净化语义对齐）：非法组件的条目跳过而非整树拒绝，
/// 避免一个坏名字导致整个目录上传失败
fn sanitize_component(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && !name.contains(['\\', '/'])
        && !name.chars().any(|c| c.is_control())
        && name.chars().count() <= 255
}

/// 根名之外的最大层级（服务端 rel_path 限 16 段，含根名）
const DIR_WALK_DEPTH_MAX: usize = 15;

/// 递归收集目录树；空目录（无任何子项）记入 empty_dirs 供 mkdir 还原
fn walk_dir_tree(dir: &std::path::Path, rel: &str, tree: &mut DirTree) -> anyhow::Result<()> {
    if rel.split('/').count() > DIR_WALK_DEPTH_MAX {
        tree.skipped += 1;
        return Ok(());
    }
    let entries = std::fs::read_dir(dir).with_context(|| format!("读取目录失败: {}", dir.display()))?;
    let mut children = 0usize;
    for entry in entries {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => {
                tree.skipped += 1;
                continue;
            }
        };
        let name = entry.file_name().to_string_lossy().into_owned();
        if !sanitize_component(&name) {
            tree.skipped += 1;
            continue;
        }
        let meta = match entry.metadata() {
            Ok(m) => m,
            Err(_) => {
                tree.skipped += 1;
                continue;
            }
        };
        let child_rel = format!("{rel}/{name}");
        if meta.is_dir() {
            children += 1;
            walk_dir_tree(&entry.path(), &child_rel, tree)?;
        } else if meta.is_file() {
            children += 1;
            tree.files.push((entry.path(), child_rel));
        } else {
            tree.skipped += 1; // 符号链接等非常规条目
        }
    }
    if children == 0 {
        tree.empty_dirs.push(rel.to_string());
    }
    Ok(())
}

/// 递归收集拖入目录的树（rel 以拖入目录的根名为第一层）
fn collect_dir_tree(root: &std::path::Path) -> anyhow::Result<DirTree> {
    let root_name = root
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    if !sanitize_component(&root_name) {
        anyhow::bail!("目录名含非法字符: {root_name:?}");
    }
    let mut tree = DirTree {
        root_name: root_name.clone(),
        files: Vec::new(),
        empty_dirs: Vec::new(),
        skipped: 0,
    };
    walk_dir_tree(root, &root_name, &mut tree)?;
    Ok(tree)
}

/// Windows 冲突询问弹窗：Some(true)=覆盖、Some(false)=重命名。其它平台无原生对话框
/// 机制 → None（调用方使用默认值"重命名"）。基于既有 windows-sys 依赖的 MessageBoxW，
/// 不引入新的对话框 crate
#[cfg(windows)]
fn ask_overwrite_dialog(rel_root: &str) -> Option<bool> {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        MessageBoxW, IDYES, MB_ICONQUESTION, MB_SETFOREGROUND, MB_YESNO,
    };
    let text = format!(
        "服务器上已存在目录「{rel_root}」。\n\n是 = 覆盖合并（同名文件覆盖，其余保留）\n否 = 重命名为新目录（原目录不动）"
    );
    let text_w: Vec<u16> = text.encode_utf16().chain([0]).collect();
    let title_w: Vec<u16> = "目录上传".encode_utf16().chain([0]).collect();
    let r = unsafe {
        MessageBoxW(
            std::ptr::null_mut(),
            text_w.as_ptr(),
            title_w.as_ptr(),
            MB_YESNO | MB_ICONQUESTION | MB_SETFOREGROUND,
        )
    };
    Some(r == IDYES)
}

/// 非 Windows：无统一原生对话框，默认重命名
#[cfg(not(windows))]
fn ask_overwrite_dialog(_rel_root: &str) -> Option<bool> {
    None
}

/// 目录上传执行：探测根目录 →（已存在时 Windows 弹窗选择覆盖/重命名，其它平台默认
/// 重命名）→ mkdir 还原空目录 → 逐文件上传（覆盖语义随用户选择）。
/// 返回远端目录绝对路径（服务端确认值，非从文件路径推导）
async fn upload_dir_tree(
    server_url: &str,
    token: &str,
    session: &str,
    cert: Option<&str>,
    tree: &DirTree,
) -> Option<String> {
    let root_rel = tree.root_name.clone();

    // 1. 探测根目录是否已存在
    let probe = match ws_dir_setup(server_url, token, session, &root_rel, true, false, cert).await {
        Ok(p) => p,
        Err(e) => {
            crate::dlog!("[upload] 目录 {root_rel} 探测失败: {e:#}");
            return None;
        }
    };
    // 2. 冲突处理：Windows 弹窗询问覆盖/重命名；其它平台默认重命名
    let (rel, overwrite, dir_path) = if probe.existed {
        match ask_overwrite_dialog(&root_rel) {
            Some(true) => {
                // 覆盖合并：沿用原目录，同名文件覆盖写
                (root_rel.clone(), true, probe.path.clone())
            }
            choice => {
                // 重命名：自动改名为 root_N 新建目录（内部无冲突）
                let created = match ws_dir_setup(server_url, token, session, &root_rel, false, true, cert).await {
                    Ok(c) => c,
                    Err(e) => {
                        crate::dlog!("[upload] 目录 {root_rel} 重命名创建失败: {e:#}");
                        return None;
                    }
                };
                crate::dlog!("[upload] 目录冲突（选择 {}），重命名为 {}", if choice == Some(false) { "重命名" } else { "默认重命名" }, created.actual_rel);
                (created.actual_rel.clone(), false, created.path.clone())
            }
        }
    } else {
        (root_rel.clone(), false, probe.path.clone())
    };

    // 3. mkdir 还原空目录（根目录改名时同步映射前缀）
    for d in &tree.empty_dirs {
        let d = remap_root(d, &tree.root_name, &rel);
        if let Err(e) = ws_dir_setup(server_url, token, session, &d, false, false, cert).await {
            crate::dlog!("[upload] 空目录 {d} 还原失败: {e:#}");
        }
    }

    // 4. 逐文件上传（rel 同步映射；覆盖语义随用户选择）。
    // 注意：rel_path 只含目录部分（不含文件名）——服务端落地 = cwd/rel_path/name，
    // 完整 rel 会多拼一层同名目录
    let mut ok = 0usize;
    let mut fail = 0usize;
    for (path, file_rel) in &tree.files {
        let frel = remap_root(file_rel, &tree.root_name, &rel);
        let fdir = match frel.rsplit_once('/') {
            Some((d, _)) => d,
            None => "", // 根目录直下的文件
        };
        let name = path
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        let size = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
        let res = match tokio::fs::File::open(path).await {
            Ok(mut file) => {
                ws_upload(server_url, token, &name, size, &mut file, session, fdir, overwrite, cert).await
            }
            Err(e) => Err(anyhow!("本地文件打开失败: {e}")),
        };
        match res {
            Ok(_) => ok += 1,
            Err(e) => {
                crate::dlog!("[upload] {frel} 上传失败: {e:#}");
                fail += 1;
            }
        }
    }
    crate::dlog!(
        "[upload] 目录 {} 上传完成: 成功 {ok} 失败 {fail} 跳过 {}",
        tree.root_name,
        tree.skipped
    );
    if ok == 0 && !tree.files.is_empty() {
        return None; // 全部文件失败视为整体失败（回退原文透传）
    }
    Some(dir_path)
}

/// rel 路径的根名映射：root 改名（root → root_N）时同步子路径前缀
fn remap_root(rel: &str, old_root: &str, new_root: &str) -> String {
    if old_root == new_root {
        return rel.to_string();
    }
    if let Some(rest) = rel.strip_prefix(old_root) {
        format!("{new_root}{rest}")
    } else {
        rel.to_string()
    }
}

/// raw mode 兜底恢复：正常返回路径经 Drop 关闭；process::exit 路径需在此之前手动关闭。
/// Windows 下额外持有 conhost 环境开启 VT 处理前的 stdout 模式，Drop 时一并恢复。
struct RawModeGuard {
    /// 非 WT 环境（conhost）开启 ENABLE_VIRTUAL_TERMINAL_PROCESSING 前的原模式；None = 未改动
    #[cfg(windows)]
    vt_mode: Option<u32>,
}

impl RawModeGuard {
    fn new() -> Self {
        #[cfg(windows)]
        {
            let vt_mode = match std::env::var_os("WT_SESSION") {
                Some(_) => None, // Windows Terminal 默认支持 VT 处理，非 conhost、PowerShell 等环境，无需开启 VT 处理
                None => {
                    crate::dlog!("[pty] WT 环境检测：未检测到 Windows Terminal 会话 WT_SESSION，尝试开启 VT 处理");
                    match enable_vt_processing() {
                        Some(mode) => {
                            crate::dlog!("[pty] WT 环境检测：VT 处理已开启，原控制台模式={mode}");
                            Some(mode)
                        }
                        None => {
                            crate::dlog!("[pty] WT 环境检测：VT 处理开启失败，或已默认开启");
                            None
                        }
                    }
                }
            };
            RawModeGuard { vt_mode }
        }
        #[cfg(not(windows))]
        RawModeGuard {}
    }
}

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        let _ = crossterm::terminal::disable_raw_mode();
        #[cfg(windows)]
        if let Some(mode) = self.vt_mode {
            restore_console_mode(mode);
        }
    }
}

/// 非 Windows Terminal（conhost 等）宿主下，stdout 的 VT 处理默认可能未开启，
/// 远端输出的 ANSI 序列会以 `ESC[...` 字面量形式显示为乱码。为 stdout 开启
/// ENABLE_VIRTUAL_TERMINAL_PROCESSING，返回原控制台模式供 Drop 恢复。
#[cfg(windows)]
fn enable_vt_processing() -> Option<u32> {
    use windows_sys::Win32::System::Console::{
        GetConsoleMode, GetStdHandle, SetConsoleMode, ENABLE_VIRTUAL_TERMINAL_PROCESSING,
        STD_OUTPUT_HANDLE,
    };
    unsafe {
        let h = GetStdHandle(STD_OUTPUT_HANDLE);
        let mut mode = 0u32;
        if GetConsoleMode(h, &mut mode) == 0 {
            return None;
        }
        if SetConsoleMode(h, mode | ENABLE_VIRTUAL_TERMINAL_PROCESSING) == 0 {
            return None;
        }
        Some(mode)
    }
}

/// 恢复 stdout 控制台模式（Drop 路径，失败只能静默）
#[cfg(windows)]
fn restore_console_mode(mode: u32) {
    use windows_sys::Win32::System::Console::{GetStdHandle, SetConsoleMode, STD_OUTPUT_HANDLE};
    unsafe {
        let h = GetStdHandle(STD_OUTPUT_HANDLE);
        let _ = SetConsoleMode(h, mode);
    }
}

/// 本地输入中的"终端应答序列"过滤器。
///
/// ConPTY 环境下，本地终端对远端查询的应答会以按键事件形式回流（含 ConPTY 自身
/// 启动握手 `ESC[6n` 的应答）。若原样转发给远端，会在查询方（tmux 等）放弃等待后
/// 到达，成为残留在输入缓冲的垃圾——表现为 shell 提示符后回显 `[?61;4;...c` 等。
/// 远端查询由服务端确定性兜底应答，因此转发前按状态机吞掉以下应答序列
/// （支持跨事件拆分重组）：
/// - CPR 应答 `ESC[<数字>;]R`
/// - DA 应答 `ESC[?<数字>;]c`
/// - OSC 应答 `ESC]...BEL` / `ESC]...ESC\`
struct ReplyFilter {
    pending: Vec<u8>,
    state: FilterState,
}

enum FilterState {
    /// 普通：字节直接放行（ESC 单独收集待定）
    Normal,
    /// 已见 ESC，待判定序列类型
    Esc,
    /// ESC[ CSI 收集中，直到终结字节
    Csi,
    /// 裸 `[` 收集中：ConPTY/crossterm 会吞掉应答序列的 ESC，残余以 `[` 开头逐键到达
    Naked,
    /// ESC] OSC 收集中，直到 BEL/ST
    Osc,
    /// OSC 内出现 ESC（可能是 ST 前半）
    OscEsc,
}

impl ReplyFilter {
    fn new() -> Self {
        ReplyFilter {
            pending: Vec::new(),
            state: FilterState::Normal,
        }
    }

    /// 喂入一批按键字节，返回应转发给远端的字节（应答序列被吞掉）
    fn feed(&mut self, bytes: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(bytes.len());
        for &b in bytes {
            match self.state {
                FilterState::Normal => {
                    if b == 0x1b {
                        self.pending.push(b);
                        self.state = FilterState::Esc;
                    } else if b == b'[' {
                        // 无 ESC 前缀的 `[`：可能是被吞掉 ESC 的应答残余，收集判定
                        self.pending.clear();
                        self.pending.push(b);
                        self.state = FilterState::Naked;
                    } else {
                        out.push(b);
                    }
                }
                FilterState::Naked => {
                    // 终结判定先于压入：pending[1..] 为已收集参数（不含当前字节）
                    if (b == b'R' || b == b'c' || b == b't' || b == b'y')
                        && self.pending.len() > 1
                        && is_reply_csi(b, &self.pending[1..])
                    {
                        // 应答形状（如 [10;1R / [?1;0c / [>0;10;1c / [4;700;1450t / [?2026;2$y 不经此处）
                        self.pending.clear();
                        self.state = FilterState::Normal;
                    } else if b.is_ascii_digit()
                        || matches!(b, b';' | b'?' | b'>' | b'<' | b'=' | b':' | b'$')
                    {
                        self.pending.push(b);
                        if self.pending.len() >= 64 {
                            // 异常超长（非完整应答）：放行防卡死（与 Csi 态上限一致）
                            out.append(&mut self.pending);
                            self.state = FilterState::Normal;
                        }
                    } else {
                        // 非参数字符（字母/[ 等，vim 的 [c/[d 等按键）：立即放行
                        out.append(&mut self.pending);
                        out.push(b);
                        self.state = FilterState::Normal;
                    }
                }
                FilterState::Esc => match b {
                    b'[' => {
                        self.pending.push(b);
                        self.state = FilterState::Csi;
                    }
                    b']' => {
                        self.pending.push(b);
                        self.state = FilterState::Osc;
                    }
                    _ => {
                        // 非查询/应答形状的转义序列（如 Alt+键）：整体放行
                        self.pending.push(b);
                        out.append(&mut self.pending);
                        self.state = FilterState::Normal;
                    }
                },
                FilterState::Csi => {
                    self.pending.push(b);
                    if (0x40..=0x7e).contains(&b) {
                        if self.is_reply_sequence() {
                            self.pending.clear(); // 吞掉应答
                        } else {
                            out.append(&mut self.pending);
                        }
                        self.state = FilterState::Normal;
                    } else if self.pending.len() >= 64 {
                        // 异常超长（非完整序列）：放行防卡死
                        out.append(&mut self.pending);
                        self.state = FilterState::Normal;
                    }
                }
                FilterState::Osc => match b {
                    0x07 => {
                        // BEL 终结：OSC 应答吞掉
                        self.pending.clear();
                        self.state = FilterState::Normal;
                    }
                    0x1b => {
                        self.pending.push(b);
                        self.state = FilterState::OscEsc;
                    }
                    _ => {
                        self.pending.push(b);
                        if self.pending.len() >= 256 {
                            out.append(&mut self.pending);
                            self.state = FilterState::Normal;
                        }
                    }
                },
                FilterState::OscEsc => {
                    if b == b'\\' {
                        // ST 终结：OSC 应答吞掉
                        self.pending.clear();
                    } else {
                        // 非法 OSC 终结：放行已收集内容
                        self.pending.push(b);
                        out.append(&mut self.pending);
                    }
                    self.state = FilterState::Normal;
                }
            }
        }
        out
    }

    /// 取出滞留的判定字节并复位到普通状态（空闲超时冲刷用）：
    /// 孤立的裸 `[` / ESC 不应无限期滞留到下一次输入才放行
    fn take_pending(&mut self) -> Vec<u8> {
        self.state = FilterState::Normal;
        std::mem::take(&mut self.pending)
    }

    /// 判定已收集的完整 CSI 序列是否为终端应答（pending = ESC [ params... final）。
    /// 保险网：查询已在服务端输出流被丢弃，此处覆盖兜底漏网时的宿主终端应答形态。
    fn is_reply_sequence(&self) -> bool {
        let final_byte = *self.pending.last().unwrap();
        let params = &self.pending[2..self.pending.len() - 1];
        is_reply_csi(final_byte, params)
    }
}

/// 终端应答 CSI 形状判定（final + 参数）：CPR(R)/DA(c)/XTWINOPS 尺寸报告(t)/DECRQM 应答($y)
fn is_reply_csi(final_byte: u8, params: &[u8]) -> bool {
    if params.is_empty() {
        return false;
    }
    match final_byte {
        b'R' | b't' => params.iter().all(|&c| c.is_ascii_digit() || c == b';'),
        b'c' => params.iter().all(|&c| {
            c.is_ascii_digit() || matches!(c, b';' | b'?' | b'>' | b'<' | b'=' | b':')
        }),
        b'y' => params.iter().all(|&c| {
            c.is_ascii_digit() || matches!(c, b';' | b'?' | b'$')
        }),
        _ => false,
    }
}

/// 按键事件 → 终端字节序列（xterm 风格转义）
fn key_to_bytes(key: crossterm::event::KeyEvent) -> Option<Vec<u8>> {
    use crossterm::event::{KeyCode, KeyModifiers};
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    // xterm 修饰符参数：1 + Shift(1) + Alt(2) + Ctrl(4)
    let xterm_mod = 1 + (alt as u8) * 2 + (ctrl as u8) * 4;
    match key.code {
        KeyCode::Char(c) => {
            let mut buf = [0u8; 4];
            let s = c.encode_utf8(&mut buf);
            let mut out = Vec::new();
            if alt {
                out.push(0x1b);
            }
            if ctrl {
                if c == ' ' {
                    out.push(0x00); // Ctrl+Space → NUL
                } else {
                    let b = s.as_bytes()[0];
                    if b.is_ascii_alphabetic() || (b'@'..=b'_').contains(&b) {
                        out.push(b & 0x1f); // Ctrl+字母/@/[/\ ]/^/_ → 控制字符
                    } else {
                        out.extend_from_slice(s.as_bytes());
                    }
                }
            } else {
                out.extend_from_slice(s.as_bytes());
            }
            Some(out)
        }
        KeyCode::Enter => Some(b"\r".to_vec()),
        KeyCode::Backspace => Some(if alt { b"\x1b\x7f".to_vec() } else { vec![0x7f] }),
        KeyCode::Tab => Some(b"\t".to_vec()),
        KeyCode::BackTab => Some(b"\x1b[Z".to_vec()),
        KeyCode::Esc => Some(b"\x1b".to_vec()),
        KeyCode::Left => Some(csi_mod_seq(b"D", xterm_mod)),
        KeyCode::Right => Some(csi_mod_seq(b"C", xterm_mod)),
        KeyCode::Up => Some(csi_mod_seq(b"A", xterm_mod)),
        KeyCode::Down => Some(csi_mod_seq(b"B", xterm_mod)),
        KeyCode::Home => Some(b"\x1b[H".to_vec()),
        KeyCode::End => Some(b"\x1b[F".to_vec()),
        KeyCode::PageUp => Some(b"\x1b[5~".to_vec()),
        KeyCode::PageDown => Some(b"\x1b[6~".to_vec()),
        KeyCode::Insert => Some(b"\x1b[2~".to_vec()),
        KeyCode::Delete => Some(b"\x1b[3~".to_vec()),
        KeyCode::F(n) => Some(match n {
            1 => b"\x1bOP".to_vec(),
            2 => b"\x1bOQ".to_vec(),
            3 => b"\x1bOR".to_vec(),
            4 => b"\x1bOS".to_vec(),
            5 => b"\x1b[15~".to_vec(),
            6 => b"\x1b[17~".to_vec(),
            7 => b"\x1b[18~".to_vec(),
            8 => b"\x1b[19~".to_vec(),
            9 => b"\x1b[20~".to_vec(),
            10 => b"\x1b[21~".to_vec(),
            11 => b"\x1b[23~".to_vec(),
            12 => b"\x1b[24~".to_vec(),
            _ => return None,
        }),
        _ => None,
    }
}

/// 方向键序列：无修饰为经典 `\x1b[A` 形态；带修饰附加 xterm 参数（如 Ctrl+Up → `\x1b[1;5A`、Alt+Left → `\x1b[1;3D`）
fn csi_mod_seq(final_byte: &[u8], modifier: u8) -> Vec<u8> {
    if modifier == 1 {
        let mut out = vec![0x1b, b'['];
        out.extend_from_slice(final_byte);
        out
    } else {
        let mut out = format!("\x1b[1;{modifier}").into_bytes();
        out.extend_from_slice(final_byte);
        out
    }
}

/// 建立执行 WS 会话，凭证失效时自动恢复：
/// 401（凭证被拒）时凭指纹重新注册一次并重试。覆盖"管理员恢复凭证（token 轮换）
/// 后客户端旧 token 失效"的场景——记录仍为 Approved 时指纹注册直接领取新 token；
/// 记录已被吊销/拒绝时注册只会重置为 Pending（不发 token），重试仍失败并返回原
/// 错误，审批不会被绕过。
pub async fn ws_exec_recovered(
    server: &ServerReg,
    program: &str,
    args: &[String],
    tty: bool,
    tty_size: Option<(u16, u16)>,
) -> Result<ExecWs> {
    async fn connect(
        server: &ServerReg,
        token: &str,
        program: &str,
        args: &[String],
        tty: bool,
        tty_size: Option<(u16, u16)>,
    ) -> Result<ExecWs> {
        ws_exec(
            &server.server_url,
            token,
            program,
            args,
            None,
            tty,
            tty_size,
            server.cert_sha256.as_deref(),
        )
        .await
    }
    match connect(server, server.token.as_deref().unwrap_or_default(), program, args, tty, tty_size).await
    {
        Ok(v) => Ok(v),
        Err(e) if is_auth_failure(&e) => {
            crate::dlog!("[ws] 凭证被拒（401），尝试凭指纹自动重新注册");
            let Some(new_token) = recover_token(&server.server_url).await else {
                return Err(e); // 未领到新 token（如被重置为 Pending），维持原错误
            };
            crate::dlog!("[ws] 重新注册成功，已领取轮换后的 token，重试连接");
            connect(server, &new_token, program, args, tty, tty_size).await
        }
        Err(e) => Err(e),
    }
}

/// 判定连接失败是否为凭证失效（服务端 401 / WS 握手 rejected 中的凭证原因）
fn is_auth_failure(e: &anyhow::Error) -> bool {
    let msg = e.to_string();
    msg.contains("401") || msg.contains("凭证")
}

/// 凭指纹重新注册：服务端记录仍为 Approved 时领取轮换后的新 token，
/// 写回本地配置并返回；其余情况（Pending/被拒/网络失败）返回 None。
async fn recover_token(server_url: &str) -> Option<String> {
    let info = MachineInfo::collect();
    let resp = register(server_url, &info).await.ok()?;
    let token = resp["token"].as_str()?.to_string();
    let mut config = ClientConfig::load().ok()?;
    let existing = config.servers.iter_mut().find(|s| s.server_url == server_url)?;
    existing.token = Some(token.clone());
    existing.registered = true;
    if let Some(h) = resp["server_hostname"].as_str()
        && !h.is_empty()
    {
        existing.hostname = h.to_string();
    }
    config.save().ok()?;
    Some(token)
}

pub fn resolve_exec_target(alias: &str, args: &[String]) -> Result<(ServerReg, serde_json::Value)> {
    let aliases = load_aliases()?;
    let Some(def) = aliases.get(alias) else {
        return Err(anyhow!("别名不存在: {alias}"));
    };
    let cmdline = render_template(&def.template, args);
    let Some((program, rest)) = super::config::parse_command_line(&cmdline) else {
        return Err(anyhow!("命令模板无效: {cmdline}"));
    };

    let config = ClientConfig::load()?;
    let server = match def.server.as_deref().filter(|s| !s.is_empty()) {
        Some(url) => config
            .find_server(url)
            .filter(|s| s.registered && s.token.is_some())
            .cloned()
            .ok_or_else(|| anyhow!("别名绑定的服务器不可用: {url}，请重新注册或修改别名"))?,
        None => config
            .default_server()
            .cloned()
            .ok_or_else(|| anyhow!("尚未注册任何服务器，请先完成注册"))?,
    };
    Ok((server, json!({ "program": program, "args": rest })))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 路径串解析：空格分隔、引号包裹含空格路径、引内 "" 字面引号、反斜杠转义
    #[test]
    fn parse_paths_variants() {
        assert_eq!(parse_paths("C:\\a.txt"), vec!["C:\\a.txt"]);
        assert_eq!(parse_paths("\"C:\\a b\\c.txt\""), vec!["C:\\a b\\c.txt"]);
        assert_eq!(
            parse_paths("\"C:\\a b.txt\" C:\\c.txt /tmp/x"),
            vec!["C:\\a b.txt", "C:\\c.txt", "/tmp/x"]
        );
        // 引内 "" → 字面引号（含双引号的 Unix 路径）
        assert_eq!(parse_paths("\"/tmp/a\"\"b.txt\""), vec!["/tmp/a\"b.txt"]);
        // 引内反斜杠不影响（非紧邻引号）
        assert_eq!(parse_paths("\"C:\\a\\b.txt\""), vec!["C:\\a\\b.txt"]);
        // 多空白分隔
        assert_eq!(parse_paths("  spaced   out\t "), vec!["spaced", "out"]);
        // 引内空格保留
        assert_eq!(parse_paths("\"  lead\""), vec!["  lead"]);
    }

    /// 上传判定：全部 token 存在（文件或目录）才触发；不存在的路径按普通输入透传
    #[test]
    fn upload_paths_requires_all_regular_files() {
        let dir = std::env::temp_dir().join(format!("rc-upload-paths-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let a = dir.join("a.txt");
        std::fs::write(&a, "x").unwrap();

        // 单个存在的文件 → 触发
        assert_eq!(
            upload_paths(a.to_string_lossy().as_bytes()).map(|v| v.len()),
            Some(1)
        );
        // 目录 → 触发（目录上传）
        assert_eq!(
            upload_paths(dir.to_string_lossy().as_bytes()).map(|v| v.len()),
            Some(1)
        );
        // 不存在的路径 → 不触发
        assert!(upload_paths(b"definitely-not-exist-xyz").is_none());
        // 普通命令文本 → 不触发
        assert!(upload_paths(b"ls -la").is_none());
        // 多路径其一不存在 → 不触发
        let mixed = format!("{} nope-xyz", a.display());
        assert!(upload_paths(mixed.as_bytes()).is_none());
        // 引号包裹的含空格路径 → 触发
        let spaced = dir.join("spaced file.txt");
        std::fs::write(&spaced, "x").unwrap();
        let quoted = format!("\"{}\"", spaced.display());
        assert_eq!(
            upload_paths(quoted.as_bytes()).map(|v| v.len()),
            Some(1)
        );
        // 非法 UTF-8 → 不触发
        assert!(upload_paths(&[0xff, 0xfe]).is_none());

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 目录树收集：层级还原（rel 含根名）、空目录识别、非法组件跳过
    #[test]
    fn collect_dir_tree_walks_and_flags_empty_dirs() {
        let root = std::env::temp_dir().join(format!("rc-dirtree-{}", std::process::id()));
        let sub = root.join("sub");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::create_dir_all(root.join("empty")).unwrap();
        std::fs::write(sub.join("a.txt"), "x").unwrap();
        std::fs::write(root.join("b.txt"), "y").unwrap();

        let tree = collect_dir_tree(&root).unwrap();
        assert_eq!(tree.root_name, format!("rc-dirtree-{}", std::process::id()));
        let rels: Vec<&str> = tree.files.iter().map(|(_, r)| r.as_str()).collect();
        let root_name = tree.root_name.clone();
        assert!(rels.contains(&format!("{root_name}/sub/a.txt").as_str()), "rels = {rels:?}");
        assert!(rels.contains(&format!("{root_name}/b.txt").as_str()), "rels = {rels:?}");
        assert_eq!(tree.empty_dirs, vec![format!("{root_name}/empty")]);
        assert_eq!(tree.skipped, 0);

        // 非法组件校验（Windows 文件系统本身不允许控制字符名，此处直接验证组件规则）
        assert!(!sanitize_component(".."));
        assert!(!sanitize_component(".."));
        assert!(!sanitize_component("a/b"));
        assert!(sanitize_component("ok.txt"));
        // 根名非法 → 整体拒绝
        assert!(collect_dir_tree(std::path::Path::new(".")).is_err()); // 根名 "." 非法

        let _ = std::fs::remove_dir_all(&root);
    }

    /// upload_paths 端到端：无引号空格路径整串当单文件查
    #[test]
    fn upload_paths_supports_unquoted_spaced_paths() {
        let dir = std::env::temp_dir().join(format!("rc-up-unquoted-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let spaced = dir.join("x y.txt");
        std::fs::write(&spaced, "x").unwrap();

        // 无引号单路径（含空格）→ 整串命中
        assert_eq!(
            upload_paths(spaced.to_string_lossy().as_bytes()).map(|v| v.len()),
            Some(1)
        );
        // 命令 + 存在路径 → 整串不是文件，不触发（原文透传）
        let cmd = format!("cat {}", spaced.display());
        assert!(upload_paths(cmd.as_bytes()).is_none());
        // 两个无引号路径（均不含空格；含空格多路径只支持引号包裹形式）→ 逐目标上传
        let plain = dir.join("p1.txt");
        std::fs::write(&plain, "x").unwrap();
        let plain2 = dir.join("p2.txt");
        std::fs::write(&plain2, "x").unwrap();
        let two = format!("{} {}", plain.display(), plain2.display());
        assert_eq!(upload_paths(two.as_bytes()).map(|v| v.len()), Some(2));
        // 引号包裹 → 引号语义直接命中
        let quoted = format!("\"{}\"", spaced.display());
        assert_eq!(upload_paths(quoted.as_bytes()).map(|v| v.len()), Some(1));

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 远端路径注入 shell 的引用规则：单引号字面量，`$`/反引号不展开，内部 `'` 转义
    #[test]
    fn quote_for_shell_rules() {
        assert_eq!(quote_for_shell("/up/a.txt"), "'/up/a.txt'");
        assert_eq!(quote_for_shell("/up/a b.txt"), "'/up/a b.txt'");
        assert_eq!(quote_for_shell("/up/a\"b.txt"), "'/up/a\"b.txt'");
        // 命令替换/变量展开在单引号内不生效（防恶意文件名注入）
        assert_eq!(quote_for_shell("/up/$(reboot).txt"), "'/up/$(reboot).txt'");
        assert_eq!(quote_for_shell("/up/`id`.txt"), "'/up/`id`.txt'");
        assert_eq!(quote_for_shell("/up/$HOME.txt"), "'/up/$HOME.txt'");
        // 内部单引号以 '\'' 转义
        assert_eq!(quote_for_shell("/up/a'b.txt"), "'/up/a'\\''b.txt'");
    }

    /// 无 ESC 前缀的应答残余（ConPTY/crossterm 吞掉 ESC 后逐键到达）应被吞掉
    #[test]
    fn reply_filter_swallows_naked_reply_remainder() {
        let mut f = ReplyFilter::new();
        // 逐字节喂入（模拟 crossterm 逐键转发）
        for b in b"[10;1R" {
            assert_eq!(f.feed(&[*b]), b"");
        }
        let mut f = ReplyFilter::new();
        assert_eq!(f.feed(b"[?1;0c"), b"");
        let mut f = ReplyFilter::new();
        assert_eq!(f.feed(b"[>0;10;1c"), b"");
        // 混合：应答后接普通输入
        let mut f = ReplyFilter::new();
        assert_eq!(f.feed(b"[10;1Rls"), b"ls");
    }

    /// 长应答（无 ESC 前缀、参数 ≥32 字节的实测 WT DA1 形态）应被完整吞掉
    #[test]
    fn reply_filter_swallows_long_naked_reply() {
        let mut f = ReplyFilter::new();
        // 实测乱码本体：参数 "?61;4;6;7;14;21;22;23;24;28;32;42;52" 达 36 字节
        for b in b"[?61;4;6;7;14;21;22;23;24;28;32;42;52c" {
            assert_eq!(f.feed(&[*b]), b"");
        }
    }

    /// vim 的 [ 类按键（[c/[d/[Z）不应被误吞
    #[test]
    fn reply_filter_passes_vim_bracket_keys() {
        let mut f = ReplyFilter::new();
        assert_eq!(f.feed(b"[c"), b"[c"); // 空参数放行
        let mut f = ReplyFilter::new();
        assert_eq!(f.feed(b"[d"), b"[d");
        let mut f = ReplyFilter::new();
        assert_eq!(f.feed(b"[Z"), b"[Z");
    }

    /// CPR 应答应被吞掉（单块与跨块拆分）
    #[test]
    fn reply_filter_swallows_cpr_reply() {
        let mut f = ReplyFilter::new();
        assert_eq!(f.feed(b"\x1b[1;1R"), b"");
        let mut f = ReplyFilter::new();
        assert_eq!(f.feed(b"\x1b"), b""); // ESC 单独到达
        assert_eq!(f.feed(b"[1;"), b"");
        assert_eq!(f.feed(b"1R"), b"");
    }

    /// WT 风格 DA1 应答（截图中的乱码本体）应被吞掉
    #[test]
    fn reply_filter_swallows_da_reply() {
        let mut f = ReplyFilter::new();
        let da = b"\x1b[?61;4;6;7;14;21;22;23;24;28;32;42;52c";
        assert_eq!(f.feed(da), b"");
    }

    /// DA2 应答（参数含 >）应被吞掉
    #[test]
    fn reply_filter_swallows_da2_reply() {
        let mut f = ReplyFilter::new();
        assert_eq!(f.feed(b"\x1b[>0;10;1c"), b"");
        let mut f = ReplyFilter::new();
        assert_eq!(f.feed(b"\x1b[>c"), b""); // 部分终端的 DA2 应答
    }

    /// OSC 应答（BEL / ST 终结）应被吞掉
    #[test]
    fn reply_filter_swallows_osc_reply() {
        let mut f = ReplyFilter::new();
        assert_eq!(f.feed(b"\x1b]11;rgb:1c1c/1c1c/1c1c\x07"), b"");
        let mut f = ReplyFilter::new();
        assert_eq!(f.feed(b"\x1b]11;rgb:0000/0000/0000\x1b\\"), b"");
    }

    /// 普通按键与非应答转义序列应原样放行
    #[test]
    fn reply_filter_passes_normal_input() {
        let mut f = ReplyFilter::new();
        assert_eq!(f.feed(b"ls -la\r"), b"ls -la\r");
        assert_eq!(f.feed(b"\x1b[A"), b"\x1b[A"); // 方向键
        assert_eq!(f.feed(b"\x1b[Z"), b"\x1b[Z"); // BackTab
        assert_eq!(f.feed(b"\x1b[Z\x1b[1;1R"), b"\x1b[Z"); // 混合
        assert_eq!(f.feed(b"\x1b[1;1Rabc"), b"abc"); // 应答后接普通输入
        assert_eq!(f.feed(b"\x1bo"), b"\x1bo"); // ESC+非序列字符（Alt+o）
        assert_eq!(f.feed(b"\x1b"), b""); // 悬空 ESC 留待下批
    }

    /// 空闲冲刷：滞留判定字节（裸 [ / 悬空 ESC）取出后状态复位，后续输入正常分类
    #[test]
    fn reply_filter_take_pending_flushes_and_resets() {
        // 裸 [ 滞留
        let mut f = ReplyFilter::new();
        assert_eq!(f.feed(b"["), b"");
        assert_eq!(f.take_pending(), b"[");
        // 复位后普通输入不受影响
        assert_eq!(f.feed(b"abc"), b"abc");
        // 悬空 ESC 滞留
        let mut f = ReplyFilter::new();
        assert_eq!(f.feed(b"\x1b"), b"");
        assert_eq!(f.take_pending(), b"\x1b");
        assert_eq!(f.feed(b"\x1b[A"), b"\x1b[A"); // 复位后方向键仍完整放行
        // 无滞留时冲刷返回空
        let mut f = ReplyFilter::new();
        assert_eq!(f.take_pending(), b"");
    }

    /// 端到端验证别名模板渲染：REMOTE_CMD_HOME 隔离 + 写入别名与服务器配置
    #[test]
    fn test_resolve_exec_target_renders_template() {
        let home = std::env::temp_dir().join(format!("remote-cmd-test-{}", std::process::id()));
        let client_dir = home.join("client");
        std::fs::create_dir_all(&client_dir).unwrap();
        // SAFETY: 测试进程独占环境变量；其它测试不依赖 REMOTE_CMD_HOME
        unsafe { std::env::set_var("REMOTE_CMD_HOME", &home) };

        std::fs::write(
            client_dir.join("aliases.json"),
            r#"{"deploy":{"template":"git add {args}"}}"#,
        )
        .unwrap();
        std::fs::write(
            client_dir.join("client.json"),
            r#"{"servers":[{"server_url":"https://example.com","client_id":"c1","hostname":"h1","token":"tk","registered":true}]}"#,
        )
        .unwrap();

        let (_, payload) = resolve_exec_target("deploy", &["-A".to_string()]).unwrap();
        println!("payload = {payload}");
        assert_eq!(payload["program"], "git");
        assert_eq!(payload["args"][0], "add");
        assert_eq!(payload["args"][1], "-A");

        // 无参数场景
        let (_, payload) = resolve_exec_target("deploy", &[]).unwrap();
        println!("payload = {payload}");
        assert_eq!(payload["program"], "git");
        assert_eq!(payload["args"][0], "add");
        assert_eq!(payload["args"].as_array().unwrap().len(), 1);

        // 清理
        unsafe { std::env::remove_var("REMOTE_CMD_HOME") };
        let _ = std::fs::remove_dir_all(&home);
    }
}
