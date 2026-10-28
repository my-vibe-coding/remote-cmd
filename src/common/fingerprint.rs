use sha2::{Digest, Sha256};

use super::AppResult;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct MachineInfo {
    pub hostname: String,
    pub ip: String,
    pub os: String,
    pub machine_guid: String,
    pub user: String,
}

impl MachineInfo {
    /// 采集本机信息：hostname（COMPUTERNAME）、本机 IP、OS、MachineGuid（Windows）
    pub fn collect() -> Self {
        let hostname = local_hostname();
        let user = std::env::var("USERNAME").unwrap_or_else(|_| "unknown".into());
        let ip = local_ip().unwrap_or_else(|| "0.0.0.0".into());
        MachineInfo {
            hostname,
            ip,
            os: std::env::consts::OS.to_string(),
            machine_guid: machine_guid(),
            user,
        }
    }

    /// 归一化指纹：SHA256(guid|hostname|ip|os)，防重复注册/伪造凭证领取
    pub fn fingerprint(&self) -> String {
        let raw = format!(
            "{}|{}|{}|{}",
            self.machine_guid.trim().to_lowercase(),
            self.hostname.trim().to_lowercase(),
            self.ip.trim(),
            self.os.trim().to_lowercase()
        );
        hex_sha256(raw.as_bytes())
    }
}

pub fn hex_sha256(data: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(data);
    let out = hasher.finalize();
    out.iter().map(|b| format!("{b:02x}")).collect()
}

/// 本机主机名：Windows 取 COMPUTERNAME；Unix 依次尝试 /proc/sys/kernel/hostname、
/// /etc/hostname、$HOSTNAME。服务器在注册响应中以此回传自己的显示名（server_hostname）。
pub fn local_hostname() -> String {
    #[cfg(windows)]
    {
        std::env::var("COMPUTERNAME").unwrap_or_else(|_| "unknown".into())
    }
    #[cfg(not(windows))]
    {
        std::fs::read_to_string("/proc/sys/kernel/hostname")
            .or_else(|_| std::fs::read_to_string("/etc/hostname"))
            .map(|s| s.trim().to_string())
            .ok()
            .filter(|s| !s.is_empty())
            .or_else(|| std::env::var("HOSTNAME").ok())
            .unwrap_or_else(|| "unknown".into())
    }
}

/// 机器唯一标识：Windows 读注册表 MachineGuid；Linux 依次尝试 /etc/machine-id、
/// /var/lib/dbus/machine-id（与 Windows MachineGuid 对等，重装系统才会变化），
/// 再退化为 $HOSTNAME。该值是指纹四分量之一，直接影响幂等注册与凭证领取。
/// 注意：改变取值来源会使既有 Linux 客户端的指纹变化，重新注册时被视为新机器。
fn machine_guid() -> String {
    #[cfg(windows)]
    {
        if let Ok(out) = std::process::Command::new("reg")
            .args(["query", r"HKLM\SOFTWARE\Microsoft\Cryptography", "/v", "MachineGuid"])
            .output()
        {
            let text = String::from_utf8_lossy(&out.stdout);
            for line in text.lines() {
                if let Some(idx) = line.find("MachineGuid")
                    && let Some(val) = line[idx + "MachineGuid".len()..].split_whitespace().last() {
                        return val.to_string();
                    }
            }
        }
        // reg query 失败时退化为 hostname，保证仍可用
        std::env::var("COMPUTERNAME").unwrap_or_else(|_| "unknown-guid".into())
    }
    #[cfg(not(windows))]
    {
        std::fs::read_to_string("/etc/machine-id")
            .or_else(|_| std::fs::read_to_string("/var/lib/dbus/machine-id"))
            .map(|s| s.trim().to_string())
            .ok()
            .filter(|s| !s.is_empty())
            .or_else(|| std::env::var("HOSTNAME").ok())
            .unwrap_or_else(|| "unknown-guid".into())
    }
}

/// 通过 UDP connect 技巧获取本机出口 IP（不真正发包）
fn local_ip() -> Option<String> {
    let socket = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect("8.8.8.8:80").ok()?;
    Some(socket.local_addr().ok()?.ip().to_string())
}

/// 简化校验：字段非空
pub fn validate_info(info: &MachineInfo) -> AppResult<()> {
    if info.hostname.trim().is_empty() || info.machine_guid.trim().is_empty() {
        return Err(super::AppError::msg("hostname 与 machine_guid 不能为空"));
    }
    Ok(())
}
