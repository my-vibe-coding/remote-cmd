use portable_pty::{Child, CommandBuilder, PtySize};

#[allow(unused)]
fn run(program: &str, args: &[&str], cwd: Option<&str>) -> Option<Box<dyn Child + Send + Sync>> {
    let pty_system = portable_pty::native_pty_system();
    let pair = match pty_system.openpty(PtySize {
        rows: 24,
        cols: 80,
        pixel_width: 0,
        pixel_height: 0,
    }) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("打开 PTY 失败: {:?}", e);
            return None;
        }
    };

    let mut cmd = CommandBuilder::new(program);
    cmd.args(args);
    if let Some(cwd) = cwd {
        cmd.cwd(cwd);
    }
    let child = match pair.slave.spawn_command(cmd) {
        Ok(c) => c,
        Err(err) => {
            macro_log::w!("PTY 启动进程失败 {}: {err}", program);
            return None;
        }
    };
    drop(pair.slave); // 父进程立即释放 slave 端
    Some(child)
}

#[cfg(unix)]
pub fn get_cwd(pid: u32) -> Result<String, std::io::Error> {
    let path = std::fs::read_link(format!("/proc/{}/cwd", pid))?;
    Ok(path.to_string_lossy().into_owned())
}

/// 非 Unix 平台无公开接口读取其他进程的 cwd（Windows 无 /proc），
/// 调用方按失败回退默认目录处理
#[cfg(not(unix))]
#[allow(unused)] // Windows 服务端不参与 cwd 解析链，保留接口对称性
pub fn get_cwd(_pid: u32) -> Result<String, std::io::Error> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "非 Unix 平台不支持读取其他进程 cwd",
    ))
}

/// 上传落地目录解析（Unix）：tmux 感知的进程 cwd 解析链。
/// 1. tmux 检测：tmux 的 pane shell 由 tmux server（守护进程，脱离我们的 PTY）启动，
///    外层注册 shell 的 /proc cwd 在用户 cd 进 tmux 后不再变化——因此先探测 tmux
///    客户端（shell 本身 exec tmux、或其直接子进程，comm="tmux" 且控制终端与 shell
///    相同，tty 匹配排除守护化的 server），读取其 TMUX 环境变量（socket,pid,会话id），
///    执行 `tmux -S <socket> display-message -p -t $<id> #{pane_current_path}` 取
///    活动 pane 前台进程的实时 cwd；
/// 2. 无 tmux / 查询失败 → /proc/<pid>/cwd（普通 shell 的实时 cwd）。
#[cfg(unix)]
pub async fn resolve_process_cwd(pid: u32) -> Option<String> {
    if let Some(dir) = tmux_pane_cwd(pid).await {
        return Some(dir);
    }
    get_cwd(pid).ok()
}

/// 非 Unix：无 /proc 与 tmux 语义，直接回退默认目录
#[cfg(not(unix))]
pub async fn resolve_process_cwd(_pid: u32) -> Option<String> {
    None
}

/// tmux 活动 pane cwd 探测与查询；任何一步失败返回 None（调用方降级）
#[cfg(unix)]
async fn tmux_pane_cwd(shell_pid: u32) -> Option<String> {
    // 候选：shell 本身（exec tmux 场景）与其直接子进程（bash fork tmux 场景）。
    // tmux 客户端与 shell 共享外层 PTY 的控制终端；tmux server 守护化后无控制终端，
    // 不会被命中。tmux new -d（分离创建）客户端立即退出，探不到 → 自然走普通 cwd。
    let shell_tty = proc_tty_nr(shell_pid)?;
    let mut candidates = vec![shell_pid];
    candidates.extend(children_of(shell_pid));
    let client = candidates.into_iter().find(|&p| {
        proc_comm(p).as_deref() == Some("tmux") && proc_tty_nr(p) == Some(shell_tty)
    })?;

    let tmux_env = proc_environ_kv(client, "TMUX")?;
    let (socket, target) = parse_tmux_env(&tmux_env)?;
    query_tmux_pane_cwd(&socket, target).await
}

/// 解析 TMUX 环境变量 "socket_path,server_pid,session_id" → (socket, -t 目标)。
/// 会话 id 为数字时转成 tmux 唯一会话 id 语法 "$<n>"（多会话下精确定位本客户端
/// 挂接的会话，而非"最近活跃会话"）；非数字放弃精确定位（target = None）。
#[cfg(unix)]
fn parse_tmux_env(env: &str) -> Option<(String, Option<String>)> {
    let mut parts = env.splitn(3, ',');
    let socket = parts.next()?.trim();
    if !socket.starts_with('/') || socket.len() > 128 || socket.chars().any(|c| c.is_control()) {
        return None;
    }
    parts.next()?; // server pid，无需使用
    let session = parts.next()?.trim();
    let target = session.parse::<u64>().ok().map(|n| format!("${n}"));
    Some((socket.to_string(), target))
}

/// 执行 tmux display-message 查询活动 pane 的当前路径（2s 超时，防 tmux server 卡死拖住上传）
#[cfg(unix)]
async fn query_tmux_pane_cwd(socket: &str, target: Option<String>) -> Option<String> {
    let mut cmd = tokio::process::Command::new("tmux");
    cmd.arg("-S").arg(socket).arg("display-message").arg("-p");
    if let Some(t) = &target {
        cmd.arg("-t").arg(t);
    }
    cmd.arg("#{pane_current_path}");
    cmd.stdin(std::process::Stdio::null());
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::null());
    let out = tokio::time::timeout(std::time::Duration::from_secs(2), cmd.output())
        .await
        .ok()?
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let dir = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (dir.starts_with('/') && std::path::Path::new(&dir).is_dir()).then_some(dir)
}

/// 直接子进程列表（/proc/<pid>/task/<pid>/children，空格分隔）
#[cfg(unix)]
fn children_of(pid: u32) -> Vec<u32> {
    std::fs::read_to_string(format!("/proc/{pid}/task/{pid}/children"))
        .map(|s| s.split_whitespace().filter_map(|t| t.parse().ok()).collect())
        .unwrap_or_default()
}

#[cfg(unix)]
fn proc_comm(pid: u32) -> Option<String> {
    std::fs::read_to_string(format!("/proc/{pid}/comm"))
        .ok()
        .map(|s| s.trim().to_string())
}

/// 控制终端设备号（/proc/<pid>/stat 第 7 字段）；守护进程无控制终端为 0。
/// comm 可能含空格与 ')'，从最后一个 ')' 之后解析
#[cfg(unix)]
fn proc_tty_nr(pid: u32) -> Option<u64> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let rest = stat.rsplit(')').next()?;
    let mut it = rest.split_whitespace();
    it.next()?; // state
    it.next()?; // ppid
    it.next()?; // pgrp
    it.next()?; // session
    it.next()?.parse().ok()
}

/// 读取进程环境变量（/proc/<pid>/environ，NUL 分隔；同 uid 进程可读）
#[cfg(unix)]
fn proc_environ_kv(pid: u32, key: &str) -> Option<String> {
    let data = std::fs::read(format!("/proc/{pid}/environ")).ok()?;
    data.split(|&b| b == 0)
        .find(|e| e.starts_with(key.as_bytes()) && e.get(key.len()) == Some(&b'='))
        .and_then(|e| String::from_utf8(e[key.len() + 1..].to_vec()).ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// TMUX 环境变量解析：socket 校验、会话 id → "$<n>" 目标、非法输入放弃
    #[test]
    #[cfg(unix)]
    fn parse_tmux_env_cases() {
        let (socket, target) = parse_tmux_env("/tmp/tmux-0/default,4231,0").unwrap();
        assert_eq!(socket, "/tmp/tmux-0/default");
        assert_eq!(target.as_deref(), Some("$0"));

        // 自定义 socket 路径与非 0 会话 id
        let (socket, target) = parse_tmux_env("/home/u/.tmux.sock,123,7").unwrap();
        assert_eq!(socket, "/home/u/.tmux.sock");
        assert_eq!(target.as_deref(), Some("$7"));

        // 会话段非数字（如命名会话残留）→ 放弃精确定位但不整体失败
        let (_, target) = parse_tmux_env("/tmp/tmux-0/default,4231,abc").unwrap();
        assert_eq!(target, None);

        // 非绝对路径 socket / 缺段 / 空串 → None
        assert!(parse_tmux_env("tmux-0/default,1,0").is_none());
        assert!(parse_tmux_env("/tmp/tmux-0/default,1").is_none());
        assert!(parse_tmux_env("").is_none());
        // 控制字符 socket → None
        assert!(parse_tmux_env("/tmp/a\nb,1,0").is_none());
    }

    // #[tokio::test]
    async fn test_run() {
        let child = run("conhost", &["--", "cmd"], Some("D:\\"));
        // let child = run("conhost", &["--", "cmd"], None);
        assert!(child.is_some());
        let mut child = child.unwrap();
        // 获取 pid
        let pid = child.process_id().unwrap();
        macro_log::i!("子进程 PID: {}", pid);
        assert!(pid > 0);
        let _ = child.wait();
    }

    #[test]
    #[cfg(unix)]
    fn test_get_cwd() {
        let child = run("bash", &["-c", "sleep 1"], Some("/tmp")).unwrap();
        let pid = child.process_id().unwrap();
        let cwd = get_cwd(pid).unwrap();
        macro_log::i!("当前工作目录: {}", cwd);
        assert!(!cwd.is_empty());
        assert_eq!(cwd, "/tmp");
    }
}
