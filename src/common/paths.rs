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
    if let Some(home) = dirs::home_dir() {
        return home.join(".remote-cmd");
    }
    PathBuf::from(".remote-cmd")
}
