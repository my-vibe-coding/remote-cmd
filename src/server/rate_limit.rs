use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// 所有场景中最大的窗口（用于清扫过期桶）
const MAX_WINDOW: Duration = Duration::from_secs(300);
/// 桶数量达到该阈值时触发一次全量清扫，防止海量伪造 IP 撑大内存
const SWEEP_THRESHOLD: usize = 4096;

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
        // 桶数量过大时全量清扫过期条目（不同场景窗口不同，统一按最大窗口保守保留）
        if buckets.len() >= SWEEP_THRESHOLD {
            buckets.retain(|_, hits| hits.iter().any(|t| now.duration_since(*t) < MAX_WINDOW));
        }
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
