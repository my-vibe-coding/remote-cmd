use std::path::Path;

use rcgen::generate_simple_self_signed;

use crate::common::AppResult;

pub struct TlsPaths {
    pub cert: std::path::PathBuf,
    pub key: std::path::PathBuf,
}

/// 加载或生成自签证书（持久化到数据目录，重启复用）
pub fn ensure_cert(dir: &Path) -> AppResult<TlsPaths> {
    let cert = dir.join("cert.pem");
    let key = dir.join("key.pem");
    if cert.exists() && key.exists() {
        return Ok(TlsPaths { cert, key });
    }
    std::fs::create_dir_all(dir)?;
    let rcgen_cert = generate_simple_self_signed(vec![
        "localhost".into(),
        "127.0.0.1".into(),
        "remote-cmd".into(),
    ])
    .map_err(|e| crate::common::AppError::msg(format!("生成证书失败: {e}")))?;
    std::fs::write(&cert, rcgen_cert.cert.pem())?;
    std::fs::write(&key, rcgen_cert.key_pair.serialize_pem())?;
    macro_log::i!("已生成自签证书: {} / {}", cert.display(), key.display());
    Ok(TlsPaths { cert, key })
}
