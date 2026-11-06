//! 自更新进度弹窗（xdialog 封装）。
//!
//! xdialog 的事件循环必须占用主线程（main.rs 已把 update 命令调度进
//! `XDialogBuilder::run_i32`，更新逻辑运行在其拉起的线程上）。本模块在该逻辑线程上
//! 创建并驱动弹窗；无显示环境（无头 Linux 服务器等）时 `show_progress` 返回
//! `NoBackendAvailable`，所有操作退化为 no-op，更新流程照常执行。

use xdialog::{ProgressDialogProxy, XDialogIcon};

/// 自更新进度弹窗句柄：Drop 时关闭弹窗；创建失败（无显示环境等）时全部为 no-op
pub struct UpdateProgress(Option<ProgressDialogProxy>);

impl UpdateProgress {
    /// 创建进度弹窗。失败（无显示环境 / 后端不可用）时返回 no-op 句柄
    pub fn new(main_instruction: &str, body: &str) -> Self {
        let proxy = xdialog::show_progress(
            "remote-cmd 更新",
            main_instruction,
            body,
            XDialogIcon::Information,
        )
        .ok();
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
