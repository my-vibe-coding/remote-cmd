use anyhow::{anyhow, Result};

use super::config::{load_aliases, AliasDef};

/// 生成 bat 脚本到 %USERPROFILE%\.remote-cmd\bin\<别名>.bat
/// 内容: @"<本 exe>" run <别名> %*
pub fn gen_script(alias: &str) -> Result<std::path::PathBuf> {
    let aliases = load_aliases()?;
    if !aliases.contains_key(alias) {
        return Err(anyhow!("别名不存在: {alias}"));
    }
    let exe = std::env::current_exe()?;
    let dir = crate::common::paths::client_data_dir().join("bin");
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(format!("{alias}.bat"));
    let content = format!("@\"{}\" run \"{}\" %*\r\n", exe.display(), alias);
    std::fs::write(&path, content)?;
    Ok(path)
}

/// 为所有别名生成脚本
pub fn gen_scripts(alias: Option<&str>) -> Result<()> {
    match alias {
        Some(name) => {
            let path = gen_script(name)?;
            macro_log::i!("已生成脚本: {}", path.display());
        }
        None => {
            let aliases = load_aliases()?;
            if aliases.is_empty() {
                macro_log::i!("暂无别名，请先在客户端页面创建");
                return Ok(());
            }
            for name in aliases.keys() {
                let path = gen_script(name)?;
                macro_log::i!("已生成脚本: {}", path.display());
            }
            macro_log::i!("提示: 将以下目录加入 PATH 即可在任意位置调用:");
            let dir = crate::common::paths::client_data_dir().join("bin");
            macro_log::i!("  {}", dir.display());
        }
    }
    Ok(())
}

/// 别名 CRUD 校验：名称仅允许字母数字下划线连字符
pub fn validate_alias_name(name: &str) -> Result<()> {
    if name.is_empty() || name.len() > 64 {
        return Err(anyhow!("别名长度需在 1-64 之间"));
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        return Err(anyhow!("别名仅允许字母、数字、下划线、连字符"));
    }
    Ok(())
}

pub fn validate_template(template: &str) -> Result<()> {
    if template.trim().is_empty() {
        return Err(anyhow!("命令模板不能为空"));
    }
    Ok(())
}

#[allow(unused)]
fn _keep_alias_def(_d: &AliasDef) {}
