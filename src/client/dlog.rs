//! 客户端专用文件调试日志。
//!
//! 不能用 macro_log::d!：其底层走 stdout 打印，而 PTY 会话的 stdout 就是终端本身，
//! 日志会混入远端输出污染画面。本模块将日志追加写入客户端数据目录下的 debug.log。

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

/// 日志文件大小上限：超过则重建（防无限增长）
const MAX_LOG_BYTES: u64 = 8 * 1024 * 1024;

fn sink() -> Option<&'static Mutex<File>> {
    static SINK: OnceLock<Option<Mutex<File>>> = OnceLock::new();
    SINK.get_or_init(|| {
        let path = log_path();
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        // 超限重建，保留最近一轮日志
        if let Ok(meta) = std::fs::metadata(&path)
            && meta.len() > MAX_LOG_BYTES
        {
            let _ = std::fs::remove_file(&path);
        }
        OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .ok()
            .map(Mutex::new)
    })
    .as_ref()
}

/// 日志文件路径：%USERPROFILE%\.remote-cmd\client\debug.log
pub fn log_path() -> PathBuf {
    crate::common::paths::client_data_dir().join("debug.log")
}

/// 追加一条日志（由 dlog! 宏调用，自带 file!/line! 定位信息）
pub fn write(file: &str, line: u32, args: std::fmt::Arguments<'_>) {
    let Some(mutex) = sink() else { return };
    if let Ok(mut f) = mutex.lock() {
        let name = file.rsplit(['\\', '/']).next().unwrap_or(file);
        let _ = writeln!(
            f,
            "{} {}:{} {}",
            chrono::Local::now().format("%H:%M:%S%.3f"),
            name,
            line,
            args
        );
    }
}

/// 客户端调试日志宏：写入 client 目录下 debug.log（不影响终端输出流）
#[macro_export]
macro_rules! dlog {
    ($($arg: tt)+) => {
        $crate::client::dlog::write(file!(), line!(), format_args!($($arg)+))
    };
}
