use anyhow::{anyhow, Result};

use super::config::load_aliases;

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
    // `--` 之后的参数（含别名与 %* 透传参数）不再被 clap 解析为选项
    let content = format!("@\"{}\" run -- \"{}\" %*\r\n", exe.display(), alias);
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

/// 别名对应的脚本路径（无论是否存在）
pub fn script_path(alias: &str) -> std::path::PathBuf {
    crate::common::paths::client_data_dir().join("bin").join(format!("{alias}.bat"))
}

/// 在系统文件管理器中定位文件（Windows: 资源管理器并选中；macOS: Finder；其它 Unix: 打开父目录）
#[cfg(windows)]
pub fn reveal_in_explorer(path: &std::path::Path) -> Result<()> {
    use std::os::windows::ffi::OsStrExt;

    unsafe {
        use windows_sys::Win32::Foundation::S_OK;
        use windows_sys::Win32::System::Com::{CoInitializeEx, CoTaskMemFree, COINIT_APARTMENTTHREADED};
        use windows_sys::Win32::UI::Shell::{ILCreateFromPathW, SHOpenFolderAndSelectItems};

        // 已初始化时返回 S_FALSE，同样视为可用
        let _ = CoInitializeEx(std::ptr::null_mut(), COINIT_APARTMENTTHREADED as u32);
        let wide: Vec<u16> = path.as_os_str().encode_wide().chain(std::iter::once(0)).collect();
        let pidl = ILCreateFromPathW(wide.as_ptr());
        if pidl.is_null() {
            return Err(anyhow!("解析路径失败"));
        }
        let hr = SHOpenFolderAndSelectItems(pidl, 0, std::ptr::null(), 0);
        CoTaskMemFree(pidl.cast());
        if hr != S_OK {
            return Err(anyhow!("SHOpenFolderAndSelectItems 失败: 0x{hr:08X}"));
        }
    }
    Ok(())
}

#[cfg(target_os = "macos")]
pub fn reveal_in_explorer(path: &std::path::Path) -> Result<()> {
    std::process::Command::new("open").arg("-R").arg(path).spawn()?;
    Ok(())
}

#[cfg(all(unix, not(target_os = "macos")))]
pub fn reveal_in_explorer(path: &std::path::Path) -> Result<()> {
    let dir = path.parent().unwrap_or(path);
    std::process::Command::new("xdg-open").arg(dir).spawn()?;
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
