//! HTTPS server example for s3s
//!
//! This example demonstrates how to run an S3 service over HTTPS using TLS.
//! It uses tokio-rustls for TLS support and generates a self-signed certificate for testing.
//!
//! # Usage
//!
//! ```bash
//! cargo run --example https
//! ```
//!
//! Then you can access the server at <https://localhost:8014> (note: you'll need to accept
//! the self-signed certificate warning in your browser or S3 client).
//!
//! For production use, replace the self-signed certificate with a proper certificate
//! from a trusted certificate authority.

use s3s::auth::SimpleAuth;
use s3s::dto::{GetObjectInput, GetObjectOutput};
use s3s::service::S3ServiceBuilder;
use s3s::{S3, S3Request, S3Response, S3Result};

use std::io;
use std::sync::Arc;

use tokio::net::TcpListener;
use tokio_rustls::TlsAcceptor;
use tokio_rustls::rustls;
use tokio_rustls::rustls::ServerConfig;
use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer};

use hyper_util::rt::{TokioExecutor, TokioIo};
use hyper_util::server::conn::auto::Builder as ConnBuilder;

/// A minimal S3 implementation for demonstration purposes
#[derive(Debug, Clone)]
struct DummyS3;

#[async_trait::async_trait]
impl S3 for DummyS3 {
    async fn get_object(&self, _req: S3Request<GetObjectInput>) -> S3Result<S3Response<GetObjectOutput>> {
        Err(s3s::s3_error!(NotImplemented, "GetObject is not implemented"))
    }
}

/// Generate a self-signed certificate for testing purposes
fn generate_self_signed_cert() -> io::Result<(Vec<CertificateDer<'static>>, PrivateKeyDer<'static>)> {
    use rcgen::{CertificateParams, KeyPair};

    // Generate a new key pair
    let key_pair = KeyPair::generate().map_err(io::Error::other)?;

    // Create certificate parameters
    let mut params = CertificateParams::new(vec!["localhost".to_string()]).map_err(io::Error::other)?;

    params.distinguished_name.push(rcgen::DnType::CommonName, "localhost");

    // Generate the certificate
    let cert = params.self_signed(&key_pair).map_err(io::Error::other)?;

    // Convert to DER format
    let cert_der = CertificateDer::from(cert.der().to_vec());
    let key_der = PrivateKeyDer::try_from(key_pair.serialize_der()).map_err(io::Error::other)?;

    Ok((vec![cert_der], key_der))
}

/// Create TLS server configuration
fn create_tls_config() -> io::Result<ServerConfig> {
    let (certs, key) = generate_self_signed_cert()?;

    let mut config = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .map_err(io::Error::other)?;

    // Use default protocol versions (TLS 1.2 and 1.3)
    config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];

    Ok(config)
}

#[tokio::main]
async fn main() -> io::Result<()> {
    // Install the default crypto provider (required for rustls)
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();

    // Setup tracing
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env().add_directive(tracing::Level::INFO.into()))
        .init();

    // Create a simple S3 service
    let s3_service = {
        let mut builder = S3ServiceBuilder::new(DummyS3);

        // Enable authentication (optional)
        builder.set_auth(SimpleAuth::from_single("AKEXAMPLES3S", "SKEXAMPLES3S"));

        builder.build()
    };

    // Create TLS configuration
    let tls_config = create_tls_config()?;
    let tls_acceptor = TlsAcceptor::from(Arc::new(tls_config));

    // Bind to address
    let addr = "127.0.0.1:8014";
    let listener = TcpListener::bind(addr).await?;

    tracing::info!("HTTPS server listening on https://{}", addr);
    tracing::info!("Using self-signed certificate - you'll need to accept the certificate warning");
    tracing::info!("Press Ctrl+C to stop");

    let http_server = ConnBuilder::new(TokioExecutor::new());
    let graceful = hyper_util::server::graceful::GracefulShutdown::new();

    let mut ctrl_c = std::pin::pin!(tokio::signal::ctrl_c());

    loop {
        let (stream, remote_addr) = tokio::select! {
            res = listener.accept() => {
                match res {
                    Ok(conn) => conn,
                    Err(err) => {
                        tracing::error!("error accepting connection: {err}");
                        continue;
                    }
                }
            }
            _ = ctrl_c.as_mut() => {
                tracing::info!("Received Ctrl+C, shutting down...");
                break;
            }
        };

        tracing::debug!("Accepted connection from {}", remote_addr);

        // Perform TLS handshake
        let tls_stream = match tls_acceptor.accept(stream).await {
            Ok(s) => s,
            Err(e) => {
                tracing::error!("TLS handshake failed from {}: {}", remote_addr, e);
                continue;
            }
        };

        tracing::debug!("TLS handshake completed for {}", remote_addr);

        // Serve the connection
        let conn = http_server.serve_connection(TokioIo::new(tls_stream), s3_service.clone());
        let conn = graceful.watch(conn.into_owned());

        tokio::spawn(async move {
            if let Err(e) = conn.await {
                tracing::error!("Error serving connection: {}", e);
            }
        });
    }

    // Graceful shutdown
    tokio::select! {
        () = graceful.shutdown() => {
            tracing::info!("Gracefully shut down!");
        },
        () = tokio::time::sleep(std::time::Duration::from_secs(10)) => {
            tracing::info!("Waited 10 seconds for graceful shutdown, aborting...");
        }
    }

    Ok(())
}
