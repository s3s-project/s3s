// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

//! An HTTP/3 client that smoke tests an S3 endpoint.
//!
//! The S3 SDKs and most curl builds cannot speak HTTP/3, so this example talks
//! the protocol directly: it creates a bucket, uploads an object, reads the
//! object back and verifies the bytes, lists the bucket, and deletes the object
//! and the bucket. Every response is checked to be HTTP/3. A server that uses a
//! self-signed certificate has to be trusted with `--cert`.
//!
//! # Arguments
//!
//! | argument | default | meaning |
//! | --- | --- | --- |
//! | `--url` | `https://localhost:8014` | endpoint to connect to |
//! | `--cert` | unset | PEM certificate chain to trust |
//! | `--bucket` | `s3s-smoke` | bucket used by the test |
//!
//! # Run
//!
//! ```text
//! cargo run -p s3s-fs --features binary,http3 -- --http3 --cert-out target/s3s-fs-cert.pem target/s3s-fs-data
//!
//! cargo run -p s3s-fs --features http3 --example http3-client -- \
//!   --url https://localhost:8014 --cert target/s3s-fs-cert.pem
//! ```

use bytes::{Buf, Bytes};
use http::{Method, Request, Response, StatusCode, Uri, Version, header};
use quinn::rustls::pki_types::CertificateDer;
use quinn::rustls::pki_types::pem::PemObject;

use std::env;
use std::error::Error;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

type Result<T = ()> = std::result::Result<T, Box<dyn Error + Send + Sync>>;
type Client = h3::client::SendRequest<h3_quinn::OpenStreams, Bytes>;
type ClientStream = h3::client::RequestStream<h3_quinn::BidiStream<Bytes>, Bytes>;

const OBJECT_KEY: &str = "hello.txt";
const OBJECT_BODY: &[u8] = b"hello over HTTP/3";

struct Args {
    url: Uri,
    cert: Option<PathBuf>,
    bucket: String,
}

fn usage() -> &'static str {
    "usage: http3-client [--url URL] [--cert PEM] [--bucket NAME]"
}

fn parse_args() -> Result<Args> {
    let mut url: Option<Uri> = None;
    let mut cert = None;
    let mut bucket = "s3s-smoke".to_owned();
    let mut args = env::args().skip(1);

    while let Some(arg) = args.next() {
        let mut value = || args.next().ok_or_else(|| io::Error::other(format!("{arg} requires a value")));

        match arg.as_str() {
            "--url" => url = Some(value()?.parse()?),
            "--cert" => cert = Some(PathBuf::from(value()?)),
            "--bucket" => bucket = value()?,
            "--help" | "-h" => {
                println!("{}", usage());
                std::process::exit(0);
            }
            other => return Err(io::Error::other(format!("unexpected argument: {other}\n{}", usage())).into()),
        }
    }

    Ok(Args {
        url: url.unwrap_or("https://localhost:8014".parse()?),
        cert,
        bucket,
    })
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result {
    let args = parse_args()?;

    match tokio::time::timeout(Duration::from_secs(60), smoke_test(args)).await {
        Ok(result) => result,
        Err(_) => Err(io::Error::other("the smoke test timed out").into()),
    }
}

async fn smoke_test(args: Args) -> Result {
    let authority = args
        .url
        .authority()
        .ok_or_else(|| io::Error::other("--url must contain a host"))?;
    let host = authority.host().to_owned();
    let port = authority.port_u16().unwrap_or(443);

    let endpoint = client_endpoint(args.cert.as_deref())?;
    let connection = connect(&endpoint, &host, port).await?;
    let (mut h3_connection, mut client) = h3::client::builder().build(h3_quinn::Connection::new(connection)).await?;

    let driver = tokio::spawn(async move {
        let _ = std::future::poll_fn(|cx| h3_connection.poll_close(cx)).await;
    });

    println!("smoke testing {} over HTTP/3", args.url);

    let base = args.url.to_string();
    let base = base.trim_end_matches('/');
    let bucket_uri = format!("{base}/{}", args.bucket);
    let object_uri = format!("{bucket_uri}/{OBJECT_KEY}");

    let (response, _) = request(&mut client, Method::PUT, &bucket_uri, None).await?;
    step("create bucket", &response, StatusCode::OK)?;

    let (response, _) = request(&mut client, Method::PUT, &object_uri, Some(Bytes::from_static(OBJECT_BODY))).await?;
    step("create object", &response, StatusCode::OK)?;

    let (response, body) = request(&mut client, Method::GET, &object_uri, None).await?;
    step("read object", &response, StatusCode::OK)?;

    if body != OBJECT_BODY {
        return Err(io::Error::other(format!("read back {} bytes, expected {}", body.len(), OBJECT_BODY.len())).into());
    }

    println!("read back {} bytes, the payload matches", body.len());

    let list_uri = format!("{bucket_uri}?list-type=2");
    let (response, body) = request(&mut client, Method::GET, &list_uri, None).await?;
    step("list objects", &response, StatusCode::OK)?;

    if !String::from_utf8_lossy(&body).contains(OBJECT_KEY) {
        return Err(io::Error::other("the listing does not contain the uploaded object").into());
    }

    let (response, _) = request(&mut client, Method::DELETE, &object_uri, None).await?;
    step("delete object", &response, StatusCode::NO_CONTENT)?;

    let (response, _) = request(&mut client, Method::GET, &object_uri, None).await?;
    step("read a deleted object", &response, StatusCode::NOT_FOUND)?;

    let (response, _) = request(&mut client, Method::DELETE, &bucket_uri, None).await?;
    step("delete bucket", &response, StatusCode::NO_CONTENT)?;

    endpoint.close(0u32.into(), b"smoke test complete");
    driver.abort();
    let _ = driver.await;

    println!("smoke test passed");

    Ok(())
}

/// Connects to the first address of the host that accepts the connection, so a
/// server on one family of a dual-stack name is reachable.
async fn connect(endpoint: &quinn::Endpoint, host: &str, port: u16) -> Result<quinn::Connection> {
    let mut last_error = None;

    for addr in tokio::net::lookup_host((host, port)).await? {
        let connecting = match endpoint.connect(addr, host) {
            Ok(connecting) => connecting,
            Err(error) => {
                last_error = Some(error.to_string());
                continue;
            }
        };

        match connecting.await {
            Ok(connection) => return Ok(connection),
            Err(error) => last_error = Some(error.to_string()),
        }
    }

    Err(io::Error::other(last_error.unwrap_or_else(|| format!("{host} did not resolve to an address"))).into())
}

fn client_endpoint(cert: Option<&Path>) -> Result<quinn::Endpoint> {
    let _ = quinn::rustls::crypto::ring::default_provider().install_default();

    let mut roots = quinn::rustls::RootCertStore::empty();

    if let Some(path) = cert {
        for certificate in CertificateDer::pem_file_iter(path)? {
            roots.add(certificate?)?;
        }
    }

    let mut tls = quinn::rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    tls.alpn_protocols = vec![b"h3".to_vec()];

    let config = quinn::ClientConfig::new(Arc::new(quinn::crypto::rustls::QuicClientConfig::try_from(tls)?));

    let mut endpoint = quinn::Endpoint::client("127.0.0.1:0".parse()?)?;
    endpoint.set_default_client_config(config);

    Ok(endpoint)
}

async fn request(client: &mut Client, method: Method, uri: &str, body: Option<Bytes>) -> Result<(Response<()>, Vec<u8>)> {
    let mut builder = Request::builder().method(method).uri(uri);

    if let Some(body) = &body {
        builder = builder.header(header::CONTENT_LENGTH, body.len());
    }

    let mut stream = client.send_request(builder.body(())?).await?;

    if let Some(body) = body {
        stream.send_data(body).await?;
    }

    stream.finish().await?;
    receive_response(stream).await
}

async fn receive_response(mut stream: ClientStream) -> Result<(Response<()>, Vec<u8>)> {
    let response = stream.recv_response().await?;

    if response.version() != Version::HTTP_3 {
        return Err(io::Error::other(format!("the response is {:?}, not HTTP/3", response.version())).into());
    }

    let mut body = Vec::new();

    while let Some(mut chunk) = stream.recv_data().await? {
        while chunk.has_remaining() {
            let size = chunk.chunk().len();
            body.extend_from_slice(chunk.chunk());
            chunk.advance(size);
        }
    }

    let _ = stream.recv_trailers().await?;

    Ok((response, body))
}

fn step(name: &str, response: &Response<()>, expected: StatusCode) -> Result {
    if response.status() != expected {
        return Err(io::Error::other(format!("{name}: expected {expected}, got {}", response.status())).into());
    }

    println!("{name}: {} over {:?}", response.status(), response.version());

    Ok(())
}
