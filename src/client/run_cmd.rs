use std::collections::BTreeMap;

use anyhow::{anyhow, Result};
use serde_json::json;

use super::config::{load_aliases, render_template, ClientConfig};
use super::remote::{consume_sse, exec_sse, SseEvent};

/// `remote-cmd run <别名> [args...]`：读取本地配置 → 模板渲染 → 转发执行 → 流式输出 → 透传退出码
pub async fn run(alias: &str, args: &[String]) -> Result<()> {
    let config = ClientConfig::load()?;
    if !config.registered || config.token.is_none() {
        anyhow::bail!("尚未注册，请先启动 remote-cmd 完成注册申请");
    }
    let server_url = if config.server_url.is_empty() {
        anyhow::bail!("未配置服务器地址，请先启动 remote-cmd 完成配置")
    } else {
        config.server_url.clone()
    };

    let aliases: BTreeMap<String, _> = load_aliases()?;
    let Some(def) = aliases.get(alias) else {
        anyhow::bail!("别名不存在: {alias}，请先在客户端页面创建");
    };
    let cmdline = render_template(&def.template, args);
    let Some((program, rest)) = super::config::parse_command_line(&cmdline) else {
        anyhow::bail!("命令模板无效: {cmdline}");
    };

    let body = json!({
        "program": program,
        "args": rest,
    });

    let resp = exec_sse(&server_url, config.token.as_deref().unwrap(), &body).await;
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

/// 供本地页面 /api/local/exec 使用的共享逻辑：返回 SSE 字节流前的校验
pub fn resolve_alias(alias: &str, args: &[String]) -> Result<serde_json::Value> {
    let aliases = load_aliases()?;
    let Some(def) = aliases.get(alias) else {
        return Err(anyhow!("别名不存在: {alias}"));
    };
    let cmdline = render_template(&def.template, args);
    let Some((program, rest)) = super::config::parse_command_line(&cmdline) else {
        return Err(anyhow!("命令模板无效: {cmdline}"));
    };
    Ok(json!({ "program": program, "args": rest }))
}
