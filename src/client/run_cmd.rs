use std::io::{IsTerminal, Read as _, Write as _};

use anyhow::{anyhow, Result};
use futures::{SinkExt, StreamExt};
use serde_json::json;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message as WsMessage;

use super::config::{load_aliases, render_template, ClientConfig, ServerReg};
use super::remote::{parse_frame, resize_msg, ws_exec, ExecFrame, ExecWs, STDIN_CLOSE};

/// `remote-cmd run <别名> [args...]`：读取本地配置 → 模板渲染 → WS 会话 → stdin 转发 + 流式输出 → 透传退出码
pub async fn run(alias: &str, args: &[String]) -> Result<()> {
    let (server, body) = resolve_exec_target(alias, args)?;
    let program = body["program"].as_str().unwrap_or_default().to_string();
    let cmd_args: Vec<String> = serde_json::from_value(body["args"].clone())?;

    // 本地 stdin 为终端 → PTY 附加会话（raw mode + resize 同步，无超时）；
    // 管道 stdin → 字节流转发 + 流式输出（服务端默认超时）
    let pty = std::io::stdin().is_terminal();
    // PTY 会话：本地尺寸随 init 帧上报（服务端按此创建 PTY，避免 80x24 初始竞态）
    let tty_size = if pty { crossterm::terminal::size().ok() } else { None };
    let ExecWs { sink, stream } = match ws_exec(
        &server.server_url,
        server.token.as_deref().unwrap_or_default(),
        &program,
        &cmd_args,
        None,
        pty,
        tty_size,
    )
    .await
    {
        Ok(w) => w,
        Err(e) => anyhow::bail!("请求服务器失败: {e}"),
    };

    if pty {
        return run_pty_session(sink, stream).await;
    }

    // 管道 stdin → 转发原始字节，EOF 时发 stdin_close
    let stdin_task = spawn_stdin_pump(sink);

    // 输出泵：Binary 按 tag 分流；Exit 取退出码
    let mut exit_code: Option<i32> = None;
    let mut stream = stream;
    while let Some(msg) = stream.next().await {
        match msg {
            Ok(frame) => match parse_frame(frame) {
                Some(ExecFrame::Output { stream, data }) => {
                    if stream == "stderr" {
                        let mut err = std::io::stderr();
                        let _ = err.write_all(&data);
                        let _ = err.flush();
                    } else {
                        let mut out = std::io::stdout();
                        let _ = out.write_all(&data);
                        let _ = out.flush();
                    }
                }
                Some(ExecFrame::Exit { exit_code: code, timeout, .. }) => {
                    if timeout {
                        eprintln!("[remote-cmd] 远程命令执行超时");
                    }
                    exit_code = code;
                    break;
                }
                None => {}
            },
            Err(e) => {
                eprintln!("[remote-cmd] 连接中断: {e}");
                break;
            }
        }
    }

    stdin_task.abort();

    match exit_code {
        Some(code) => std::process::exit(code),
        None => {
            // 进程被 kill / 启动失败 / 连接中断
            eprintln!("[remote-cmd] 远程命令异常退出");
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
        while let Some(data) = rx.recv().await {
            if sink.send(WsMessage::Binary(data.into())).await.is_err() {
                return;
            }
        }
        // 本地 EOF → 通知服务端关闭子进程 stdin
        let _ = sink.send(WsMessage::Text(STDIN_CLOSE.into())).await;
        let _ = reader.await;
    })
}

/// PTY 交互会话：本地终端 raw mode，按键即时转发、resize 同步、ANSI 原样渲染。
/// Ctrl+] 脱离会话（断开连接，远端子进程被服务端终止）。
async fn run_pty_session(
    mut sink: futures::stream::SplitSink<super::remote::WsStream, WsMessage>,
    mut stream: futures::stream::SplitStream<super::remote::WsStream>,
) -> Result<()> {
    use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers};

    crossterm::terminal::enable_raw_mode().map_err(|e| anyhow!("进入 raw mode 失败: {e}"))?;
    let _raw = RawModeGuard; // Drop 兜底恢复（显式退出路径已在 exit 前手动关闭）

    enum InputAction {
        Data(Vec<u8>),
        Resize(u16, u16),
        Detach,
    }

    // 本地输入线程：阻塞读控制台事件 → 通道
    let (itx, mut irx) = mpsc::channel::<InputAction>(64);
    let _input_thread = std::thread::spawn(move || {
        loop {
            match crossterm::event::read() {
                Ok(Event::Key(k)) if k.kind == KeyEventKind::Press => {
                    if k.modifiers.contains(KeyModifiers::CONTROL) && k.code == KeyCode::Char(']') {
                        let _ = itx.blocking_send(InputAction::Detach);
                        break;
                    }
                    if let Some(bytes) = key_to_bytes(k) {
                        if itx.blocking_send(InputAction::Data(bytes)).is_err() {
                            break;
                        }
                    }
                }
                Ok(Event::Resize(cols, rows)) => {
                    let _ = itx.blocking_send(InputAction::Resize(cols, rows));
                }
                Ok(_) => {}
                Err(_) => break,
            }
        }
    });

    // 输入转发任务：本地事件 → WS 帧（经 ReplyFilter 吞掉终端应答序列）；Detach 时通知主循环断开
    let (detach_tx, mut detach_rx) = mpsc::channel::<()>(1);
    let mut reply_filter = ReplyFilter::new();
    let forward = tokio::spawn(async move {
        while let Some(a) = irx.recv().await {
            let msg = match a {
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
                InputAction::Resize(c, r) => {
                    crate::dlog!("[pty-in] 客户端上报终端尺寸 {}x{}", c, r);
                    resize_msg(c, r)
                }
                InputAction::Detach => {
                    crate::dlog!("[pty-in] 客户端脱离会话（Ctrl+]）");
                    let _ = detach_tx.send(()).await;
                    break;
                }
            };
            if sink.send(msg).await.is_err() {
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
    loop {
        tokio::select! {
            maybe = stream.next() => match maybe {
                Some(Ok(frame)) => match parse_frame(frame) {
                    Some(ExecFrame::Output { data, .. }) => {
                        if crate::common::looks_like_query(&data) {
                            crate::dlog!(
                                "[pty-out] 客户端收到远端查询 {} 字节: {}",
                                data.len(),
                                crate::common::escape_bytes(&data)
                            );
                        }
                        let mut out = std::io::stdout();
                        let _ = out.write_all(&data);
                        let _ = out.flush();
                    }
                    Some(ExecFrame::Exit { exit_code: code, .. }) => {
                        exit_code = code;
                        break;
                    }
                    None => {}
                },
                Some(Err(e)) => {
                    eprintln!("[remote-cmd] 连接中断: {e}");
                    break;
                }
                None => break,
            },
            _ = detach_rx.recv() => {
                detached = true;
                break;
            }
        }
    }

    forward.abort();
    let _ = crossterm::terminal::disable_raw_mode();
    drop(_raw); // 避免 Drop 重复 disable（disable 幂等，仅语义清晰）

    if detached {
        eprintln!("[remote-cmd] 已脱离会话，远端进程将被终止");
        std::process::exit(0); // 不经 Drop（已手动恢复终端），立即断开
    }

    match exit_code {
        Some(code) => std::process::exit(code),
        None => {
            eprintln!("[remote-cmd] 远程命令异常退出");
            std::process::exit(1);
        }
    }
}

/// raw mode 兜底恢复：正常返回路径经 Drop 关闭；process::exit 路径需在此之前手动关闭
struct RawModeGuard;
impl Drop for RawModeGuard {
    fn drop(&mut self) {
        let _ = crossterm::terminal::disable_raw_mode();
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
                    if (b == b'R' || b == b'c')
                        && self.pending.len() > 1
                        && self.pending[1..].iter().all(|&c| {
                            c.is_ascii_digit() || matches!(c, b';' | b'?' | b'>' | b'<' | b'=' | b':')
                        })
                    {
                        // 应答形状（如 [10;1R / [?1;0c / [>0;10;1c）：吞掉
                        self.pending.clear();
                        self.state = FilterState::Normal;
                    } else if b.is_ascii_digit()
                        || matches!(b, b';' | b'?' | b'>' | b'<' | b'=' | b':')
                    {
                        self.pending.push(b);
                        if self.pending.len() >= 32 {
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

    /// 判定已收集的完整 CSI 序列是否为终端应答（pending = ESC [ params... final）
    fn is_reply_sequence(&self) -> bool {
        let final_byte = *self.pending.last().unwrap();
        let params = &self.pending[2..self.pending.len() - 1];
        let params_valid = params
            .iter()
            .all(|&b| b.is_ascii_digit() || matches!(b, b';' | b'?' | b'>' | b'<' | b'=' | b':'));
        match final_byte {
            // CPR 应答（ESC[1;1R）
            b'R' => params_valid,
            // DA 应答（ESC[?61;...c / DA2: ESC[>0;10;1c）
            b'c' => params_valid,
            _ => false,
        }
    }
}

/// 按键事件 → 终端字节序列（xterm 风格转义）
fn key_to_bytes(key: crossterm::event::KeyEvent) -> Option<Vec<u8>> {
    use crossterm::event::{KeyCode, KeyModifiers};
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    match key.code {
        KeyCode::Char(c) => {
            let mut buf = [0u8; 4];
            let s = c.encode_utf8(&mut buf);
            let mut out = Vec::new();
            if alt {
                out.push(0x1b);
            }
            if ctrl {
                let b = s.as_bytes()[0];
                if b.is_ascii_alphabetic() || (b'@'..=b'_').contains(&b) {
                    out.push(b & 0x1f); // Ctrl+字母/@/[/\ ]/^/_ → 控制字符
                } else {
                    out.extend_from_slice(s.as_bytes());
                }
            } else {
                out.extend_from_slice(s.as_bytes());
            }
            Some(out)
        }
        KeyCode::Enter => Some(b"\r".to_vec()),
        KeyCode::Backspace => Some(vec![0x7f]),
        KeyCode::Tab => Some(b"\t".to_vec()),
        KeyCode::BackTab => Some(b"\x1b[Z".to_vec()),
        KeyCode::Esc => Some(b"\x1b".to_vec()),
        KeyCode::Left => Some(b"\x1b[D".to_vec()),
        KeyCode::Right => Some(b"\x1b[C".to_vec()),
        KeyCode::Up => Some(b"\x1b[A".to_vec()),
        KeyCode::Down => Some(b"\x1b[B".to_vec()),
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

/// 解析别名的执行目标：别名绑定的服务器优先，否则使用默认（第一个已注册）服务器。
/// 返回 (目标服务器, exec 请求体)
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
