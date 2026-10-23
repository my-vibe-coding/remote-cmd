use std::io::Write;
use std::path::Path;

use serde::de::DeserializeOwned;
use serde::Serialize;

use super::AppResult;

/// 读取 JSON 文件；文件不存在时返回 None
pub fn load_json<T: DeserializeOwned>(path: &Path) -> AppResult<Option<T>> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(Some(serde_json::from_slice(&bytes)?)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// 原子写入 JSON：先写同目录临时文件，再 persist 覆盖目标（Windows 下 rename 不能覆盖已存在文件）
pub fn save_json_atomic<T: Serialize>(path: &Path, value: &T) -> AppResult<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let bytes = serde_json::to_vec_pretty(value)?;
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let tmp = tempfile::NamedTempFile::new_in(dir)?;
    (&tmp).write_all(&bytes)?;
    match tmp.persist(path) {
        Ok(_) => Ok(()),
        Err(first) => {
            // Windows 下安全软件可能短暂占用新建临时文件导致 persist 报拒绝访问：稍候重试
            std::thread::sleep(std::time::Duration::from_millis(100));
            match first.file.persist(path) {
                Ok(_) => Ok(()),
                Err(second) => {
                    macro_log::w!("原子写入失败，退化为直接覆盖: {}", second.error);
                    std::fs::write(path, &bytes)?;
                    Ok(())
                }
            }
        }
    }
}

/// 追加一行 JSON 到 jsonl 文件
pub fn append_jsonl<T: Serialize>(path: &Path, value: &T) -> AppResult<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut file = std::fs::OpenOptions::new().create(true).append(true).open(path)?;
    let line = serde_json::to_string(value)?;
    file.write_all(line.as_bytes())?;
    file.write_all(b"\n")?;
    Ok(())
}

/// 读取 jsonl 文件全部行；不存在返回空
pub fn load_jsonl<T: DeserializeOwned>(path: &Path) -> AppResult<Vec<T>> {
    let mut out = vec![];
    let Ok(content) = std::fs::read_to_string(path) else {
        return Ok(out);
    };
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        out.push(serde_json::from_str(line)?);
    }
    Ok(out)
}
