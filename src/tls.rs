use std::fs::File;
use std::io::BufReader;
use std::sync::Arc;

use rustls::ServerConfig;
use tokio_rustls::TlsAcceptor;
use tracing::info;

/// TLS configuration wrapper for STARTTLS support.
#[derive(Clone)]
pub struct TlsConfig {
    acceptor: TlsAcceptor,
}

impl TlsConfig {
    /// Load TLS configuration from PEM certificate and key files.
    pub fn load(cert_path: &str, key_path: &str) -> Result<Self, Box<dyn std::error::Error>> {
        let cert_file = File::open(cert_path)?;
        let mut cert_reader = BufReader::new(cert_file);
        let certs: Vec<_> =
            rustls_pemfile::certs(&mut cert_reader).collect::<Result<Vec<_>, _>>()?;

        if certs.is_empty() {
            return Err("no certificates found in cert file".into());
        }

        let key_file = File::open(key_path)?;
        let mut key_reader = BufReader::new(key_file);
        let key = rustls_pemfile::private_key(&mut key_reader)?
            .ok_or("no private key found in key file")?;

        let config = ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(certs, key)?;

        info!(cert = cert_path, key = key_path, "TLS configuration loaded");

        Ok(Self {
            acceptor: TlsAcceptor::from(Arc::new(config)),
        })
    }

    /// Perform TLS handshake on a plain TCP stream.
    pub async fn accept(
        &self,
        stream: tokio::net::TcpStream,
    ) -> Result<tokio_rustls::server::TlsStream<tokio::net::TcpStream>, std::io::Error> {
        self.acceptor.accept(stream).await
    }
}
