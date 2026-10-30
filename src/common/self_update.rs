//! 自更新：从 GitHub Releases 拉取最新版本并替换自身。
//!
//! 流程：版本检测（releases/latest）→ 系统代理 → 下载 zip + SHA256SUMS 校验 →
//! 替换自身 → 重新生成 shim。
//!
//! 替换策略（平台差异）：
//! - Windows：运行中的 exe 不能被覆盖/删除，但**可以改名**——先把当前 exe 改名为
//!   `.exe.old`，再把新二进制改名到原路径，最后 spawn 新进程（--self-cleanup 模式）
//!   等待旧进程退出后删除备份；
//! - Linux：`rename` 覆盖自身是原子的，运行中进程继续持有旧 inode，无需备份步骤。
//!
//! 代理策略：REMOTE_CMD_PROXY 环境变量 > Windows 图形界面代理（注册表 WinINET，
//! 不在环境变量里，需手动读取）> reqwest 默认（HTTP_PROXY/HTTPS_PROXY 等环境变量）> 直连。

use std::fs::{self, File};
use std::io::Write as _;
use std::path::PathBuf;
use std::process::Command;

use anyhow::{anyhow, bail, Context, Result};

/// GitHub Releases 最新版查询接口（未认证限流 60 次/小时/IP，手动触发足够）
const REPO_API: &str = "https://api.github.com/repos/my-vibe-coding/remote-cmd/releases/latest";

pub const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");

// ---------- 目标三元组 ----------

/// 编译期目标三元组：决定下载哪个平台的 asset（命名约定 remote-cmd-{target}.zip）
pub fn current_target() -> &'static str {
    #[cfg(all(target_os = "windows", target_arch = "x86_64"))]
    {
        "x86_64-pc-windows-msvc"
    }
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    {
        "x86_64-unknown-linux-musl"
    }
    #[cfg(not(any(all(target_os = "windows", target_arch = "x86_64"), all(target_os = "linux", target_arch = "x86_64"))))]
    {
        compile_error!("self-update 不支持当前编译目标");
    }
}

// ---------- 代理 ----------

/// Windows 图形界面设置的代理（WinINET 注册表配置）不在环境变量里，需手动读取。
#[cfg(windows)]
fn system_proxy() -> Option<String> {
    let out = Command::new("reg")
        .args([
            "query",
            r"HKCU\Software\Microsoft\Windows\CurrentVersion\Internet Settings",
        ])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let mut enabled = false;
    let mut server: Option<String> = None;
    let mut auto_config: Option<String> = None;
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        let mut it = line.split_whitespace();
        match it.next().unwrap_or_default() {
            "ProxyEnable" => enabled = it.last() == Some("0x1"),
            "ProxyServer" => server = it.last().map(|s| s.to_string()),
            "AutoConfigURL" => auto_config = it.last().map(|s| s.to_string()),
            _ => {}
        }
    }
    // ProxyEnable => 是否启用手动设置代理
    // AutoConfigURL => 是否使用自动配置脚本
    if !enabled && auto_config.is_none() {
        return None;
    }
    let server = server?;
    // ProxyServer 形如 "host:port"（所有协议共用）或 "http=...;https=...;ftp=..."（分协议）
    let https = server
        .split(';')
        .find_map(|kv| kv.strip_prefix("https=").map(|s| s.to_string()))
        .unwrap_or(server);
    if https.is_empty() {
        return None;
    }
    // HTTP 代理以 CONNECT 方式转发 HTTPS 流量，前缀统一补 http://
    if https.starts_with("http://") || https.starts_with("https://") {
        Some(https)
    } else {
        Some(format!("http://{https}"))
    }
}

/// 非 Windows：reqwest 默认读 HTTP_PROXY/HTTPS_PROXY/ALL_PROXY 等环境变量，无需额外处理
#[cfg(not(windows))]
fn system_proxy() -> Option<String> {
    None
}

/// 构建带代理策略的 HTTP 客户端（只限连接超时，下载大文件不做总超时）
fn build_client() -> Result<reqwest::Client> {
    let mut b = reqwest::Client::builder()
        .user_agent("remote-cmd-self-update")
        .connect_timeout(std::time::Duration::from_secs(15))
        .timeout(std::time::Duration::from_secs(300));
    let proxy = std::env::var("REMOTE_CMD_PROXY")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .or_else(system_proxy);
    if let Some(p) = proxy {
        macro_log::i!("自更新使用代理: {p}");
        b = b.proxy(reqwest::Proxy::all(&p)?);
    }
    Ok(b.build()?)
}

// ---------- 版本比较 ----------

/// "v0.1.5" / "0.1.5" -> (0, 1, 5)
fn parse_version(v: &str) -> Option<(u64, u64, u64)> {
    let v = v.trim().trim_start_matches('v');
    let mut it = v.split('.');
    Some((
        it.next()?.trim().parse().ok()?,
        it.next()?.trim().parse().ok()?,
        it.next()?.trim().parse().ok()?,
    ))
}

fn is_newer(latest: &str, current: &str) -> bool {
    match (parse_version(latest), parse_version(current)) {
        (Some(l), Some(c)) => l > c,
        _ => false,
    }
}

// ---------- GitHub Releases ----------

struct ReleaseInfo {
    /// 去掉 v 前缀的版本号
    tag: String,
    /// 匹配当前平台的 zip 下载地址（手动发布的 release 可能没有资产）
    asset_url: Option<String>,
    /// SHA256SUMS 校验文件下载地址（可选）
    sums_url: Option<String>,
}

/// 查询最新 release（asset 查找交给调用方：无新版本时不应因缺资产而报错）
async fn fetch_release(client: &reqwest::Client) -> Result<ReleaseInfo> {
    let resp = client
        .get(REPO_API)
        .header("Accept", "application/vnd.github+json")
        .send()
        .await
        .context("访问 GitHub Releases 失败（如需代理可设置 REMOTE_CMD_PROXY 环境变量）")?
        .error_for_status()
        .context("GitHub API 返回错误")?;
    let v: serde_json::Value = resp.json().await.context("解析 GitHub API 响应失败")?;
    let tag = v["tag_name"]
        .as_str()
        .ok_or_else(|| anyhow!("GitHub 响应缺少 tag_name"))?
        .to_string();
    let assets = v["assets"]
        .as_array()
        .ok_or_else(|| anyhow!("GitHub 响应缺少 assets"))?;
    #[cfg(windows)]
    let want = format!("remote-cmd-{}-{}.zip", tag, current_target());
    #[cfg(not(windows))]
    let want = format!("remote-cmd-{}-{}.tar.gz", tag, current_target());
    let mut asset_url = None;
    let mut sums_url = None;
    for a in assets {
        let name = a["name"].as_str().unwrap_or_default();
        let url = a["browser_download_url"].as_str();
        match name {
            n if n == want => asset_url = url.map(|s| s.to_string()),
            "SHA256SUMS" => sums_url = url.map(|s| s.to_string()),
            _ => {}
        }
    }
    Ok(ReleaseInfo {
        tag: tag.trim_start_matches('v').to_string(),
        asset_url,
        sums_url,
    })
}

/// 仅检查：打印最新版本信息（不下载不更新）
pub async fn check() -> Result<()> {
    let client = build_client()?;
    let rel = fetch_release(&client).await?;
    if !is_newer(&rel.tag, CURRENT_VERSION) {
        macro_log::i!("已是最新版本 v{CURRENT_VERSION}");
        return Ok(());
    }
    match rel.asset_url {
        Some(_) => macro_log::i!(
            "发现新版本 v{}（当前 v{CURRENT_VERSION}），运行 remote-cmd update 执行更新",
            rel.tag
        ),
        None => macro_log::w!(
            "发现新版本 v{}，但该 Release 未提供当前平台（{}）的资产",
            rel.tag,
            current_target()
        ),
    }
    Ok(())
}

// ---------- 下载与校验 ----------

async fn download(client: &reqwest::Client, url: &str, dest: &PathBuf) -> Result<()> {
    let mut resp = client
        .get(url)
        .send()
        .await
        .context("下载失败")?
        .error_for_status()
        .context("下载返回错误状态")?;
    let mut f = File::create(dest)?;
    while let Some(chunk) = resp.chunk().await? {
        f.write_all(&chunk)?;
    }
    f.flush()?;
    Ok(())
}

fn sha256_file(path: &PathBuf) -> Result<String> {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    let mut f = File::open(path)?;
    std::io::copy(&mut f, &mut h)?;
    Ok(h.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

/// 从 SHA256SUMS 文本中解析目标文件的期望哈希（行格式 "<hex>  <文件名>"，
/// 文件名带 tag 前缀，按 "-{target}.zip" 后缀匹配）
fn expected_hash(sums: &str, target: &str) -> Option<String> {
    let suffix = format!("-{target}.zip");
    sums.lines().find_map(|l| {
        let mut it = l.split_whitespace();
        let hash = it.next()?;
        let name = it.next()?;
        (name.ends_with(&suffix)).then(|| hash.to_string())
    })
}

/// 解压 zip 内的可执行文件到 dest（Windows 取 .exe，其它取非 .exe 主文件）
fn extract_binary(zip_path: &PathBuf, dest: &PathBuf) -> Result<()> {
    let f = File::open(zip_path)?;
    let mut ar = zip::ZipArchive::new(f).context("读取 zip 失败")?;
    for i in 0..ar.len() {
        let mut entry = ar.by_index(i)?;
        if entry.is_dir() {
            continue;
        }
        let name = entry.name().to_string();
        #[cfg(windows)]
        let is_binary = name.ends_with(".exe");
        #[cfg(not(windows))]
        let is_binary = !name.ends_with(".exe");
        if !is_binary {
            continue;
        }
        let mut out = File::create(dest)?;
        std::io::copy(&mut entry, &mut out)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(dest, fs::Permissions::from_mode(0o755))?;
        }
        return Ok(());
    }
    Err(anyhow!("压缩包内未找到可执行文件"))
}

// ---------- 替换自身 ----------

fn exe_dir() -> Result<PathBuf> {
    std::env::current_exe()
        .context("获取自身路径失败")?
        .parent()
        .map(|p| p.to_path_buf())
        .ok_or_else(|| anyhow!("无法获取程序所在目录"))
}

/// 执行自更新：检查 → 下载 → 校验 → 替换自身 → 重新生成 shim。
/// 已是最新版本时直接返回（不视为错误）。
pub async fn update() -> Result<()> {
    let client = build_client()?;
    let rel = fetch_release(&client).await?;
    if !is_newer(&rel.tag, CURRENT_VERSION) {
        macro_log::i!("已是最新版本 v{CURRENT_VERSION}（GitHub 最新 v{}）", rel.tag);
        return Ok(());
    }
    let asset_url = rel.asset_url.ok_or_else(|| {
        anyhow!("v{} 未提供当前平台（{}）的资产，请检查 Release", rel.tag, current_target())
    })?;
    macro_log::i!("发现新版本 v{}，开始下载（当前 v{CURRENT_VERSION}）", rel.tag);

    let dir = exe_dir()?;
    let zip_path = dir.join(format!("remote-cmd-{}.zip.download", current_target()));
    let sums_path = dir.join("SHA256SUMS.download");

    // 下载 + 完整性校验
    download(&client, &asset_url, &zip_path).await?;
    let actual = sha256_file(&zip_path)?;
    match &rel.sums_url {
        Some(u) => {
            download(&client, u, &sums_path).await?;
            let sums = fs::read_to_string(&sums_path)?;
            match expected_hash(&sums, current_target()) {
                Some(h) if h.eq_ignore_ascii_case(&actual) => {}
                Some(h) => bail!("SHA256 校验失败：期望 {h}，实际 {actual}"),
                None => bail!("SHA256SUMS 中缺少当前平台的校验项"),
            }
        }
        None => macro_log::w!("该版本未提供 SHA256SUMS，跳过完整性校验"),
    }

    // 解压 + 替换
    let exe = std::env::current_exe().context("获取自身路径失败")?;
    #[cfg(windows)]
    let new_bin = dir.join("remote-cmd.new.exe");
    #[cfg(not(windows))]
    let new_bin = dir.join("remote-cmd.new");
    extract_binary(&zip_path, &new_bin)?;

    #[cfg(windows)]
    {
        // Windows：运行中的 exe 不能覆盖但可以改名——两次 rename 完成换位
        let old = exe.with_extension("exe.old");
        let _ = fs::remove_file(&old);
        fs::rename(&exe, &old).context("重命名旧程序失败")?;
        fs::rename(&new_bin, &exe).context("启用新程序失败")?;
        // spawn 清理进程：等旧进程（本进程）退出后删除备份
        Command::new(&exe).arg("--self-cleanup").arg(&old).spawn().ok();
    }
    #[cfg(not(windows))]
    {
        // Linux：rename 覆盖自身原子生效，运行中进程继续持有旧 inode
        fs::rename(&new_bin, &exe).context("启用新程序失败")?;
    }

    // shim 是主程序的完整副本，同步重新生成（current_exe 此时已指向新二进制内容）
    if let Err(e) = crate::client::script::gen_scripts(None) {
        macro_log::w!("重新生成 shim 失败（可手动执行 gen-script）: {e:#}");
    }

    // 清理下载残留
    let _ = fs::remove_file(&zip_path);
    let _ = fs::remove_file(&sums_path);

    macro_log::i!("已更新到 v{}，重新运行生效", rel.tag);
    Ok(())
}

/// --self-cleanup 模式：等待旧进程退出（文件锁释放）后删除旧版本备份。
/// 由 update 流程 spawn 新二进制时调用，正常使用不会触达。
pub fn self_cleanup(paths: &[String]) {
    for p in paths {
        for _ in 0..50 {
            match fs::remove_file(p) {
                Ok(_) => break,
                Err(_) => std::thread::sleep(std::time::Duration::from_millis(100)),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 无 ESC 前缀的应答残余（ConPTY/crossterm 吞掉 ESC 后逐键到达）应被吞掉
    #[test]
    fn check_system_proxy() {
        let proxy = system_proxy();
        println!("system proxy: {proxy:?}");
    }
}
