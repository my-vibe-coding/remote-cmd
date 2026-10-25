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
    let ExecWs { sink, stream } = match ws_exec(
        &server.server_url,
        server.token.as_deref().unwrap_or_default(),
        &program,
        &cmd_args,
        None,
        pty,
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

    // 同步初始终端尺寸（服务端默认 80x24）
    if let Ok((cols, rows)) = crossterm::terminal::size() {
        let _ = sink.send(resize_msg(cols, rows)).await;
    }

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

    // 输入转发任务：本地事件 → WS 帧；Detach 时通知主循环断开
    let (detach_tx, mut detach_rx) = mpsc::channel::<()>(1);
    let forward = tokio::spawn(async move {
        while let Some(a) = irx.recv().await {
            let msg = match a {
                InputAction::Data(d) => WsMessage::Binary(d.into()),
                InputAction::Resize(c, r) => resize_msg(c, r),
                InputAction::Detach => {
                    let _ = detach_tx.send(()).await;
                    break;
                }
            };
            if sink.send(msg).await.is_err() {
                break;
            }
        }
    });

    // 输出泵：PTY 输出为原始 ANSI 字节，直接写本地终端
    let mut exit_code: Option<i32> = None;
    let mut detached = false;
    loop {
        tokio::select! {
            maybe = stream.next() => match maybe {
                Some(Ok(frame)) => match parse_frame(frame) {
                    Some(ExecFrame::Output { data, .. }) => {
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

/// raw mode 兜底恢复：正常返回路径经 Drop 关闭；process::exit 路径需在此之前手动 disable
struct RawModeGuard;
impl Drop for RawModeGuard {
    fn drop(&mut self) {
        let _ = crossterm::terminal::disable_raw_mode();
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
