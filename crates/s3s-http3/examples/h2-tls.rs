// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

use hyper_util::rt::{TokioExecutor, TokioIo};
use hyper_util::server::conn::auto::Builder as ConnBuilder;
use s3s::host::SingleDomain;
use s3s::service::S3ServiceBuilder;
use s3s_fs::FileSystem;
use std::env;
use std::error::Error;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::net::TcpListener;
use tokio_rustls::TlsAcceptor;
use tokio_rustls::rustls;
use tokio_rustls::rustls::pki_types::pem::PemObject;
use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer};

type Result<T = ()> = std::result::Result<T, Box<dyn Error + Send + Sync>>;

fn env_path(name: &str, default: &str) -> PathBuf {
    env::var_os(name).map_or_else(|| PathBuf::from(default), PathBuf::from)
}

fn load_tls_config(cert_path: &Path, key_path: &Path) -> Result<rustls::ServerConfig> {
    let certs = CertificateDer::pem_file_iter(cert_path)
        .map_err(io::Error::other)?
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(io::Error::other)?;

    if certs.is_empty() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "certificate file is empty").into());
    }

    let key = PrivateKeyDer::from_pem_file(key_path).map_err(io::Error::other)?;

    let mut config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .map_err(io::Error::other)?;

    config.alpn_protocols = vec![b"h2".to_vec()];

    Ok(config)
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result {
    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("failed to install ring TLS provider");

    let root = env_path("S3S_HTTP2_ROOT", "target/s3s-http2-data");
    let cert = env_path("S3S_HTTP2_CERT", "cert.pem");
    let key = env_path("S3S_HTTP2_KEY", "key.pem");

    let bind: std::net::SocketAddr = env::var("S3S_HTTP2_BIND")
        .unwrap_or_else(|_| "127.0.0.1:8015".to_owned())
        .parse()?;

    std::fs::create_dir_all(&root)?;

    let filesystem = FileSystem::new(&root).map_err(|error| io::Error::other(format!("{error:?}")))?;

    let mut builder = S3ServiceBuilder::new(filesystem);
    builder.set_host(SingleDomain::new("localhost")?);
    let service = builder.build();

    let tls_acceptor = TlsAcceptor::from(Arc::new(load_tls_config(&cert, &key)?));
    let listener = TcpListener::bind(bind).await?;

    println!("HTTP/2 server listening on https://{bind}");
    println!("data root: {}", root.display());

    let http_server = ConnBuilder::new(TokioExecutor::new());
    let graceful = hyper_util::server::graceful::GracefulShutdown::new();
    let mut ctrl_c = std::pin::pin!(tokio::signal::ctrl_c());

    loop {
        let (socket, peer) = tokio::select! {
            result = listener.accept() => match result {
                Ok(connection) => connection,
                Err(error) => {
                    eprintln!("accept failed: {error}");
                    continue;
                }
            },
            _ = ctrl_c.as_mut() => break,
        };

        socket.set_nodelay(true)?;

        let tls_stream = match tls_acceptor.accept(socket).await {
            Ok(stream) => stream,
            Err(error) => {
                eprintln!("TLS handshake failed for {peer}: {error}");
                continue;
            }
        };

        let connection = http_server.serve_connection(TokioIo::new(tls_stream), service.clone());
        let connection = graceful.watch(connection.into_owned());

        tokio::spawn(async move {
            if let Err(error) = connection.await {
                eprintln!("HTTP/2 connection failed for {peer}: {error}");
            }
        });
    }

    tokio::select! {
        () = graceful.shutdown() => {}
        () = tokio::time::sleep(std::time::Duration::from_secs(10)) => {}
    }

    Ok(())
}
