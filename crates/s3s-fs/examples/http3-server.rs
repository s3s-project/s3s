// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

//! An HTTP/3-only server for the S3 API, backed by a local directory.
//!
//! The `s3s-fs` binary always serves TCP and adds QUIC with `--http3`; this
//! example serves QUIC only, which is what the HTTP/2-versus-HTTP/3 benchmark
//! harness needs. It uses the `s3s_fs::http3` module directly, so it also shows
//! how to embed the file system service in another program.
//!
//! # Environment
//!
//! | variable | default | meaning |
//! | --- | --- | --- |
//! | `S3S_HTTP3_ROOT` | `target/s3s-http3-data` | directory used as the object store |
//! | `S3S_HTTP3_BIND` | `127.0.0.1:8443` | UDP address to bind |
//! | `S3S_HTTP3_CERT` | unset | certificate chain in PEM |
//! | `S3S_HTTP3_KEY` | unset | private key in PEM |
//! | `S3S_HTTP3_CERT_OUT` | unset | write the certificate that is used to this PEM file |
//! | `S3S_HTTP3_ACCESS_KEY` | unset | `SigV4` access key |
//! | `S3S_HTTP3_SECRET_KEY` | unset | `SigV4` secret key |
//!
//! With `S3S_HTTP3_CERT` and `S3S_HTTP3_KEY` unset, a temporary self-signed certificate is
//! generated at startup; set both to use your own PEM files, or set `S3S_HTTP3_CERT_OUT`
//! to export the generated certificate for a client. Without credentials the server
//! prints a warning and asks the operator to keep the endpoint on loopback, so leave
//! `S3S_HTTP3_BIND` on a loopback address in that case. The endpoint needs UDP, TLS 1.3,
//! and the `h3` ALPN protocol, and it does not listen on TCP.
//!
//! # Run
//!
//! ```text
//! S3S_HTTP3_CERT_OUT=target/s3s-http3-cert.pem \
//!   cargo run -p s3s-fs --features http3 --example http3-server
//!
//! cargo run -p s3s-fs --features http3 --example http3-client -- \
//!   --url https://localhost:8443 --cert target/s3s-http3-cert.pem
//! ```

use s3s::auth::SimpleAuth;
use s3s::host::SingleDomain;
use s3s::service::S3ServiceBuilder;
use s3s_fs::FileSystem;
use s3s_fs::http3::{Http3Config, Http3Server};

use std::env;
use std::error::Error;
use std::io;
use std::net::SocketAddr;
use std::path::PathBuf;

type Result<T = ()> = std::result::Result<T, Box<dyn Error + Send + Sync>>;

fn env_path(name: &str, default: &str) -> PathBuf {
    env::var_os(name).map_or_else(|| PathBuf::from(default), PathBuf::from)
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result {
    let root = env_path("S3S_HTTP3_ROOT", "target/s3s-http3-data");
    let cert = env::var_os("S3S_HTTP3_CERT").map(PathBuf::from);
    let key = env::var_os("S3S_HTTP3_KEY").map(PathBuf::from);
    let cert_out = env::var_os("S3S_HTTP3_CERT_OUT").map(PathBuf::from);

    let bind: SocketAddr = env::var("S3S_HTTP3_BIND")
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

    let config = Http3Config::new(bind, cert, key, cert_out);
    let server = Http3Server::bind(config)
        .await
        .map_err(|error| io::Error::other(format!("{error:?}")))?;

    println!("HTTP/3 server listening on https://{}", server.local_addr());
    println!("data root: {}", root.display());

    let shutdown = async {
        tokio::signal::ctrl_c().await.expect("failed to install Ctrl-C handler");
        println!("shutting down");
    };

    server.serve(service, shutdown).await;

    Ok(())
}
