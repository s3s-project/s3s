// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

//! Helpers shared by the HTTP/3 integration test modules.

use bytes::{Buf, Bytes};
use h3::client::RequestStream;
use http::{HeaderMap, Request, Response};
use quinn::rustls::pki_types::CertificateDer;
use quinn::rustls::pki_types::pem::PemObject;

use std::error::Error;
use std::path::Path;
use std::sync::Arc;

pub(crate) type TestResult<T = ()> = Result<T, Box<dyn Error + Send + Sync>>;
pub(crate) type ClientStream = RequestStream<h3_quinn::BidiStream<Bytes>, Bytes>;
pub(crate) type Client = h3::client::SendRequest<h3_quinn::OpenStreams, Bytes>;
pub(crate) type ResponseData = (Response<()>, Vec<u8>, Option<HeaderMap>);

/// Converts a library error, which intentionally is not a `std` error, into the
/// boxed error used by the tests.
pub(crate) fn library_error(error: &s3s_fs::Error) -> Box<dyn Error + Send + Sync> {
    std::io::Error::other(format!("{error:?}")).into()
}

/// A temporary directory for one test, removed when it is dropped.
pub(crate) struct CleanupGuard {
    path: std::path::PathBuf,
}

impl CleanupGuard {
    /// Creates an empty temporary directory tagged with `name`, which has to be
    /// unique within the test target because tests run in parallel.
    pub(crate) fn new(name: &str) -> TestResult<(std::path::PathBuf, Self)> {
        let path = std::env::temp_dir().join(format!("s3s-fs-http3-{name}-{}", std::process::id()));

        if path.exists() {
            std::fs::remove_dir_all(&path)?;
        }

        std::fs::create_dir_all(&path)?;

        Ok((path.clone(), Self { path }))
    }
}

impl Drop for CleanupGuard {
    fn drop(&mut self) {
        if self.path.exists() {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }
}

/// Reads the first certificate of a PEM chain.
pub(crate) fn load_certificate(path: &Path) -> TestResult<CertificateDer<'static>> {
    CertificateDer::pem_file_iter(path)?
        .next()
        .ok_or_else(|| std::io::Error::other("the certificate file is empty"))?
        .map_err(Into::into)
}

/// Builds a client endpoint that trusts exactly the given certificate.
pub(crate) fn client_endpoint(certificate: CertificateDer<'static>) -> TestResult<quinn::Endpoint> {
    let _ = quinn::rustls::crypto::ring::default_provider().install_default();

    let mut roots = quinn::rustls::RootCertStore::empty();
    roots.add(certificate)?;

    let mut tls = quinn::rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    tls.alpn_protocols = vec![b"h3".to_vec()];

    let config = quinn::ClientConfig::new(Arc::new(quinn::crypto::rustls::QuicClientConfig::try_from(tls)?));

    let mut endpoint = quinn::Endpoint::client("127.0.0.1:0".parse()?)?;
    endpoint.set_default_client_config(config);

    Ok(endpoint)
}

/// Connects a client to `address` and returns the endpoint, the driver task and
/// the client handle.
pub(crate) async fn connect_client(
    address: std::net::SocketAddr,
    certificate: CertificateDer<'static>,
) -> TestResult<(quinn::Endpoint, tokio::task::JoinHandle<()>, Client)> {
    let client_endpoint = client_endpoint(certificate)?;
    let connection = client_endpoint.connect(address, "localhost")?.await?;
    let (mut h3_connection, client) = h3::client::builder().build(h3_quinn::Connection::new(connection)).await?;

    let driver = tokio::spawn(async move {
        let _ = std::future::poll_fn(|cx| h3_connection.poll_close(cx)).await;
    });

    Ok((client_endpoint, driver, client))
}

pub(crate) async fn receive_response(mut stream: ClientStream) -> TestResult<(Response<()>, Vec<u8>, Option<HeaderMap>)> {
    let response = stream.recv_response().await?;
    let mut body = Vec::new();

    while let Some(mut chunk) = stream.recv_data().await? {
        while chunk.has_remaining() {
            let size = chunk.chunk().len();
            body.extend_from_slice(chunk.chunk());
            chunk.advance(size);
        }
    }

    let trailers = stream.recv_trailers().await?;
    Ok((response, body, trailers))
}

pub(crate) async fn send(
    client: &mut Client,
    request: Request<()>,
    chunks: impl IntoIterator<Item = Bytes>,
) -> TestResult<ResponseData> {
    let mut stream = client.send_request(request).await?;

    for chunk in chunks {
        stream.send_data(chunk).await?;
    }

    stream.finish().await?;
    receive_response(stream).await
}

/// Shuts the server down, waits for it to drain, and closes the client.
pub(crate) async fn shutdown_server(
    shutdown: tokio::sync::oneshot::Sender<()>,
    client: Client,
    server: tokio::task::JoinHandle<()>,
    client_endpoint: quinn::Endpoint,
    driver: tokio::task::JoinHandle<()>,
) -> TestResult {
    let _ = shutdown.send(());
    drop(client);

    tokio::time::timeout(std::time::Duration::from_secs(2), server)
        .await
        .map_err(|_| std::io::Error::other("the server did not shut down"))??;

    client_endpoint.close(0u32.into(), b"test complete");
    driver.abort();
    let _ = driver.await;

    Ok(())
}
