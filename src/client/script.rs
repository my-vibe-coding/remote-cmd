use anyhow::{anyhow, Result};

use super::config::load_aliases;

/// 别名脚本文件名后缀：Windows 依赖 PATHEXT 需要 .exe，Unix 无后缀
#[cfg(windows)]
const SCRIPT_SUFFIX: &str = ".exe";
#[cfg(not(windows))]
const SCRIPT_SUFFIX: &str = "";

/// 生成别名脚本到 <数据目录>\bin\<别名><SCRIPT_SUFFIX>
/// 内容为主 exe 的副本；exe 启动时根据「所在目录为 bin 且文件名对应已存在别名」自动进入别名执行模式。
/// 相比 Windows 旧版 .bat 脚本：Ctrl+C 直接终止、无「终止批处理操作吗(Y/N)?」提示，且保留退出码。
pub fn gen_script(alias: &str) -> Result<std::path::PathBuf> {
    let aliases = load_aliases()?;
    if !aliases.contains_key(alias) {
        return Err(anyhow!("别名不存在: {alias}"));
    }
    let exe = std::env::current_exe()?;
    let dir = crate::common::paths::client_data_dir().join("bin");
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(format!("{alias}{SCRIPT_SUFFIX}"));
    if path.exists() {
        std::fs::remove_file(&path)?;
    }
    // Unix 上 fs::copy 保留可执行权限位，复制出来即可直接运行
    std::fs::copy(&exe, &path)?;
    // 清理 Windows 旧版 .bat 脚本（PATHEXT 中 .exe 优先级高于 .bat，这里直接删除避免混淆）
    #[cfg(windows)]
    {
        let old = dir.join(format!("{alias}.bat"));
        if old.exists() {
            let _ = std::fs::remove_file(&old);
        }
    }
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
    crate::common::paths::client_data_dir().join("bin").join(format!("{alias}{SCRIPT_SUFFIX}"))
}

/// 当前进程是否为脚本生成创建的别名副本（位于 bin 目录且文件名对应已存在的别名）
/// 用于在 main 中于 clap 解析之前拦截，直接进入别名执行模式
pub fn shim_alias() -> Option<String> {
    let exe = std::env::current_exe().ok()?;
    let bin = crate::common::paths::client_data_dir().join("bin");
    let exe_dir = std::fs::canonicalize(&exe).ok()?.parent()?.to_path_buf();
    let bin_dir = std::fs::canonicalize(&bin).ok()?;
    if exe_dir != bin_dir {
        return None;
    }
    let alias = exe.file_stem()?.to_str()?;
    let aliases = load_aliases().ok()?;
    aliases.contains_key(alias).then(|| alias.to_string())
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
