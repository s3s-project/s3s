// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

//! HTTP/3 support for the file system server.
//!
//! This module owns the QUIC endpoint, the TLS configuration and the serve
//! loop, so the binary only passes the address, the certificate paths and the
//! service. The same [`S3Service`] and the same address can be served over TCP
//! at the same time: TCP and UDP are separate namespaces, so both transports
//! can bind the same port. A client selects the transport, the server never
//! advertises HTTP/3 over the TCP connection.
//!
//! # Certificate
//!
//! [`Http3Config`] takes PEM files for the certificate chain and the private
//! key. Without them a self-signed certificate for `localhost` is generated, so
//! a local endpoint works out of the box; that certificate is only meant for
//! development. The optional certificate output records the certificate that is
//! actually used, which is how a client learns to trust a generated one. Only
//! the certificate is written, never the private key.
//!
//! # Shutdown
//!
//! [`Http3Server::serve`] stops accepting connections once the shutdown future
//! resolves, asks the active connections to shut down, and returns after they
//! drained or after [`s3s_http3::DEFAULT_SHUTDOWN_TIMEOUT`]. Sharing one
//! shutdown signal with the TCP listener keeps both transports in step.
//!
//! # Failure
//!
//! [`Http3Server::bind`] resolves the TLS material and binds the UDP socket
//! before any request is served, so a broken certificate or an occupied port
//! fails the caller instead of silently serving TCP only.
//!
//! # Example
//!
//! ```no_run
//! # use s3s::service::S3Service;
//! # async fn example(addr: std::net::SocketAddr, service: S3Service) -> s3s_fs::Result<()> {
//! let config = s3s_fs::http3::Http3Config::new(addr, None, None, None);
//! s3s_fs::http3::serve_http3(config, service, std::future::pending::<()>()).await
//! # }
//! ```

use s3s::service::S3Service;

use std::future::Future;
use std::io;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use quinn::rustls::pki_types::pem::PemObject;
use quinn::rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};

use crate::Result;

/// Configuration of an HTTP/3 endpoint.
pub struct Http3Config {
    addr: SocketAddr,
    cert: Option<PathBuf>,
    key: Option<PathBuf>,
    cert_out: Option<PathBuf>,
}

impl Http3Config {
    /// Creates a configuration that binds `addr`.
    ///
    /// `cert` and `key` are PEM files holding the certificate chain and the
    /// private key. When both are [`None`], a self-signed certificate for
    /// `localhost` is generated; supplying only one of them is an error.
    ///
    /// `cert_out` optionally receives the certificate chain that is actually
    /// used, including the generated one. It never receives the private key.
    #[must_use]
    pub fn new(addr: SocketAddr, cert: Option<PathBuf>, key: Option<PathBuf>, cert_out: Option<PathBuf>) -> Self {
        Self {
            addr,
            cert,
            key,
            cert_out,
        }
    }
}

/// A bound QUIC endpoint that serves HTTP/3.
pub struct Http3Server {
    endpoint: quinn::Endpoint,
    addr: SocketAddr,
}

impl Http3Server {
    /// Builds the TLS configuration, binds the UDP socket and writes the
    /// certificate when a certificate output path is configured.
    ///
    /// The endpoint accepts QUIC connections when this returns, so TLS and
    /// binding errors surface before the caller starts serving requests.
    pub async fn bind(config: Http3Config) -> Result<Self> {
        let Http3Config {
            addr,
            cert,
            key,
            cert_out,
        } = config;

        let tls = load_tls(cert.as_deref(), key.as_deref())?;

        if let Some(path) = &cert_out {
            tokio::fs::write(path, &tls.certificate_pem).await?;
        }

        let endpoint = quinn::Endpoint::server(tls.server_config()?, addr)?;
        let addr = endpoint.local_addr()?;

        tracing::info!("HTTP/3 is listening on https://{addr}");

        Ok(Self { endpoint, addr })
    }

    /// Returns the local address the endpoint is bound to.
    #[must_use]
    pub fn local_addr(&self) -> SocketAddr {
        self.addr
    }

    /// Serves `service` until `shutdown` resolves.
    ///
    /// The endpoint stops accepting connections, the active connections receive
    /// a GOAWAY, and connections that are still open are closed after
    /// [`s3s_http3::DEFAULT_SHUTDOWN_TIMEOUT`].
    pub async fn serve(self, service: S3Service, shutdown: impl Future<Output = ()>) {
        s3s_http3::serve(self.endpoint, service, shutdown).await;
    }
}

/// Binds an HTTP/3 endpoint from `config` and serves `service` on it until
/// `shutdown` resolves.
pub async fn serve_http3(config: Http3Config, service: S3Service, shutdown: impl Future<Output = ()>) -> Result<()> {
    Http3Server::bind(config).await?.serve(service, shutdown).await;
    Ok(())
}

/// The TLS material of an endpoint.
struct Tls {
    certs: Vec<CertificateDer<'static>>,
    key: PrivateKeyDer<'static>,
    certificate_pem: Vec<u8>,
}

impl Tls {
    /// Builds the QUIC server configuration, which requires TLS 1.3 and the
    /// ALPN protocol `h3`.
    fn server_config(&self) -> Result<quinn::ServerConfig> {
        let _ = quinn::rustls::crypto::ring::default_provider().install_default();

        let mut tls = quinn::rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(self.certs.clone(), self.key.clone_key())?;

        tls.alpn_protocols = vec![b"h3".to_vec()];

        let crypto = quinn::crypto::rustls::QuicServerConfig::try_from(tls)?;

        Ok(quinn::ServerConfig::with_crypto(Arc::new(crypto)))
    }
}

/// Loads the configured PEM files or generates a self-signed certificate.
fn load_tls(cert: Option<&Path>, key: Option<&Path>) -> Result<Tls> {
    match (cert, key) {
        (Some(cert), Some(key)) => {
            let certificate_pem = std::fs::read(cert)?;
            let certs = parse_certificates(&certificate_pem)?;
            let key = PrivateKeyDer::from_pem_file(key)?;

            Ok(Tls {
                certs,
                key,
                certificate_pem,
            })
        }
        (None, None) => {
            tracing::warn!("no certificate configured; generating a self-signed certificate for localhost");

            let certified = rcgen::generate_simple_self_signed(vec!["localhost".to_owned()])?;
            let certificate_pem = certified.cert.pem().into_bytes();
            let certs = parse_certificates(&certificate_pem)?;
            let key = PrivateKeyDer::from(PrivatePkcs8KeyDer::from(certified.signing_key.serialize_der()));

            Ok(Tls {
                certs,
                key,
                certificate_pem,
            })
        }
        _ => Err(io::Error::new(io::ErrorKind::InvalidInput, "certificate and private key must be set together").into()),
    }
}

/// Parses a PEM certificate chain, which must not be empty.
fn parse_certificates(pem: &[u8]) -> Result<Vec<CertificateDer<'static>>> {
    let certs = CertificateDer::pem_slice_iter(pem).collect::<std::result::Result<Vec<_>, _>>()?;

    if certs.is_empty() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "certificate file is empty").into());
    }

    Ok(certs)
}
