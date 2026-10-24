use std::path::PathBuf;

/// 服务端数据目录：默认 %USERPROFILE%\.remote-cmd\server
pub fn server_data_dir(custom: Option<&str>) -> PathBuf {
    if let Some(dir) = custom {
        return PathBuf::from(dir);
    }
    base_dir().join("server")
}

/// 客户端数据目录：%USERPROFILE%\.remote-cmd\client
pub fn client_data_dir() -> PathBuf {
    base_dir().join("client")
}

fn base_dir() -> PathBuf {
    // REMOTE_CMD_HOME 可覆盖数据根目录（用于多实例/测试隔离）
    if let Ok(dir) = std::env::var("REMOTE_CMD_HOME") {
        if !dir.trim().is_empty() {
            return PathBuf::from(dir);
        }
    }
    if let Some(home) = dirs::home_dir() {
        return home.join(".remote-cmd");
    }
    PathBuf::from(".remote-cmd")
}
