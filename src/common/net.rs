/// 通过 ipv4.ip.sb 获取公网 IPv4 地址，失败返回 None
pub async fn fetch_public_ipv4() -> Option<String> {
    let resp = reqwest::Client::new()
        .get("https://ipv4.ip.sb")
        .header("User-Agent", "curl/8.0")
        .timeout(std::time::Duration::from_secs(5))
        .send()
        .await
        .ok()?;
    let text = resp.text().await.ok()?;
    let ip = text.trim();
    if ip.is_empty() { None } else { Some(ip.to_string()) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_fetch_public_ipv4() {
        match fetch_public_ipv4().await {
            Some(ip) => println!("公网 IPv4: {ip}"),
            None => println!("获取公网 IPv4 失败"),
        }
    }
}
