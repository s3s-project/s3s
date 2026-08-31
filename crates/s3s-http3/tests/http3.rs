// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 The s3s Authors

use bytes::{Buf, Bytes};
use h3::client::RequestStream;
use http::{HeaderMap, Method, Request, Response, StatusCode};
use quinn::rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use s3s::service::S3ServiceBuilder;
use s3s_fs::FileSystem;

use std::error::Error;
use std::sync::Arc;

type TestResult<T = ()> = Result<T, Box<dyn Error + Send + Sync>>;
type ClientStream = RequestStream<h3_quinn::BidiStream<Bytes>, Bytes>;

fn server_endpoint() -> TestResult<(s3s_http3::Endpoint, CertificateDer<'static>)> {
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

fn client_endpoint(certificate: CertificateDer<'static>) -> TestResult<quinn::Endpoint> {
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

async fn receive_response(mut stream: ClientStream) -> TestResult<(Response<()>, Vec<u8>, Option<HeaderMap>)> {
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn serves_put_and_get_over_http3() -> TestResult {
    let root = std::env::temp_dir().join(format!("s3s-http3-{}", std::process::id()));

    if root.exists() {
        std::fs::remove_dir_all(&root)?;
    }
    std::fs::create_dir_all(&root)?;

    let filesystem = FileSystem::new(&root).map_err(|error| std::io::Error::other(format!("{error:?}")))?;
    let service = S3ServiceBuilder::new(filesystem).build();
    let (endpoint, certificate) = server_endpoint()?;
    let server_address = endpoint.local_addr()?;

    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
    let server_task = tokio::spawn(s3s_http3::serve(endpoint, service, async move {
        let _ = shutdown_rx.await;
    }));

    let client_endpoint = client_endpoint(certificate)?;
    let connection = client_endpoint.connect(server_address, "localhost")?.await?;

    let (mut h3_connection, mut send_request) = h3::client::builder().build(h3_quinn::Connection::new(connection)).await?;

    let driver = tokio::spawn(async move {
        let _ = std::future::poll_fn(|cx| h3_connection.poll_close(cx)).await;
    });

    let mut stream = send_request
        .send_request(
            Request::builder()
                .method(Method::PUT)
                .uri("http://localhost/bucket")
                .body(())?,
        )
        .await?;
    stream.finish().await?;
    let (response, _, _) = receive_response(stream).await?;
    assert_eq!(response.status(), StatusCode::OK);

    let mut stream = send_request
        .send_request(
            Request::builder()
                .method(Method::PUT)
                .uri("http://localhost/bucket/key")
                .header("content-length", "11")
                .body(())?,
        )
        .await?;
    stream.send_data(Bytes::from_static(b"hello world")).await?;
    stream.finish().await?;

    let (response, _, _) = receive_response(stream).await?;
    assert_eq!(response.status(), StatusCode::OK);

    let mut stream = send_request
        .send_request(
            Request::builder()
                .method(Method::GET)
                .uri("http://localhost/bucket/key")
                .body(())?,
        )
        .await?;
    stream.finish().await?;

    let (response, body, trailers) = receive_response(stream).await?;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(body, b"hello world");
    assert!(trailers.is_none());

    for name in [
        "connection",
        "keep-alive",
        "proxy-authenticate",
        "proxy-authorization",
        "te",
        "trailer",
        "transfer-encoding",
        "upgrade",
    ] {
        assert!(!response.headers().contains_key(name), "{name} leaked");
    }

    let _ = shutdown_tx.send(());
    drop(send_request);
    server_task.await?;

    client_endpoint.close(0u32.into(), b"test complete");
    driver.abort();
    let _ = driver.await;

    std::fs::remove_dir_all(root)?;
    Ok(())
}
