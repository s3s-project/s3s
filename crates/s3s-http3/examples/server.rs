// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

//! An HTTP/3 server for the S3 API, backed by a local directory.
//!
//! # Environment
//!
//! | variable | default | meaning |
//! | --- | --- | --- |
//! | `S3S_HTTP3_ROOT` | `target/s3s-http3-data` | directory used as the object store |
//! | `S3S_HTTP3_BIND` | `127.0.0.1:8443` | UDP address to bind |
//! | `S3S_HTTP3_CERT` | unset | certificate chain in PEM |
//! | `S3S_HTTP3_KEY` | unset | private key in PEM |
//! | `S3S_HTTP3_ACCESS_KEY` | unset | `SigV4` access key |
//! | `S3S_HTTP3_SECRET_KEY` | unset | `SigV4` secret key |
//!
//! With `S3S_HTTP3_CERT` and `S3S_HTTP3_KEY` unset, a temporary self-signed certificate is
//! generated at startup; set both to use your own PEM files. Without credentials the server
//! prints a warning and asks the operator to keep the endpoint on loopback, so leave
//! `S3S_HTTP3_BIND` on a loopback address in that case. The endpoint needs UDP, TLS 1.3,
//! and the `h3` ALPN protocol.
//!
//! # Run
//!
//! ```text
//! cargo run -p s3s-http3 --example server
//!
//! curl --http3-only -k --resolve localhost:8443:127.0.0.1 \
//!   -X PUT https://localhost:8443/bucket
//!
//! curl --http3-only -k --resolve localhost:8443:127.0.0.1 \
//!   -X PUT --data-binary 'hello over HTTP/3' https://localhost:8443/bucket/key
//!
//! curl --http3-only -k --resolve localhost:8443:127.0.0.1 \
//!   https://localhost:8443/bucket/key
//! ```
//!
//! The final command should print `hello over HTTP/3`.

use quinn::rustls::pki_types::pem::PemObject;
use quinn::rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};

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

fn load_server_config(cert: Option<&Path>, key: Option<&Path>) -> Result<quinn::ServerConfig> {
    let (certs, key) = match (cert, key) {
        (Some(cert), Some(key)) => (load_certs(cert)?, PrivateKeyDer::from_pem_file(key).map_err(io::Error::other)?),
        (None, None) => {
            eprintln!("using a temporary self-signed certificate; set S3S_HTTP3_CERT and S3S_HTTP3_KEY to use your own");
            self_signed()?
        }
        _ => {
            return Err(
                io::Error::new(io::ErrorKind::InvalidInput, "S3S_HTTP3_CERT and S3S_HTTP3_KEY must be set together").into(),
            );
        }
    };

    let mut tls = quinn::rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .map_err(io::Error::other)?;

    tls.alpn_protocols = vec![b"h3".to_vec()];

    let crypto = quinn::crypto::rustls::QuicServerConfig::try_from(tls).map_err(io::Error::other)?;

    Ok(quinn::ServerConfig::with_crypto(Arc::new(crypto)))
}

fn load_certs(path: &Path) -> Result<Vec<CertificateDer<'static>>> {
    let certs = CertificateDer::pem_file_iter(path)
        .map_err(io::Error::other)?
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(io::Error::other)?;

    if certs.is_empty() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "certificate file is empty").into());
    }

    Ok(certs)
}

fn self_signed() -> Result<(Vec<CertificateDer<'static>>, PrivateKeyDer<'static>)> {
    let certified = rcgen::generate_simple_self_signed(vec!["localhost".to_owned()]).map_err(io::Error::other)?;
    let cert = certified.cert.der().clone();
    let key = PrivateKeyDer::from(PrivatePkcs8KeyDer::from(certified.signing_key.serialize_der()));
    Ok((vec![cert], key))
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result {
    let _ = quinn::rustls::crypto::ring::default_provider().install_default();
    let root = env_path("S3S_HTTP3_ROOT", "target/s3s-http3-data");
    let cert = env::var_os("S3S_HTTP3_CERT").map(PathBuf::from);
    let key = env::var_os("S3S_HTTP3_KEY").map(PathBuf::from);

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
    let endpoint = s3s_http3::Endpoint::server(load_server_config(cert.as_deref(), key.as_deref())?, bind)?;
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
