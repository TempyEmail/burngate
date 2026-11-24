use std::sync::{Arc, RwLock};

use rustls::ServerConfig;
use rustls_pki_types::pem::PemObject;
use rustls_pki_types::{CertificateDer, PrivateKeyDer};
use tokio_rustls::TlsAcceptor;
use tracing::{error, info};

/// Reloadable TLS configuration for STARTTLS.
///
/// The certificate is read from disk at startup and can be re-read at any
/// time with [`reload`](Self::reload) (on SIGHUP and on a timer), so renewed
/// certificates are picked up without a restart. If a reload fails, the
/// previous certificate stays in use. If no certificate has ever loaded,
/// STARTTLS is not advertised until one does.
#[derive(Clone)]
pub struct TlsConfig {
    cert_path: Arc<str>,
    key_path: Arc<str>,
    acceptor: Arc<RwLock<Option<TlsAcceptor>>>,
}

impl TlsConfig {
    /// Create the config and attempt an initial load. Never fails: a load
    /// error is logged and retried on the next reload.
    pub fn new(cert_path: &str, key_path: &str) -> Self {
        let cfg = Self {
            cert_path: cert_path.into(),
            key_path: key_path.into(),
            acceptor: Arc::new(RwLock::new(None)),
        };
        cfg.reload();
        cfg
    }

    /// Load TLS configuration from PEM certificate and key files.
    pub fn load(cert_path: &str, key_path: &str) -> Result<Self, Box<dyn std::error::Error>> {
        let acceptor = build_acceptor(cert_path, key_path)?;
        Ok(Self {
            cert_path: cert_path.into(),
            key_path: key_path.into(),
            acceptor: Arc::new(RwLock::new(Some(acceptor))),
        })
    }

    /// Re-read the certificate and key from disk. Returns true on success.
    pub fn reload(&self) -> bool {
        match build_acceptor(&self.cert_path, &self.key_path) {
            Ok(acceptor) => {
                *self.acceptor.write().unwrap_or_else(|e| e.into_inner()) = Some(acceptor);
                info!(cert = %self.cert_path, key = %self.key_path, "TLS certificate loaded");
                true
            }
            Err(e) => {
                let have_old = self.is_ready();
                error!(
                    error = %e,
                    cert = %self.cert_path,
                    keeping_previous = have_old,
                    "failed to load TLS certificate"
                );
                false
            }
        }
    }

    /// Whether a certificate is loaded and STARTTLS can be offered.
    pub fn is_ready(&self) -> bool {
        self.acceptor
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .is_some()
    }

    /// Perform the TLS handshake on a plain stream.
    pub async fn accept<S>(
        &self,
        stream: S,
    ) -> Result<tokio_rustls::server::TlsStream<S>, std::io::Error>
    where
        S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
    {
        let acceptor = self
            .acceptor
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
            .ok_or_else(|| std::io::Error::other("no TLS certificate loaded"))?;
        acceptor.accept(stream).await
    }
}

fn build_acceptor(
    cert_path: &str,
    key_path: &str,
) -> Result<TlsAcceptor, Box<dyn std::error::Error>> {
    let certs: Vec<CertificateDer<'static>> =
        CertificateDer::pem_file_iter(cert_path)?.collect::<Result<_, _>>()?;
    if certs.is_empty() {
        return Err("no certificates found in cert file".into());
    }
    let key = PrivateKeyDer::from_pem_file(key_path)?;

    let config = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)?;
    Ok(TlsAcceptor::from(Arc::new(config)))
}
