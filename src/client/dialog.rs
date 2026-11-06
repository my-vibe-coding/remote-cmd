//! 自更新进度弹窗（xdialog 封装）。
//!
//! xdialog 的事件循环必须占用主线程（main.rs 已把 update 命令调度进
//! `XDialogBuilder::run_i32`，更新逻辑运行在其拉起的线程上）。本模块在该逻辑线程上
//! 创建并驱动弹窗；无显示环境（无头 Linux 服务器等）时 `show_progress` 返回
//! `NoBackendAvailable`，所有操作退化为 no-op，更新流程照常执行。

use std::io::{BufRead as _, Write as _};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::Mutex;

use serde_json::json;
use xdialog::{ProgressDialogProxy, XDialogIcon};

/// 进度弹窗刷新节流间隔（避免逐 chunk 调用拖慢传输）
const PROG_REFRESH: std::time::Duration = std::time::Duration::from_millis(100);

/// 自更新进度弹窗句柄：Drop 时关闭弹窗；创建失败（无显示环境等）时全部为 no-op
pub struct UpdateProgress(Option<ProgressDialogProxy>);

impl UpdateProgress {
    /// 创建进度弹窗。失败（无显示环境 / 后端不可用）时返回 no-op 句柄
    pub fn new(title: &str, main_instruction: &str, body: &str) -> Self {
        let proxy = xdialog::show_progress(title, main_instruction, body, XDialogIcon::Information).ok();
        Self(proxy)
    }

    /// 设置进度（fraction: 0.0~1.0）与说明文字
    pub fn set(&self, fraction: f64, text: &str) {
        if let Some(p) = &self.0 {
            let _ = p.set_value(fraction.clamp(0.0, 1.0) as f32);
            let _ = p.set_text(text);
        }
    }

    /// 切换为不确定模式（用于校验、解压、替换等无法量化的阶段）
    pub fn indeterminate(&self, text: &str) {
        if let Some(p) = &self.0 {
            let _ = p.set_indeterminate();
            let _ = p.set_text(text);
        }
    }
}

/// 字节数人性化显示："0 B"、"3.2 MB"
pub fn fmt_bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{n} B")
    } else {
        format!("{v:.1} {}", UNITS[i])
    }
}

// ---------- 上传进度弹窗（独立子进程方案） ----------

/// 上传进度弹窗句柄：spawn 自身 exe（--progress-dialog）作为弹窗宿主，经 stdin 行式
/// JSON 汇报进度。上传方运行在 tokio 常驻进程（主线程无法让给 xdialog 事件循环），
/// 故弹窗须在子进程的主线程创建。Drop 时关闭 stdin → 宿主读到 EOF 自动退出，
/// 上传方崩溃也无窗口残留；spawn 失败（无显示环境等）时全部为 no-op
pub struct UploadProgress {
    child: Option<Child>,
    stdin: Mutex<Option<ChildStdin>>,
    /// set 节流时间戳（None = 未发送过）
    last: Mutex<Option<std::time::Instant>>,
}

impl UploadProgress {
    /// spawn 弹窗宿主并发出 begin 帧（创建弹窗）。失败时返回 no-op 句柄
    pub fn spawn(title: &str, main_instruction: &str, body: &str) -> Self {
        let mut s = Self {
            child: None,
            stdin: Mutex::new(None),
            last: Mutex::new(None),
        };
        if let Some(mut c) = spawn_host() {
            let stdin = c.stdin.take();
            s.child = Some(c);
            if let Some(w) = stdin {
                *s.stdin.lock().unwrap() = Some(w);
                s.send(json!({
                    "type": "begin",
                    "title": title,
                    "main": main_instruction,
                    "text": body,
                }));
            }
        }
        s
    }

    /// 设置进度（fraction: 0.0~1.0）与说明文字。按 PROG_REFRESH 节流；
    /// fraction ≥ 1.0 不节流（最终 100% 必达）
    pub fn set(&self, fraction: f64, text: &str) {
        {
            let mut last = self.last.lock().unwrap();
            if fraction < 1.0 {
                if let Some(t) = *last {
                    if t.elapsed() < PROG_REFRESH {
                        return;
                    }
                }
            }
            *last = Some(std::time::Instant::now());
        }
        self.send(json!({ "type": "set", "fraction": fraction.clamp(0.0, 1.0), "text": text }));
    }

    /// 切换为不确定模式（总量未知等场景）
    pub fn indeterminate(&self, text: &str) {
        self.send(json!({ "type": "indeterminate", "text": text }));
    }

    fn send(&self, v: serde_json::Value) {
        if let Ok(mut g) = self.stdin.lock() {
            if let Some(w) = g.as_mut() {
                let _ = writeln!(w, "{v}");
            }
        }
    }
}

impl Drop for UploadProgress {
    fn drop(&mut self) {
        // 关闭 stdin（宿主 EOF 退出）并回收子进程
        if let Ok(mut g) = self.stdin.lock() {
            *g = None;
        }
        if let Some(mut c) = self.child.take() {
            let _ = c.wait();
        }
    }
}

/// spawn 弹窗宿主子进程（自身 exe + --progress-dialog）。
/// 输出全部改道 null：避免子进程意外输出污染 PTY 会话终端；Windows 额外
/// CREATE_NO_WINDOW 脱离控制台
fn spawn_host() -> Option<Child> {
    let mut cmd = Command::new(std::env::current_exe().ok()?);
    cmd.arg("--progress-dialog")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    cmd.spawn().ok()
}

/// 弹窗宿主主循环（--progress-dialog 模式，运行在子进程主线程的 xdialog 逻辑线程上）。
/// 协议（行式 JSON）：
/// - `{"type":"begin","title":..,"main":..,"text":..}` → 创建弹窗（主指令创建后不可改）
/// - `{"type":"set","fraction":..,"text":..}` → 百分比进度
/// - `{"type":"indeterminate","text":..}` → 不确定模式
/// - `{"type":"close"}` / stdin EOF → 退出（弹窗随 Drop 关闭，防窗口残留）
pub fn run_progress_host() {
    let mut prog: Option<UpdateProgress> = None;
    for line in std::io::stdin().lock().lines() {
        let Ok(line) = line else { break };
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) else { continue };
        let text = v["text"].as_str().unwrap_or_default();
        match v["type"].as_str().unwrap_or_default() {
            "begin" => {
                prog = Some(UpdateProgress::new(
                    v["title"].as_str().unwrap_or_default(),
                    v["main"].as_str().unwrap_or_default(),
                    text,
                ));
            }
            "set" => {
                if let Some(p) = &prog {
                    p.set(v["fraction"].as_f64().unwrap_or(0.0), text);
                }
            }
            "indeterminate" => {
                if let Some(p) = &prog {
                    p.indeterminate(text);
                }
            }
            "close" => break,
            _ => {}
        }
    }
}

// ---------- 同步询问弹窗 ----------

/// 目录冲突询问的三个选择：覆盖合并 / 重命名 / 放弃上传
pub enum OverwriteChoice {
    Overwrite,
    Rename,
    Cancel,
}

/// 目录冲突询问弹窗（是/否/取消三按钮）。其它平台无统一原生对话框机制 →
/// Rename（调用方默认值）。
///
/// 保留 MessageBoxW 而非 xdialog：xdialog 的事件循环须占用主线程（Builder 约束），
/// 而客户端进程的主线程已被 tokio 运行时占用、无法初始化其事件循环；这里是同步
/// 阻塞式询问，MessageBoxW 恰好满足（与 BatchProgress 的子进程方案不同，询问需要
/// 立即拿到应答，无法经 stdin 协议异步驱动）
#[cfg(windows)]
pub fn ask_overwrite_dialog(rel_root: &str) -> OverwriteChoice {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        MessageBoxW, IDNO, IDYES, MB_ICONQUESTION, MB_SETFOREGROUND, MB_TOPMOST, MB_YESNOCANCEL,
    };
    let text = format!(
        "服务器上已存在目录「{rel_root}」。\n\n是 = 覆盖合并（同名文件覆盖，其余保留）\n否 = 重命名为新目录（原目录不动）\n取消 = 放弃本次上传"
    );
    let text_w: Vec<u16> = text.encode_utf16().chain([0]).collect();
    let title_w: Vec<u16> = "目录上传".encode_utf16().chain([0]).collect();
    let r = unsafe {
        MessageBoxW(
            std::ptr::null_mut(),
            text_w.as_ptr(),
            title_w.as_ptr(),
            // TOPMOST：进度弹窗（xdialog 置顶窗）可能已存在，仅 SETFOREGROUND 抢不到最顶层
            MB_YESNOCANCEL | MB_ICONQUESTION | MB_SETFOREGROUND | MB_TOPMOST,
        )
    };
    // 关闭按钮（X）同样返回 IDCANCEL，视为放弃
    if r == IDYES {
        OverwriteChoice::Overwrite
    } else if r == IDNO {
        OverwriteChoice::Rename
    } else {
        OverwriteChoice::Cancel
    }
}

/// 非 Windows：无统一原生对话框，默认重命名
#[cfg(not(windows))]
pub fn ask_overwrite_dialog(_rel_root: &str) -> OverwriteChoice {
    OverwriteChoice::Rename
}
