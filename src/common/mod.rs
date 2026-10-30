pub mod crypto;
pub mod fingerprint;
pub mod json_file;
pub mod net;
pub mod paths;
pub mod self_update;

use std::io;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum AppError {
    #[error("IO 错误: {0}")]
    Io(#[from] io::Error),
    #[error("JSON 错误: {0}")]
    Json(#[from] serde_json::Error),
    #[error("{0}")]
    Message(String),
}

pub type AppResult<T> = Result<T, AppError>;

impl AppError {
    pub fn msg(s: impl Into<String>) -> Self {
        AppError::Message(s.into())
    }
}

/// 字节流调试格式化：控制字符转义（<ESC>/<BEL>/\xHH），便于日志观察终端查询/应答
pub fn escape_bytes(b: &[u8]) -> String {
    let mut s = String::with_capacity(b.len() * 4);
    for &c in b {
        match c {
            0x1b => s.push_str("<ESC>"),
            0x07 => s.push_str("<BEL>"),
            0x20..=0x7e => s.push(c as char),
            _ => {
                use std::fmt::Write as _;
                let _ = write!(s, "\\x{c:02x}");
            }
        }
    }
    s
}

/// 输出块中是否包含终端查询形状的序列（供客户端日志嗅探：CPR/DA1/DA2/OSC）
pub fn looks_like_query(b: &[u8]) -> bool {
    let patterns: [&[u8]; 6] = [
        b"\x1b[6n",
        b"\x1b[c",
        b"\x1b[>c",
        b"\x1b[>0c",
        b"\x1b]10;",
        b"\x1b]11;",
    ];
    patterns
        .iter()
        .any(|p| !p.is_empty() && b.windows(p.len()).any(|w| w == &p[..]))
}
