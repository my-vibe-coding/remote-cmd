use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::common::json_file::{load_json, save_json_atomic};
use crate::common::paths::client_data_dir;
use crate::common::AppResult;

/// 单台服务器的注册信息
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerReg {
    pub server_url: String,
    #[serde(default)]
    pub client_id: String,
    #[serde(default)]
    pub hostname: String,
    #[serde(default)]
    pub token: Option<String>,
    #[serde(default)]
    pub registered: bool,
}

/// 客户端配置：%USERPROFILE%\.remote-cmd\client\client.json
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ClientConfig {
    #[serde(default)]
    pub machine_guid: String,
    #[serde(default)]
    pub hostname: String,
    /// 多服务器注册列表
    #[serde(default)]
    pub servers: Vec<ServerReg>,
    // ---- 旧版单服务器字段：仅用于加载迁移，不再写回 ----
    #[serde(default, skip_serializing)]
    pub server_url: String,
    #[serde(default, skip_serializing)]
    pub client_id: String,
    #[serde(default, skip_serializing)]
    pub token: Option<String>,
    #[serde(default, skip_serializing)]
    pub registered: bool,
}

impl ClientConfig {
    pub fn load() -> AppResult<Self> {
        let path = config_path();
        let mut cfg: ClientConfig = load_json(&path)?.unwrap_or_default();
        // 旧版单服务器配置迁移到 servers
        if cfg.servers.is_empty() && !cfg.server_url.is_empty() {
            cfg.servers.push(ServerReg {
                server_url: cfg.server_url.clone(),
                client_id: cfg.client_id.clone(),
                hostname: cfg.hostname.clone(),
                token: cfg.token.clone(),
                registered: cfg.registered,
            });
            cfg.server_url.clear();
            cfg.client_id.clear();
            cfg.token = None;
            cfg.registered = false;
            let _ = cfg.save();
        }
        Ok(cfg)
    }

    pub fn save(&self) -> AppResult<()> {
        save_json_atomic(&config_path(), self)
    }

    pub fn find_server(&self, url: &str) -> Option<&ServerReg> {
        self.servers.iter().find(|s| s.server_url == url)
    }

    /// 默认服务器：第一个已注册的
    pub fn default_server(&self) -> Option<&ServerReg> {
        self.servers
            .iter()
            .find(|s| s.registered && s.token.is_some())
    }
}

pub fn config_path() -> PathBuf {
    client_data_dir().join("client.json")
}

/// 命令别名映射：%USERPROFILE%\.remote-cmd\client\aliases.json
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AliasDef {
    /// 命令模板，支持 {args} 占位（出现多次每处替换；未出现且 args 非空则追加末尾）
    pub template: String,
    /// 绑定的执行服务器地址；空/None 表示使用默认服务器
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server: Option<String>,
}

pub type Aliases = std::collections::BTreeMap<String, AliasDef>;

pub fn aliases_path() -> PathBuf {
    client_data_dir().join("aliases.json")
}

pub fn load_aliases() -> AppResult<Aliases> {
    Ok(load_json(&aliases_path())?.unwrap_or_default())
}

pub fn save_aliases(aliases: &Aliases) -> AppResult<()> {
    save_json_atomic(&aliases_path(), aliases)
}

/// 模板渲染：{args} 占位替换。
/// 参数会做引号包裹转义（含空格/制表符/引号/空参数），保证经 shlex_split 还原后与原始参数一一对应。
pub fn render_template(template: &str, args: &[String]) -> String {
    let joined = args.iter().map(|a| quote_arg(a)).collect::<Vec<_>>().join(" ");
    if template.contains("{args}") {
        template.replace("{args}", &joined)
    } else if !args.is_empty() {
        format!("{template} {joined}")
    } else {
        template.to_string()
    }
}

/// 单个参数的 shell 转义：含空白/引号或为空时用双引号包裹，内部双引号转义为 \"
fn quote_arg(arg: &str) -> String {
    if arg.is_empty() || arg.chars().any(|c| c == ' ' || c == '\t' || c == '"') {
        format!("\"{}\"", arg.replace('"', "\\\""))
    } else {
        arg.to_string()
    }
}

/// 解析命令行为 program + args（不做 shell 展开，安全透传）
pub fn parse_command_line(cmd: &str) -> Option<(String, Vec<String>)> {
    let parts = shlex_split(cmd);
    let mut it = parts.into_iter();
    let program = it.next()?;
    Some((program, it.collect()))
}

/// 简易 shell 分词：支持双引号与 \" 转义
fn shlex_split(s: &str) -> Vec<String> {
    let mut out = vec![];
    let mut cur = String::new();
    let mut in_quote = false;
    let mut has_token = false;
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            // \" → 字面双引号（与 quote_arg 的转义配对）
            '\\' if chars.peek() == Some(&'"') => {
                cur.push('"');
                has_token = true;
            }
            '"' => {
                in_quote = !in_quote;
                has_token = true;
            }
            ' ' | '\t' if !in_quote => {
                if has_token {
                    out.push(std::mem::take(&mut cur));
                    has_token = false;
                }
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

#[cfg(test)]
mod tests {
    use super::*;

    /// 渲染→分词往返：含空格/引号/空串的参数应与原始参数一一对应
    #[test]
    fn render_then_split_preserves_args_with_spaces() {
        let args = vec![
            "hello world".to_string(),
            "say \"hi\"".to_string(),
            "plain".to_string(),
            String::new(),
        ];
        let cmdline = render_template("git add {args}", &args);
        let (program, rest) = parse_command_line(&cmdline).unwrap();
        assert_eq!(program, "git");
        assert_eq!(rest[0], "add");
        assert_eq!(&rest[1..], args.as_slice());
    }
}
