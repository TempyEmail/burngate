use std::collections::HashSet;
use std::env;
use std::net::SocketAddr;

/// Gateway configuration loaded from environment variables.
#[derive(Clone)]
pub struct Config {
    /// Address to listen on (e.g. 0.0.0.0:25).
    pub listen_addr: SocketAddr,
    /// Backend SMTP address to relay accepted mail to (e.g. 127.0.0.1:2525).
    pub backend_addr: String,
    /// Redis connection URL.
    pub redis_url: String,
    /// Set of accepted domains (lowercased).
    pub accepted_domains: HashSet<String>,
    /// Maximum message size in bytes (default 10MB).
    pub max_message_size: usize,
    /// Path to TLS certificate file (PEM). If unset, STARTTLS is disabled.
    pub tls_cert_path: Option<String>,
    /// Path to TLS private key file (PEM). If unset, STARTTLS is disabled.
    pub tls_key_path: Option<String>,
    /// Hostname for SMTP banner.
    pub server_name: String,
    /// Connection timeout in seconds.
    pub connection_timeout_secs: u64,
    /// Redis key pattern for active mailbox check. Use `{address}` as placeholder.
    /// Example: `mb:{address}` checks key `mb:user@example.com`.
    pub redis_key_pattern: String,
    /// Redis SET name for the known-addresses fallback check.
    /// Set to empty string to disable the fallback check entirely.
    pub redis_set_name: String,
    /// Which Redis checks to perform: "both", "key", or "set".
    pub redis_check_mode: CheckMode,
    /// Metrics reporting interval in seconds. Set to 0 to disable.
    pub metrics_interval_secs: u64,
    /// Maximum concurrent connections. 0 = unlimited.
    pub max_connections: usize,
    /// Maximum RCPT TO recipients per session.
    pub max_recipients: usize,
    /// Maximum line length in bytes for SMTP command reads.
    pub max_line_length: usize,
    /// Maximum connections per IP address per sliding window. 0 = disabled.
    pub max_connections_per_ip: u32,
}

/// Which Redis checks to perform for mailbox existence.
#[derive(Clone, Debug, PartialEq)]
pub enum CheckMode {
    /// Check key first, then fall back to set (default).
    Both,
    /// Only check the key (EXISTS).
    KeyOnly,
    /// Only check the set (SISMEMBER).
    SetOnly,
}

impl Config {
    /// Load configuration from environment variables.
    pub fn from_env() -> Self {
        let listen_addr = env::var("LISTEN_ADDR")
            .unwrap_or_else(|_| "0.0.0.0:25".to_string())
            .parse()
            .expect("LISTEN_ADDR must be a valid socket address");

        let backend_addr =
            env::var("BACKEND_SMTP").unwrap_or_else(|_| "127.0.0.1:2525".to_string());

        // Build Redis URL from individual vars or REDIS_URL
        let redis_url = if let Ok(url) = env::var("REDIS_URL") {
            url
        } else {
            let host = env::var("REDIS_HOST").unwrap_or_else(|_| "127.0.0.1".to_string());
            let port = env::var("REDIS_PORT").unwrap_or_else(|_| "6379".to_string());
            let user = env::var("REDIS_USERNAME").unwrap_or_default();
            let pass = env::var("REDIS_PASSWORD").unwrap_or_default();

            if !user.is_empty() && !pass.is_empty() {
                format!("redis://{}:{}@{}:{}", user, pass, host, port)
            } else if !pass.is_empty() {
                format!("redis://:{}@{}:{}", pass, host, port)
            } else {
                format!("redis://{}:{}", host, port)
            }
        };

        let accepted_domains: HashSet<String> = env::var("ACCEPTED_DOMAINS")
            .map(|val| {
                val.split(',')
                    .map(|s| s.trim().to_lowercase())
                    .filter(|s| !s.is_empty())
                    .collect()
            })
            .expect("ACCEPTED_DOMAINS is required (comma-separated list of domains)");

        let max_message_size = env::var("MAX_MESSAGE_SIZE")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(10 * 1024 * 1024); // 10MB

        let tls_cert_path = env::var("TLS_CERT_PATH").ok();
        let tls_key_path = env::var("TLS_KEY_PATH").ok();

        let server_name = env::var("SERVER_NAME").unwrap_or_else(|_| "burngate".to_string());

        let connection_timeout_secs = env::var("CONNECTION_TIMEOUT")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(300); // 5 minutes

        // Redis key/set configuration
        let redis_key_pattern =
            env::var("REDIS_KEY_PATTERN").unwrap_or_else(|_| "mb:{address}".to_string());

        let redis_set_name = env::var("REDIS_SET_NAME").unwrap_or_else(|_| "addresses".to_string());

        let redis_check_mode = match env::var("REDIS_CHECK_MODE")
            .unwrap_or_else(|_| "both".to_string())
            .to_lowercase()
            .as_str()
        {
            "key" | "key_only" => CheckMode::KeyOnly,
            "set" | "set_only" => CheckMode::SetOnly,
            _ => CheckMode::Both,
        };

        let metrics_interval_secs = env::var("METRICS_INTERVAL")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(60);

        let max_connections = env::var("MAX_CONNECTIONS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(1000);

        let max_recipients = env::var("MAX_RECIPIENTS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(100);

        let max_line_length = env::var("MAX_LINE_LENGTH")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(1024);

        let max_connections_per_ip = env::var("MAX_CONNECTIONS_PER_IP")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(0); // disabled by default

        Config {
            listen_addr,
            backend_addr,
            redis_url,
            accepted_domains,
            max_message_size,
            tls_cert_path,
            tls_key_path,
            server_name,
            connection_timeout_secs,
            redis_key_pattern,
            redis_set_name,
            redis_check_mode,
            metrics_interval_secs,
            max_connections,
            max_recipients,
            max_line_length,
            max_connections_per_ip,
        }
    }

    /// Build a Redis key for the given address using the configured pattern.
    pub fn redis_key_for(&self, address: &str) -> String {
        self.redis_key_pattern
            .replace("{address}", &address.to_lowercase())
    }

    /// Check if STARTTLS is available (both cert and key configured).
    pub fn tls_available(&self) -> bool {
        self.tls_cert_path.is_some() && self.tls_key_path.is_some()
    }
}
