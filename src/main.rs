use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use redis::aio::ConnectionManagerConfig;
use redis::Client;
use tokio::io::AsyncWriteExt;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, Semaphore};
use tracing::{error, info, warn};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::EnvFilter;

use burngate::config::Config;
use burngate::lookup::MailboxLookup;
use burngate::ratelimit::{IpConcurrencyLimiter, IpRateLimiter};
use burngate::session::Metrics;
use burngate::tls::TlsConfig;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Initialize structured logging, with optional OpenTelemetry OTLP export.
    // Set OTEL_EXPORTER_OTLP_ENDPOINT to enable (e.g. http://localhost:15901 for Aspire).
    let tracer_provider = if std::env::var("OTEL_EXPORTER_OTLP_ENDPOINT").is_ok() {
        let service_name = std::env::var("OTEL_SERVICE_NAME").unwrap_or_else(|_| "burngate".into());
        let exporter = opentelemetry_otlp::SpanExporter::builder()
            .with_tonic()
            .build()
            .map_err(|e| format!("failed to build OTLP exporter: {e}"))?;
        let provider = opentelemetry_sdk::trace::SdkTracerProvider::builder()
            .with_batch_exporter(exporter)
            .with_resource(
                opentelemetry_sdk::Resource::builder()
                    .with_service_name(service_name)
                    .build(),
            )
            .build();
        opentelemetry::global::set_tracer_provider(provider.clone());
        Some(provider)
    } else {
        None
    };
    let otel_layer = tracer_provider.as_ref().map(|provider| {
        use opentelemetry::trace::TracerProvider as _;
        tracing_opentelemetry::layer().with_tracer(provider.tracer("burngate"))
    });

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

    // Connect to Redis. Bound every command so a hung Redis turns into a
    // temporary failure (451) instead of stalling the session.
    let redis_timeout = Duration::from_millis(config.redis_timeout_ms.max(1));
    let redis_client = Client::open(config.redis_url.as_str())?;
    let conn_manager = redis::aio::ConnectionManager::new_with_config(
        redis_client,
        ConnectionManagerConfig::new()
            .set_response_timeout(Some(redis_timeout))
            .set_connection_timeout(Some(redis_timeout)),
    )
    .await?;
    let lookup = MailboxLookup::new(conn_manager, &config);
    info!(
        key_pattern = %config.redis_key_pattern,
        set_name = %config.redis_set_name,
        check_mode = ?config.redis_check_mode,
        "connected to Redis"
    );

    // TLS: loaded now, reloaded on SIGHUP and every TLS_RELOAD_INTERVAL seconds.
    // A failed load keeps the previous certificate.
    let tls_config = match (&config.tls_cert_path, &config.tls_key_path) {
        (Some(cert), Some(key)) => {
            let tls = TlsConfig::new(cert, key);
            if tls.is_ready() {
                info!("STARTTLS enabled");
            } else {
                error!(
                    "STARTTLS configured but no certificate could be loaded; will retry on reload"
                );
            }
            spawn_tls_reloader(tls.clone(), config.tls_reload_interval_secs);
            Some(tls)
        }
        _ => {
            info!("STARTTLS disabled (no TLS_CERT_PATH / TLS_KEY_PATH)");
            None
        }
    };

    let config = Arc::new(config);
    let metrics = Arc::new(Metrics::new());

    // Global connection limit (0 = unlimited)
    let total_permits = if config.max_connections > 0 {
        config.max_connections
    } else {
        Semaphore::MAX_PERMITS
    };
    let semaphore = Arc::new(Semaphore::new(total_permits));

    // Per-IP limits (None if disabled)
    let rate_limiter = (config.max_connections_per_ip > 0)
        .then(|| IpRateLimiter::new(config.max_connections_per_ip));
    let concurrency_limiter = (config.max_concurrent_per_ip > 0)
        .then(|| IpConcurrencyLimiter::new(config.max_concurrent_per_ip));

    // Spawn metrics reporter (disabled when METRICS_INTERVAL=0)
    if config.metrics_interval_secs > 0 {
        let metrics = metrics.clone();
        let interval_secs = config.metrics_interval_secs;
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(interval_secs));
            loop {
                interval.tick().await;
                info!(
                    accepted = metrics.accepted.load(Ordering::Relaxed),
                    rejected = metrics.rejected.load(Ordering::Relaxed),
                    deferred = metrics.deferred.load(Ordering::Relaxed),
                    connections = metrics.connections.load(Ordering::Relaxed),
                    relay_errors = metrics.relay_errors.load(Ordering::Relaxed),
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
        max_concurrent_per_ip = config.max_concurrent_per_ip,
        "listening for SMTP connections"
    );

    // Every session holds a clone of `alive_tx`; once they are all dropped,
    // `alive_rx.recv()` returns None, so we know every session has finished.
    let (alive_tx, mut alive_rx) = mpsc::channel::<()>(1);
    let shutdown = shutdown_signal();
    tokio::pin!(shutdown);

    loop {
        let (stream, peer_addr) = tokio::select! {
            _ = &mut shutdown => break,
            accepted = listener.accept() => match accepted {
                Ok(conn) => conn,
                Err(e) => {
                    error!(error = %e, "accept error");
                    // Back off briefly (e.g. out of file descriptors)
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    continue;
                }
            },
        };

        // Per-IP rate of new connections
        if let Some(limiter) = &rate_limiter {
            if !limiter.check(peer_addr.ip()) {
                warn!(peer = %peer_addr, "per-IP rate limit exceeded, rejecting");
                reject(
                    stream,
                    "421 4.7.0 Too many connections from your IP, try again later",
                );
                continue;
            }
        }

        // Per-IP simultaneous connections
        let ip_guard = match &concurrency_limiter {
            Some(limiter) => match limiter.try_acquire(peer_addr.ip()) {
                Some(guard) => Some(guard),
                None => {
                    warn!(peer = %peer_addr, "per-IP concurrency limit reached, rejecting");
                    reject(
                        stream,
                        "421 4.7.0 Too many concurrent connections from your IP",
                    );
                    continue;
                }
            },
            None => None,
        };

        // Global connection limit: tell the client to come back later rather
        // than leaving it hanging in the accept queue.
        let permit = match semaphore.clone().try_acquire_owned() {
            Ok(permit) => permit,
            Err(_) => {
                warn!(peer = %peer_addr, "connection limit reached, rejecting");
                reject(stream, "421 4.3.2 Too busy, try again later");
                continue;
            }
        };

        let config = config.clone();
        let lookup = lookup.clone();
        let tls_config = tls_config.clone();
        let metrics = metrics.clone();
        let alive = alive_tx.clone();

        tokio::spawn(async move {
            burngate::session::handle_session(
                stream, peer_addr, config, lookup, tls_config, metrics,
            )
            .await;
            drop((permit, ip_guard, alive));
        });
    }

    // Graceful shutdown: stop accepting, let in-flight sessions finish.
    drop(listener);
    drop(alive_tx);
    let in_flight = total_permits - semaphore.available_permits();
    info!(
        in_flight,
        timeout_secs = config.shutdown_timeout_secs,
        "shutting down, waiting for sessions to finish"
    );
    let drained = tokio::time::timeout(
        Duration::from_secs(config.shutdown_timeout_secs),
        alive_rx.recv(),
    )
    .await
    .is_ok();
    if drained {
        info!("all sessions finished");
    } else {
        warn!("shutdown timeout reached, dropping remaining sessions");
    }

    if let Some(provider) = tracer_provider {
        if let Err(e) = provider.shutdown() {
            warn!(error = %e, "failed to flush traces");
        }
    }
    Ok(())
}

/// Send a one-line rejection and close, without blocking the accept loop.
fn reject(mut stream: TcpStream, line: &'static str) {
    tokio::spawn(async move {
        let _ = tokio::time::timeout(Duration::from_secs(5), async {
            let _ = stream.write_all(format!("{line}\r\n").as_bytes()).await;
            let _ = stream.shutdown().await;
        })
        .await;
    });
}

/// Resolves on SIGTERM or Ctrl-C.
async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        match signal(SignalKind::terminate()) {
            Ok(mut term) => {
                tokio::select! {
                    _ = term.recv() => info!("received SIGTERM"),
                    _ = tokio::signal::ctrl_c() => info!("received Ctrl-C"),
                }
            }
            Err(e) => {
                warn!(error = %e, "cannot listen for SIGTERM; only Ctrl-C will stop the server");
                let _ = tokio::signal::ctrl_c().await;
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
        info!("received Ctrl-C");
    }
}

/// Reload the TLS certificate on SIGHUP and every `interval_secs` (0 = SIGHUP only).
fn spawn_tls_reloader(tls: TlsConfig, interval_secs: u64) {
    tokio::spawn(async move {
        let period = if interval_secs > 0 {
            Duration::from_secs(interval_secs)
        } else {
            // Effectively never; SIGHUP still works
            Duration::from_secs(u64::from(u32::MAX))
        };
        let mut timer = tokio::time::interval_at(tokio::time::Instant::now() + period, period);

        #[cfg(unix)]
        let mut hup = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup()).ok();

        loop {
            #[cfg(unix)]
            {
                let hup_recv = async {
                    match hup.as_mut() {
                        Some(s) => s.recv().await,
                        None => std::future::pending().await,
                    }
                };
                tokio::select! {
                    _ = timer.tick() => {}
                    _ = hup_recv => info!("received SIGHUP, reloading TLS certificate"),
                }
            }
            #[cfg(not(unix))]
            timer.tick().await;

            tls.reload();
        }
    });
}
