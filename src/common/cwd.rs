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
pub fn get_cwd(_pid: u32) -> Result<String, std::io::Error> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "非 Unix 平台不支持读取其他进程 cwd",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

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
