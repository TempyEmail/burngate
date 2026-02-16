use std::collections::HashMap;
use std::net::IpAddr;

use tokio::sync::Mutex;

/// Per-IP connection tracking with a sliding window.
pub struct IpRateLimiter {
    map: Mutex<HashMap<IpAddr, (u32, tokio::time::Instant)>>,
    max_per_ip: u32,
    window: std::time::Duration,
}

impl IpRateLimiter {
    pub fn new(max_per_ip: u32) -> Self {
        Self {
            map: Mutex::new(HashMap::new()),
            max_per_ip,
            window: std::time::Duration::from_secs(60),
        }
    }

    /// Returns true if the IP is allowed, false if rate-limited.
    pub async fn check_and_increment(&self, ip: IpAddr) -> bool {
        let now = tokio::time::Instant::now();
        let mut map = self.map.lock().await;
        let entry = map.entry(ip).or_insert((0, now));
        // Reset window if expired
        if now.duration_since(entry.1) >= self.window {
            entry.0 = 0;
            entry.1 = now;
        }
        if entry.0 >= self.max_per_ip {
            return false;
        }
        entry.0 += 1;
        true
    }
}
