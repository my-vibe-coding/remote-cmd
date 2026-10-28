use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::sync::Semaphore;

use super::rate_limit::RateLimiter;
use super::store::{Audit, Store};

pub const SESSION_TTL: Duration = Duration::from_secs(12 * 3600);

pub struct AppState {
    #[allow(dead_code)]
    pub data_dir: PathBuf,
    pub store: Store,
    pub audit: Arc<Audit>,
    /// 服务器自身主机名：注册响应 server_hostname 字段回传，客户端用作服务器显示名
    pub server_hostname: String,
    /// 管理员密码哈希（argon2）
    pub admin_password_hash: String,
    /// 会话 token -> 过期时间
    pub sessions: Mutex<HashMap<String, Instant>>,
    pub rate: RateLimiter,
    /// 全局并发执行上限
    pub global_sem: Arc<Semaphore>,
    /// 每凭证并发执行上限
    pub per_client_sem: Mutex<HashMap<String, Arc<Semaphore>>>,
}

impl AppState {
    pub const GLOBAL_EXEC_LIMIT: usize = 8;
    pub const PER_CLIENT_EXEC_LIMIT: usize = 2;

    pub fn new(data_dir: PathBuf, admin_password_hash: String, store: Store, audit: Audit) -> Self {
        AppState {
            data_dir,
            store,
            audit: Arc::new(audit),
            server_hostname: crate::common::fingerprint::local_hostname(),
            admin_password_hash,
            sessions: Mutex::new(HashMap::new()),
            rate: RateLimiter::new(),
            global_sem: Arc::new(Semaphore::new(Self::GLOBAL_EXEC_LIMIT)),
            per_client_sem: Mutex::new(HashMap::new()),
        }
    }

    pub fn create_session(&self) -> String {
        let token = crate::common::crypto::random_token();
        self.sessions
            .lock()
            .unwrap()
            .insert(token.clone(), Instant::now() + SESSION_TTL);
        token
    }

    pub fn session_valid(&self, token: &str) -> bool {
        let mut sessions = self.sessions.lock().unwrap();
        sessions.retain(|_, exp| *exp > Instant::now());
        sessions.contains_key(token)
    }

    pub fn per_client(&self, client_id: &str) -> Arc<Semaphore> {
        self.per_client_sem
            .lock()
            .unwrap()
            .entry(client_id.to_string())
            .or_insert_with(|| Arc::new(Semaphore::new(Self::PER_CLIENT_EXEC_LIMIT)))
            .clone()
    }
}
