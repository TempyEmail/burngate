use std::sync::atomic::Ordering;
use std::sync::Arc;

use redis::Client;
use tokio::net::TcpListener;
use tracing::{error, info, warn};
use tracing_subscriber::EnvFilter;

use burngate::config::Config;
use burngate::lookup::MailboxLookup;
use burngate::session::Metrics;
use burngate::tls::TlsConfig;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Initialize structured logging
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .json()
        .init();

    let config = Config::from_env();

    info!(
        listen = %config.listen_addr,
        backend = %config.backend_addr,
        domains = ?config.accepted_domains,
        tls = config.tls_available(),
        "starting burngate"
    );

    // Connect to Redis
    let redis_client = Client::open(config.redis_url.as_str())?;
    let conn_manager = redis::aio::ConnectionManager::new(redis_client).await?;
    let lookup = MailboxLookup::new(conn_manager, &config);
    info!(
        key_pattern = %config.redis_key_pattern,
        set_name = %config.redis_set_name,
        check_mode = ?config.redis_check_mode,
        "connected to Redis"
    );

    // Load TLS config if available
    let tls_config = if config.tls_available() {
        let cert = config.tls_cert_path.as_ref().unwrap();
        let key = config.tls_key_path.as_ref().unwrap();
        match TlsConfig::load(cert, key) {
            Ok(cfg) => {
                info!("STARTTLS enabled");
                Some(cfg)
            }
            Err(e) => {
                warn!(error = %e, "failed to load TLS config, STARTTLS disabled");
                None
            }
        }
    } else {
        info!("STARTTLS disabled (no TLS_CERT_PATH / TLS_KEY_PATH)");
        None
    };

    let config = Arc::new(config);
    let metrics = Arc::new(Metrics::new());

    // Spawn metrics reporter (disabled when METRICS_INTERVAL=0)
    if config.metrics_interval_secs > 0 {
        let metrics_clone = metrics.clone();
        let interval_secs = config.metrics_interval_secs;
        tokio::spawn(async move {
            let mut interval =
                tokio::time::interval(tokio::time::Duration::from_secs(interval_secs));
            loop {
                interval.tick().await;
                info!(
                    accepted = metrics_clone.accepted.load(Ordering::Relaxed),
                    rejected = metrics_clone.rejected.load(Ordering::Relaxed),
                    connections = metrics_clone.connections.load(Ordering::Relaxed),
                    relay_errors = metrics_clone.relay_errors.load(Ordering::Relaxed),
                    "[METRICS]"
                );
            }
        });
    }

    // Bind and accept connections
    let listener = TcpListener::bind(config.listen_addr).await?;
    info!(addr = %config.listen_addr, "listening for SMTP connections");

    loop {
        let (stream, peer_addr) = match listener.accept().await {
            Ok(conn) => conn,
            Err(e) => {
                error!(error = %e, "accept error");
                continue;
            }
        };

        let config = config.clone();
        let lookup = lookup.clone();
        let tls_config = tls_config.clone();
        let metrics = metrics.clone();

        tokio::spawn(async move {
            burngate::session::handle_session(
                stream, peer_addr, config, lookup, tls_config, metrics,
            )
            .await;
        });
    }
}
