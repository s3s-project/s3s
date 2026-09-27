// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

//! An HTTP/3 server that serves a generic [`tower`] service.
//!
//! Unlike a server for the S3 API (see the `s3s-fs` crate), this example uses the
//! `serve_with` entry point: the factory receives the peer [`std::net::SocketAddr`] of each
//! QUIC connection, and the service receives a streaming [`s3s_http3::RequestBody`].
//! Nothing here depends on the S3 API: the response body is a plain
//! [`http_body_util::Full`].
//!
//! Hop-by-hop response headers (`connection`, `transfer-encoding`, `te`, `upgrade`,
//! `keep-alive`, and any header listed in `connection`) are stripped by the transport.
//!
//! # Certificate
//!
//! With `S3S_HTTP3_CERT` and `S3S_HTTP3_KEY` unset, a temporary self-signed certificate is
//! generated at startup; set both to use your own PEM files.
//!
//! # Environment
//!
//! | variable | default | meaning |
//! | --- | --- | --- |
//! | `S3S_HTTP3_BIND` | `127.0.0.1:8444` | UDP address to bind |
//! | `S3S_HTTP3_CERT` | unset | certificate chain in PEM |
//! | `S3S_HTTP3_KEY` | unset | private key in PEM |
//!
//! # Run
//!
//! ```text
//! cargo run -p s3s-http3 --example serve-with
//!
//! curl --http3-only -k --resolve localhost:8444:127.0.0.1 \
//!   --data-binary 'hello over HTTP/3' https://localhost:8444/anything
//! ```
//!
//! The response prints the peer address and the number of body bytes received.

use bytes::Bytes;
use http::{Request, Response};
use http_body_util::{BodyExt, Full};
use quinn::rustls::pki_types::pem::PemObject;
use quinn::rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};

use std::env;
use std::error::Error;
use std::io;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

type Result<T = ()> = std::result::Result<T, Box<dyn Error + Send + Sync>>;

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

    let bind: SocketAddr = env::var("S3S_HTTP3_BIND")
        .unwrap_or_else(|_| "127.0.0.1:8444".to_owned())
        .parse()?;
    let cert = env::var_os("S3S_HTTP3_CERT").map(PathBuf::from);
    let key = env::var_os("S3S_HTTP3_KEY").map(PathBuf::from);

    let endpoint = s3s_http3::Endpoint::server(load_server_config(cert.as_deref(), key.as_deref())?, bind)?;
    let local_addr = endpoint.local_addr()?;
    println!("generic HTTP/3 service listening on https://{local_addr}");

    let make_service = move |remote_addr: SocketAddr| {
        tower::service_fn(move |request: Request<s3s_http3::RequestBody>| async move {
            let collected = BodyExt::collect(request.into_body()).await?;
            let received = collected.to_bytes();
            let received_len = received.len();
            let text = format!("remote={remote_addr}\nreceived={received_len} bytes\n");
            Ok::<_, Box<dyn Error + Send + Sync>>(Response::new(Full::new(Bytes::from(text))))
        })
    };

    let shutdown = async {
        tokio::signal::ctrl_c().await.expect("failed to install Ctrl-C handler");
        println!("shutting down");
    };

    s3s_http3::serve_with(endpoint, make_service, shutdown).await;

    Ok(())
}
