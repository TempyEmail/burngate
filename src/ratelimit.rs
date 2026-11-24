use std::collections::HashMap;
use std::net::{IpAddr, Ipv6Addr};
use std::sync::{Arc, Mutex};

/// Number of entries before stale-entry eviction is considered.
const CLEANUP_THRESHOLD: usize = 10_000;

/// Normalize an IP into the key used for per-client limits.
///
/// IPv4-mapped IPv6 addresses become plain IPv4, and other IPv6 addresses
/// are grouped by their /64 prefix. A single host usually gets a whole /64,
/// so limiting per full IPv6 address is trivial to evade.
pub fn client_key(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V4(_) => ip,
        IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return IpAddr::V4(v4);
            }
            let s = v6.segments();
            IpAddr::V6(Ipv6Addr::new(s[0], s[1], s[2], s[3], 0, 0, 0, 0))
        }
    }
}

/// Per-client limit on new connections, counted in fixed 60-second windows.
pub struct IpRateLimiter {
    inner: Mutex<RateState>,
    max_per_ip: u32,
    window: std::time::Duration,
}

struct RateState {
    map: HashMap<IpAddr, (u32, tokio::time::Instant)>,
    last_cleanup: tokio::time::Instant,
}

impl IpRateLimiter {
    pub fn new(max_per_ip: u32) -> Self {
        Self {
            inner: Mutex::new(RateState {
                map: HashMap::new(),
                last_cleanup: tokio::time::Instant::now(),
            }),
            max_per_ip,
            window: std::time::Duration::from_secs(60),
        }
    }

    /// Returns true if the IP is allowed, false if rate-limited.
    pub async fn check_and_increment(&self, ip: IpAddr) -> bool {
        self.check(ip)
    }

    /// Synchronous version of [`check_and_increment`](Self::check_and_increment).
    pub fn check(&self, ip: IpAddr) -> bool {
        let ip = client_key(ip);
        let now = tokio::time::Instant::now();
        let mut state = self.inner.lock().unwrap_or_else(|e| e.into_inner());

        // Evict stale entries when the map grows large, at most once per
        // window so a flood of live entries can't force a full scan per call.
        if state.map.len() > CLEANUP_THRESHOLD
            && now.duration_since(state.last_cleanup) >= self.window
        {
            let window = self.window;
            state
                .map
                .retain(|_, (_, started)| now.duration_since(*started) < window);
            state.last_cleanup = now;
        }

        let entry = state.map.entry(ip).or_insert((0, now));
        // Start a new window if the current one has expired
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

/// Per-client limit on simultaneous connections.
pub struct IpConcurrencyLimiter {
    map: Mutex<HashMap<IpAddr, usize>>,
    max_per_ip: usize,
}

/// Releases one concurrent-connection slot when dropped.
pub struct ConcurrencyGuard {
    limiter: Arc<IpConcurrencyLimiter>,
    key: IpAddr,
}

impl IpConcurrencyLimiter {
    pub fn new(max_per_ip: usize) -> Arc<Self> {
        Arc::new(Self {
            map: Mutex::new(HashMap::new()),
            max_per_ip,
        })
    }

    /// Take a slot for this IP, or `None` if it already has the maximum open.
    pub fn try_acquire(self: &Arc<Self>, ip: IpAddr) -> Option<ConcurrencyGuard> {
        let key = client_key(ip);
        let mut map = self.map.lock().unwrap_or_else(|e| e.into_inner());
        let count = map.entry(key).or_insert(0);
        if *count >= self.max_per_ip {
            return None;
        }
        *count += 1;
        Some(ConcurrencyGuard {
            limiter: self.clone(),
            key,
        })
    }

    /// Number of open connections currently counted for this IP.
    pub fn current(&self, ip: IpAddr) -> usize {
        let map = self.map.lock().unwrap_or_else(|e| e.into_inner());
        map.get(&client_key(ip)).copied().unwrap_or(0)
    }
}

impl Drop for ConcurrencyGuard {
    fn drop(&mut self) {
        let mut map = self.limiter.map.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(count) = map.get_mut(&self.key) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                map.remove(&self.key);
            }
        }
    }
}
