// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

//! Helpers shared by the HTTP/3 integration test modules.

use bytes::{Buf, Bytes};
use h3::client::RequestStream;
use http::{HeaderMap, Request, Response};
use quinn::rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};

use std::error::Error;
use std::sync::Arc;

pub(crate) type TestResult<T = ()> = Result<T, Box<dyn Error + Send + Sync>>;
pub(crate) type ClientStream = RequestStream<h3_quinn::BidiStream<Bytes>, Bytes>;
pub(crate) type Client = h3::client::SendRequest<h3_quinn::OpenStreams, Bytes>;
pub(crate) type ResponseData = (Response<()>, Vec<u8>, Option<HeaderMap>);

pub(crate) fn server_endpoint() -> TestResult<(s3s_http3::Endpoint, CertificateDer<'static>)> {
    let _ = quinn::rustls::crypto::ring::default_provider().install_default();
    let certificate = rcgen::generate_simple_self_signed(vec!["localhost".to_owned()])?;
    let certificate_der = certificate.cert.der().clone();
    let private_key = PrivatePkcs8KeyDer::from(certificate.signing_key.serialize_der());

    let mut tls = quinn::rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![certificate_der.clone()], PrivateKeyDer::from(private_key))?;
    tls.alpn_protocols = vec![b"h3".to_vec()];

    let config = quinn::ServerConfig::with_crypto(Arc::new(quinn::crypto::rustls::QuicServerConfig::try_from(tls)?));

    Ok((s3s_http3::Endpoint::server(config, "127.0.0.1:0".parse()?)?, certificate_der))
}

pub(crate) fn client_endpoint(certificate: CertificateDer<'static>) -> TestResult<quinn::Endpoint> {
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
