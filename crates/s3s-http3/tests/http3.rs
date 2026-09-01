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
type Client = h3::client::SendRequest<h3_quinn::OpenStreams, Bytes>;
type ResponseData = (Response<()>, Vec<u8>, Option<HeaderMap>);

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

async fn send(client: &mut Client, request: Request<()>, chunks: impl IntoIterator<Item = Bytes>) -> TestResult<ResponseData> {
    let mut stream = client.send_request(request).await?;

    for chunk in chunks {
        stream.send_data(chunk).await?;
    }

    stream.finish().await?;
    receive_response(stream).await
}

fn parse_upload_id(body: &[u8]) -> TestResult<String> {
    let mut deserializer = s3s::xml::Deserializer::new(body);

    let uploaded_id = deserializer.named_element("InitiateMultipartUploadResult", |d| {
        let mut upload_id = None;

        d.for_each_element(|d, name| match name {
            b"UploadId" => d.text(|value| {
                upload_id = Some(value.to_owned());
                Ok(())
            }),
            b"Bucket" | b"Key" => d.text(|_| Ok(())),
            _ => Err(s3s::xml::DeError::UnexpectedTagName),
        })?;

        upload_id.ok_or(s3s::xml::DeError::MissingField)
    })?;

    deserializer.expect_eof()?;
    Ok(uploaded_id)
}

fn assert_no_hop_by_hop_headers(response: &Response<()>) {
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
}

enum Input<'a> {
    Str(&'a str),
    Bytes(&'a [u8]),
}

#[derive(Default)]
enum CheckWay<'a> {
    #[default]
    Skip,
    Dynamic(Box<dyn for<'b> Fn(&'b str) -> bool + 'a>),
    Equal(Input<'a>),
}

#[derive(Default)]
struct Case<'a> {
    name: &'a str,
    method: Method,
    uri: &'a str,
    headers: Vec<(&'a str, &'a str)>,
    chunks: Option<Bytes>,
    want_status: StatusCode,
    want_body: CheckWay<'a>,
    want_content_length: CheckWay<'a>,
    want_content_range: Option<&'a str>,
}

#[allow(clippy::too_many_lines)]
async fn object_operations(client: &mut Client) -> TestResult {
    let cases = [
        Case {
            name: "create bucket",
            method: Method::PUT,
            uri: "http://localhost/bucket",
            want_status: StatusCode::OK,
            ..Default::default()
        },
        Case {
            name: "create key",
            method: Method::PUT,
            uri: "http://localhost/bucket/key",
            headers: vec![("content-length", "11")],
            chunks: Some(Bytes::from_static(b"hello world")),
            want_status: StatusCode::OK,
            ..Default::default()
        },
        Case {
            name: "path-style GET",
            method: Method::GET,
            uri: "http://localhost/bucket/key",
            want_status: StatusCode::OK,
            want_body: CheckWay::Equal(Input::Bytes(b"hello world")),
            want_content_length: CheckWay::Equal(Input::Str("11")),
            ..Default::default()
        },
        Case {
            name: "virtual-hosted GET",
            method: Method::GET,
            uri: "http://bucket.localhost/key",
            want_status: StatusCode::OK,
            want_body: CheckWay::Equal(Input::Bytes(b"hello world")),
            want_content_length: CheckWay::Equal(Input::Str("11")),
            ..Default::default()
        },
        Case {
            name: "HEAD",
            method: Method::HEAD,
            uri: "http://localhost/bucket/key",
            want_status: StatusCode::OK,
            want_body: CheckWay::Equal(Input::Bytes(b"")),
            want_content_length: CheckWay::Equal(Input::Str("11")),
            ..Default::default()
        },
        Case {
            name: "range GET",
            method: Method::GET,
            uri: "http://localhost/bucket/key",
            headers: vec![("range", "bytes=0-4")],
            want_status: StatusCode::PARTIAL_CONTENT,
            want_body: CheckWay::Equal(Input::Bytes(b"hello")),
            want_content_length: CheckWay::Equal(Input::Str("5")),
            want_content_range: Some("bytes 0-4/11"),
            ..Default::default()
        },
        Case {
            name: "missing",
            method: Method::GET,
            uri: "http://localhost/bucket/missing",
            want_status: StatusCode::NOT_FOUND,
            want_body: CheckWay::Dynamic(Box::new(|s| s.contains("<Code>NoSuchKey</Code>"))),
            ..Default::default()
        },
    ];

    for case in cases {
        let Case {
            name,
            method,
            uri,
            headers,
            chunks,
            want_status,
            want_body,
            want_content_length,
            want_content_range,
        } = case;

        let mut request_builder = Request::builder().method(method).uri(uri);

        for header in headers {
            request_builder = request_builder.header(header.0, header.1);
        }

        let (response, body, trailers) = match chunks {
            Some(c) => send(client, request_builder.body(())?, std::iter::once::<Bytes>(c)).await?,
            None => send(client, request_builder.body(())?, std::iter::empty::<Bytes>()).await?,
        };

        assert_eq!(response.status(), want_status, "{name}: status");
        match want_body {
            CheckWay::Skip => {}
            CheckWay::Equal(input) => match input {
                Input::Str(_) => panic!("{name}: unexpected want_body input type"),
                Input::Bytes(expect) => assert_eq!(body.as_slice(), expect, "{name}: body"),
            },
            CheckWay::Dynamic(f) => assert!(f(String::from_utf8(body)?.as_str()), "{name}: body"),
        }
        match want_content_length {
            CheckWay::Skip => {}
            CheckWay::Equal(input) => match input {
                Input::Str(expect) => assert_eq!(
                    response.headers().get("content-length").and_then(|value| value.to_str().ok()),
                    Some(expect),
                    "{name}: content-length",
                ),
                Input::Bytes(_) => panic!("{name}: unexpected want_content_length input type"),
            },
            CheckWay::Dynamic(_) => panic!("{name}: unexpected want_content_length input type"),
        }
        assert_eq!(
            response.headers().get("content-range").and_then(|value| value.to_str().ok()),
            want_content_range,
            "{name}: content-range",
        );
        assert!(trailers.is_none(), "{name}: unexpected trailers");
        assert_no_hop_by_hop_headers(&response);
    }

    Ok(())
}

async fn large_object(client: &mut Client) -> TestResult {
    const CHUNK_COUNT: usize = 16;
    const CHUNK_SIZE: usize = 64 * 1024;

    let chunk = Bytes::from(vec![b'x'; CHUNK_SIZE]);
    let content_length = CHUNK_COUNT * CHUNK_SIZE;

    let (response, body, trailers) = send(
        client,
        Request::builder()
            .method(Method::PUT)
            .uri("http://localhost/bucket/large")
            .header("content-length", content_length)
            .body(())?,
        (0..CHUNK_COUNT).map(|_| chunk.clone()),
    )
    .await?;

    assert_eq!(response.status(), StatusCode::OK);
    assert!(body.is_empty());
    assert!(trailers.is_none());

    let (response, body, trailers) = send(
        client,
        Request::builder()
            .method(Method::GET)
            .uri("http://localhost/bucket/large")
            .body(())?,
        std::iter::empty::<Bytes>(),
    )
    .await?;

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(body.len(), content_length);
    assert!(body.iter().all(|&byte| byte == b'x'));
    assert!(trailers.is_none());
    assert_no_hop_by_hop_headers(&response);

    Ok(())
}

async fn multipart_upload(client: &mut Client) -> TestResult {
    let (response, body, trailers) = send(
        client,
        Request::builder()
            .method(Method::POST)
            .uri("http://localhost/bucket/multipart?uploads")
            .body(())?,
        std::iter::empty::<Bytes>(),
    )
    .await?;

    assert_eq!(response.status(), StatusCode::OK);
    assert!(trailers.is_none());
    assert_no_hop_by_hop_headers(&response);

    let upload_id = parse_upload_id(&body)?;
    let part = Bytes::from_static(b"multipart body");

    let (response, body, trailers) = send(
        client,
        Request::builder()
            .method(Method::PUT)
            .uri(format!("http://localhost/bucket/multipart?partNumber=1&uploadId={upload_id}"))
            .header("content-length", part.len())
            .body(())?,
        std::iter::once(part.clone()),
    )
    .await?;

    assert_eq!(response.status(), StatusCode::OK);
    assert!(body.is_empty());
    assert!(trailers.is_none());
    assert_no_hop_by_hop_headers(&response);

    let etag = response
        .headers()
        .get("etag")
        .ok_or_else(|| std::io::Error::other("missing ETag"))?
        .to_str()?
        .to_owned();

    let completion =
        format!("<CompleteMultipartUpload><Part><ETag>{etag}</ETag><PartNumber>1</PartNumber></Part></CompleteMultipartUpload>");

    let (response, _, trailers) = send(
        client,
        Request::builder()
            .method(Method::POST)
            .uri(format!("http://localhost/bucket/multipart?uploadId={upload_id}"))
            .header("content-length", completion.len())
            .body(())?,
        std::iter::once(Bytes::from(completion)),
    )
    .await?;

    assert_eq!(response.status(), StatusCode::OK);
    assert!(trailers.is_some());
    assert_no_hop_by_hop_headers(&response);

    let (response, body, trailers) = send(
        client,
        Request::builder()
            .method(Method::GET)
            .uri("http://localhost/bucket/multipart")
            .body(())?,
        std::iter::empty::<Bytes>(),
    )
    .await?;

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(body.as_slice(), part.as_ref());
    assert!(trailers.is_none());
    assert_no_hop_by_hop_headers(&response);

    Ok(())
}

async fn concurrent_gets(client: &Client) -> TestResult {
    let handles = (0..9)
        .map(|_| {
            let mut client = client.clone();

            tokio::spawn(async move {
                let (response, body, trailers) = send(
                    &mut client,
                    Request::builder()
                        .method(Method::GET)
                        .uri("http://localhost/bucket/key")
                        .body(())?,
                    std::iter::empty::<Bytes>(),
                )
                .await?;

                assert_eq!(response.status(), StatusCode::OK);
                assert_eq!(body, b"hello world");
                assert!(trailers.is_none());
                assert_no_hop_by_hop_headers(&response);

                Ok::<(), Box<dyn Error + Send + Sync>>(())
            })
        })
        .collect::<Vec<_>>();

    for handle in handles {
        handle.await??;
    }

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn serves_put_and_get_over_http3() -> TestResult {
    let root = std::env::temp_dir().join(format!("s3s-http3-{}", std::process::id()));

    if root.exists() {
        std::fs::remove_dir_all(&root)?;
    }
    std::fs::create_dir_all(&root)?;

    let filesystem = FileSystem::new(&root).map_err(|error| std::io::Error::other(format!("{error:?}")))?;
    let mut service_builder = S3ServiceBuilder::new(filesystem);
    service_builder.set_host(s3s::host::SingleDomain::new("localhost")?);
    let service = service_builder.build();

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

    object_operations(&mut send_request).await?;
    large_object(&mut send_request).await?;
    multipart_upload(&mut send_request).await?;
    concurrent_gets(&send_request).await?;

    let _ = shutdown_tx.send(());
    drop(send_request);
    server_task.await?;

    client_endpoint.close(0u32.into(), b"test complete");
    driver.abort();
    let _ = driver.await;

    std::fs::remove_dir_all(root)?;
    Ok(())
}
