use std::io::{IsTerminal, Read as _, Write as _};

use anyhow::{anyhow, Result};
use futures::{SinkExt, StreamExt};
use serde_json::json;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message as WsMessage;

use super::config::{load_aliases, render_template, ClientConfig, ServerReg};
use super::remote::{parse_frame, ws_exec, ExecFrame, ExecWs, STDIN_CLOSE};

/// `remote-cmd run <别名> [args...]`：读取本地配置 → 模板渲染 → WS 会话 → stdin 转发 + 流式输出 → 透传退出码
pub async fn run(alias: &str, args: &[String], interactive: bool) -> Result<()> {
    let (server, body) = resolve_exec_target(alias, args)?;
    let program = body["program"].as_str().unwrap_or_default().to_string();
    let cmd_args: Vec<String> = serde_json::from_value(body["args"].clone())?;

    // 交互模式（REPL 等）默认放宽到最大超时
    let timeout_secs = if interactive {
        Some(crate::server::runner::MAX_TIMEOUT_SECS)
    } else {
        None
    };
    let ExecWs { sink, stream } = match ws_exec(
        &server.server_url,
        server.token.as_deref().unwrap_or_default(),
        &program,
        &cmd_args,
        timeout_secs,
    )
    .await
    {
        Ok(w) => w,
        Err(e) => anyhow::bail!("请求服务器失败: {e}"),
    };

    // stdin 策略：管道 → 转发原始字节；TTY+(-i) → 行模式；TTY 无 -i → 立即 EOF
    let mut stdin_task = None;
    if !std::io::stdin().is_terminal() || interactive {
        stdin_task = Some(spawn_stdin_pump(sink, interactive));
    } else {
        let mut sink = sink;
        let _ = sink.send(WsMessage::Text(STDIN_CLOSE.into())).await;
    }

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

    if let Some(task) = stdin_task {
        task.abort();
    }

    match exit_code {
        Some(code) => std::process::exit(code),
        None => {
            // 进程被 kill / 启动失败 / 连接中断
            eprintln!("[remote-cmd] 远程命令异常退出");
            std::process::exit(1);
        }
    }
}

/// stdin 泵：阻塞读本地 stdin（管道字节流 / TTY 行模式）→ WS Binary 帧 → 本地 EOF 时发 stdin_close
fn spawn_stdin_pump(
    mut sink: futures::stream::SplitSink<super::remote::WsStream, WsMessage>,
    interactive: bool,
) -> tokio::task::JoinHandle<()> {
    let (tx, mut rx) = mpsc::channel::<Vec<u8>>(16);
    let reader = tokio::task::spawn_blocking(move || {
        let mut stdin = std::io::stdin();
        if interactive {
            for line in std::io::BufRead::lines(std::io::BufReader::new(stdin)) {
                match line {
                    Ok(mut l) => {
                        l.push('\n');
                        if tx.blocking_send(l.into_bytes()).is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        } else {
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
