//! PTY 探针：外层 ConPTY + 模拟 Windows Terminal 应答行为（CPR 握手应答），
//! 联调验证 Linux 服务器上 vim/tmux 终端查询链路。仅用于调查，随查随删。

use portable_pty::{native_pty_system, CommandBuilder, PtySize};
use std::io::Read;
use std::time::{Duration, Instant};

fn main() {
    let start = Instant::now();
    let exe = std::env::var("PTY_PROBE_EXE").expect("PTY_PROBE_EXE");
    let args: Vec<String> = match std::env::var("PTY_PROBE_ARGS") {
        Ok(s) if !s.is_empty() => s.split(' ').map(|s| s.to_string()).collect(),
        _ => vec![],
    };
    let secs: u64 = std::env::var("PTY_PROBE_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(15);

    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(PtySize { rows: 30, cols: 120, pixel_width: 0, pixel_height: 0 })
        .expect("openpty");

    let mut cmd = CommandBuilder::new(&exe);
    cmd.args(&args);
    let mut child = pair.slave.spawn_command(cmd).expect("spawn");
    drop(pair.slave);

    let mut reader = pair.master.try_clone_reader().expect("reader");
    let mut writer = pair.master.take_writer().expect("writer");
    let reader_thread = std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        loop {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let text = String::from_utf8_lossy(&buf[..n]).replace('\x1b', "<ESC>");
                    eprintln!("[{:?}] OUT {:?}", start.elapsed(), text.replace('\r', ""));
                    // 模拟 Windows Terminal：对所有查询形态立即应答。
                    // 服务端应丢弃远端查询——除客户端 ConPTY 自身启动握手外，
                    // 任何 REPLY 触发都意味着查询泄漏到了宿主终端。
                    let replies: &[(&str, &[u8])] = &[
                        ("<ESC>[6n", b"\x1b[1;1R"),
                        ("<ESC>[c", b"\x1b[?61;4;6;7;14;21;22;23;24;28;32;42;52c"),
                        ("<ESC>[>c", b"\x1b[>0;10;1c"),
                        ("<ESC>]10;?", b"\x1b]10;rgb:cccc/cccc/cccc\x1b\\"),
                        ("<ESC>]11;?", b"\x1b]11;rgb:0c0c/0c0c/0c0c\x1b\\"),
                        ("<ESC>[?2026$p", b"\x1b[?2026;2$y"),
                        ("<ESC>[?2048$p", b"\x1b[?2048;0$y"),
                        ("<ESC>[?996n", b"\x1b[?996;1n"),
                        ("<ESC>[>q", b"\x1b[>1;2;1q"),
                        ("<ESC>[14t", b"\x1b[4;700;1450t"),
                        ("<ESC>[18t", b"\x1b[8;35;145t"),
                    ];
                    for (pat, reply) in replies.iter() {
                        if text.contains(*pat) {
                            eprintln!(
                                "[{:?}] REPLY -> {}（泄漏！服务端应已丢弃查询）",
                                start.elapsed(),
                                pat
                            );
                            let _ = writer.write_all(*reply);
                            let _ = writer.flush();
                        }
                    }
                }
            }
        }
    });

    std::thread::sleep(Duration::from_secs(secs));
    let _ = child.kill();
    let _ = child.wait();
    let _ = reader_thread.join();
    eprintln!("[{:?}] probe done", start.elapsed());
}
