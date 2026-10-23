use rand::distributions::Alphanumeric;
use rand::Rng;

const TOKEN_LEN: usize = 64;

/// 生成随机凭证/会话 token（hex 风格字母数字）
pub fn random_token() -> String {
    rand::thread_rng()
        .sample_iter(&Alphanumeric)
        .take(TOKEN_LEN)
        .map(char::from)
        .collect()
}

/// 生成人类易读的随机密码（避开易混淆字符）
pub fn random_password(len: usize) -> String {
    const CHARS: &str = "abcdefghjkmnpqrstuvwxyzABCDEFGHJKMNPQRSTUVWXYZ23456789";
    let mut rng = rand::thread_rng();
    (0..len)
        .map(|_| {
            let idx = rng.gen_range(0..CHARS.len());
            CHARS.as_bytes()[idx] as char
        })
        .collect()
}

/// argon2id 哈希密码
pub fn hash_password(password: &str) -> anyhow::Result<String> {
    use argon2::password_hash::{rand_core::OsRng, PasswordHasher, SaltString};
    use argon2::Argon2;
    let salt = SaltString::generate(&mut OsRng);
    let hash = Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map_err(|e| anyhow::anyhow!("密码哈希失败: {e}"))?;
    Ok(hash.to_string())
}

/// 校验密码
pub fn verify_password(password: &str, hash: &str) -> bool {
    use argon2::password_hash::PasswordVerifier;
    use argon2::{Argon2, PasswordHash};
    let Ok(parsed) = PasswordHash::new(hash) else {
        return false;
    };
    Argon2::default()
        .verify_password(password.as_bytes(), &parsed)
        .is_ok()
}
