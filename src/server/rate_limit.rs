use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// 简单滑动窗口限流器：按 (场景, IP) 计数
pub struct RateLimiter {
    buckets: Mutex<HashMap<(&'static str, IpAddr), Vec<Instant>>>,
}

impl RateLimiter {
    pub fn new() -> Self {
        RateLimiter {
            buckets: Mutex::new(HashMap::new()),
        }
    }

    /// 返回 false 表示超限（应拒绝）
    pub fn check(&self, scene: &'static str, ip: IpAddr, limit: usize, window: Duration) -> bool {
        let now = Instant::now();
        let mut buckets = self.buckets.lock().unwrap();
        let hits = buckets.entry((scene, ip)).or_default();
        hits.retain(|t| now.duration_since(*t) < window);
        if hits.len() >= limit {
            return false;
        }
        hits.push(now);
        true
    }
}

pub const REGISTER_LIMIT: usize = 10;
pub const REGISTER_WINDOW: Duration = Duration::from_secs(60);
pub const LOGIN_LIMIT: usize = 5;
pub const LOGIN_WINDOW: Duration = Duration::from_secs(300);
