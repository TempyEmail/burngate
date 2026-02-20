use std::sync::atomic::Ordering;
use std::sync::Arc;

use redis::Client;
use tokio::net::TcpListener;
use tokio::sync::Semaphore;
use tracing::{error, info, warn};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::EnvFilter;

use burngate::config::Config;
use burngate::lookup::MailboxLookup;
use burngate::ratelimit::IpRateLimiter;
use burngate::session::Metrics;
use burngate::tls::TlsConfig;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Initialize structured logging, with optional OpenTelemetry OTLP export.
    // Set OTEL_EXPORTER_OTLP_ENDPOINT to enable (e.g. http://localhost:15901 for Aspire).
    let otel_layer = if std::env::var("OTEL_EXPORTER_OTLP_ENDPOINT").is_ok() {
        let service_name = std::env::var("OTEL_SERVICE_NAME").unwrap_or_else(|_| "burngate".into());
        let exporter = opentelemetry_otlp::SpanExporter::builder()
            .with_tonic()
            .build()
            .map_err(|e| format!("failed to build OTLP exporter: {e}"))?;
        let provider = opentelemetry_sdk::trace::TracerProvider::builder()
            .with_batch_exporter(exporter, opentelemetry_sdk::runtime::Tokio)
            .with_resource(opentelemetry_sdk::Resource::new(vec![
                opentelemetry::KeyValue::new("service.name", service_name),
            ]))
            .build();
        opentelemetry::global::set_tracer_provider(provider.clone());
        #[allow(clippy::disallowed_methods)]
        let tracer = {
            use opentelemetry::trace::TracerProvider as _;
            provider.tracer("burngate")
        };
        Some(tracing_opentelemetry::layer().with_tracer(tracer))
    } else {
        None
    };

    tracing_subscriber::registry()
        .with(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .with(tracing_subscriber::fmt::layer().json())
        .with(otel_layer)
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

    // Connection semaphore (0 = unlimited, use a very large value)
    let semaphore = Arc::new(Semaphore::new(if config.max_connections > 0 {
        config.max_connections
    } else {
        Semaphore::MAX_PERMITS
    }));

    // Per-IP rate limiter (None if disabled)
    let rate_limiter = if config.max_connections_per_ip > 0 {
        Some(Arc::new(IpRateLimiter::new(config.max_connections_per_ip)))
    } else {
        None
    };

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
    info!(
        addr = %config.listen_addr,
        max_connections = config.max_connections,
        max_connections_per_ip = config.max_connections_per_ip,
        "listening for SMTP connections"
    );

    loop {
        let (stream, peer_addr) = match listener.accept().await {
            Ok(conn) => conn,
            Err(e) => {
                error!(error = %e, "accept error");
                continue;
            }
        };

        // Per-IP rate limiting
        if let Some(ref limiter) = rate_limiter {
            if !limiter.check_and_increment(peer_addr.ip()).await {
                warn!(peer = %peer_addr, "per-IP rate limit exceeded, rejecting");
                // Send 421 and close — best-effort, ignore errors
                use tokio::io::AsyncWriteExt;
                let mut stream = stream;
                let _ = stream
                    .write_all(b"421 4.7.0 Too many connections from your IP\r\n")
                    .await;
                let _ = stream.shutdown().await;
                continue;
            }
        }

        // Acquire connection semaphore permit
        let permit = match semaphore.clone().acquire_owned().await {
            Ok(permit) => permit,
            Err(_) => {
                error!("connection semaphore closed");
                break;
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
            // Permit is dropped here, releasing the semaphore slot
            drop(permit);
        });
    }

    opentelemetry::global::shutdown_tracer_provider();
    Ok(())
}
