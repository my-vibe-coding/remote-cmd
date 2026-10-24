use anyhow::{anyhow, Result};
use serde_json::json;

use super::config::{load_aliases, render_template, ClientConfig, ServerReg};
use super::remote::{consume_sse, exec_sse, SseEvent};

/// `remote-cmd run <别名> [args...]`：读取本地配置 → 模板渲染 → 转发执行 → 流式输出 → 透传退出码
pub async fn run(alias: &str, args: &[String]) -> Result<()> {
    let (server, body) = resolve_exec_target(alias, args)?;

    let resp = exec_sse(&server.server_url, server.token.as_deref().unwrap_or_default(), &body).await;
    let resp = match resp {
        Ok(r) => r,
        Err(e) => anyhow::bail!("请求服务器失败: {e}"),
    };

    let exit_code = consume_sse(resp, |event| {
        let data = event.decode_output();
        match &event {
            SseEvent::Output { stream, .. } => {
                use std::io::Write;
                if stream == "stderr" {
                    let mut err = std::io::stderr();
                    err.write_all(&data)?;
                    err.flush()?;
                } else {
                    let mut out = std::io::stdout();
                    out.write_all(&data)?;
                    out.flush()?;
                }
            }
            SseEvent::Exit { timeout, .. } => {
                if *timeout {
                    eprintln!("[remote-cmd] 远程命令执行超时");
                }
            }
        }
        Ok(())
    })
    .await?;

    match exit_code {
        Some(code) => std::process::exit(code),
        None => {
            // 进程被 kill / 启动失败
            eprintln!("[remote-cmd] 远程命令异常退出");
            std::process::exit(1);
        }
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
