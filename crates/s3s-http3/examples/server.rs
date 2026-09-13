// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

use quinn::rustls::pki_types::pem::PemObject;
use quinn::rustls::pki_types::{CertificateDer, PrivateKeyDer};

use s3s::auth::SimpleAuth;
use s3s::host::SingleDomain;
use s3s::service::S3ServiceBuilder;
use s3s_fs::FileSystem;

use std::env;
use std::error::Error;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

type Result<T = ()> = std::result::Result<T, Box<dyn Error + Send + Sync>>;

fn env_path(name: &str, default: &str) -> PathBuf {
    env::var_os(name).map_or_else(|| PathBuf::from(default), PathBuf::from)
}

fn load_server_config(cert_path: &Path, key_path: &Path) -> Result<quinn::ServerConfig> {
    let certs = CertificateDer::pem_file_iter(cert_path)
        .map_err(io::Error::other)?
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(io::Error::other)?;

    if certs.is_empty() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "certificate file is empty").into());
    }

    let key = PrivateKeyDer::from_pem_file(key_path).map_err(io::Error::other)?;

    let mut tls = quinn::rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .map_err(io::Error::other)?;

    tls.alpn_protocols = vec![b"h3".to_vec()];

    let crypto = quinn::crypto::rustls::QuicServerConfig::try_from(tls).map_err(io::Error::other)?;

    Ok(quinn::ServerConfig::with_crypto(Arc::new(crypto)))
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result {
    let _ = quinn::rustls::crypto::ring::default_provider().install_default();
    let root = env_path("S3S_HTTP3_ROOT", "target/s3s-http3-data");
    let cert = env_path("S3S_HTTP3_CERT", "cert.pem");
    let key = env_path("S3S_HTTP3_KEY", "key.pem");

    let bind: std::net::SocketAddr = env::var("S3S_HTTP3_BIND")
        .unwrap_or_else(|_| "127.0.0.1:8443".to_owned())
        .parse()?;

    std::fs::create_dir_all(&root)?;

    let filesystem = FileSystem::new(&root).map_err(|error| io::Error::other(format!("{error:?}")))?;

    let mut builder = S3ServiceBuilder::new(filesystem);
    builder.set_host(SingleDomain::new("localhost")?);

    match (env::var("S3S_HTTP3_ACCESS_KEY").ok(), env::var("S3S_HTTP3_SECRET_KEY").ok()) {
        (Some(access_key), Some(secret_key)) => {
            builder.set_auth(SimpleAuth::from_single(access_key, secret_key));
            println!("authentication enabled");
        }
        (None, None) => eprintln!("warning: authentication disabled; keep this server on loopback"),
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "both S3S_HTTP3_ACCESS_KEY and S3S_HTTP3_SECRET_KEY are required",
            )
            .into());
        }
    }

    let service = builder.build();
    let endpoint = s3s_http3::Endpoint::server(load_server_config(&cert, &key)?, bind)?;
    let local_addr = endpoint.local_addr()?;

    println!("HTTP/3 server listening on https://{local_addr}");
    println!("data root: {}", root.display());

    let shutdown = async {
        tokio::signal::ctrl_c().await.expect("failed to install Ctrl-C handler");
        println!("shutting down");
    };

    s3s_http3::serve(endpoint, service, shutdown).await;

    Ok(())
}

// Generate a local certificate:
//
// openssl req -x509 -newkey rsa:2048 -nodes \
//   -keyout key.pem \
//   -out cert.pem \
//   -days 7 \
//   -subj '/CN=localhost' \
//   -addext 'subjectAltName=DNS:localhost'
//
// Run it:
//
// cargo run -p s3s-http3 --example server
//
// In another terminal, use an HTTP/3-capable curl:
//
// curl --http3-only -k --resolve localhost:8443:127.0.0.1 \
//   -X PUT https://localhost:8443/bucket
//
// curl --http3-only -k --resolve localhost:8443:127.0.0.1 \
//   -X PUT --data-binary 'hello over HTTP/3' \
//   https://localhost:8443/bucket/key
//
// curl --http3-only -k --resolve localhost:8443:127.0.0.1 \
//   https://localhost:8443/bucket/key

// The final command should print:
// hello over HTTP/3
