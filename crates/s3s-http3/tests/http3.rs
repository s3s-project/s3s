// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 The s3s Authors

use bytes::{Buf, Bytes};
use h3::client::RequestStream;
use http::{HeaderMap, HeaderValue, Method, Request, Response, StatusCode};
use quinn::rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};

use s3s::auth::SimpleAuth;
use s3s::config::{S3Config, StaticConfigProvider};
use s3s::host::SingleDomain;
use s3s::service::S3ServiceBuilder;
use s3s_fs::FileSystem;

use std::error::Error;
use std::fs;
use std::path::Path;
use std::sync::Arc;

type TestResult<T = ()> = Result<T, Box<dyn Error + Send + Sync>>;
type ClientStream = RequestStream<h3_quinn::BidiStream<Bytes>, Bytes>;
type Client = h3::client::SendRequest<h3_quinn::OpenStreams, Bytes>;
type ResponseData = (Response<()>, Vec<u8>, Option<HeaderMap>);

const TEST_ACCESS_KEY: &str = "AKIAHTTP3TEST";
const TEST_SECRET_KEY: &str = "http3-test-secret";
const TEST_AMZ_DATE: &str = "20130524T000000Z";
const TEST_REGION: &str = "us-east-1";

fn signed_request(
    method: Method,
    uri: &str,
    content_length: Option<usize>,
    content_sha256: &str,
    payload: s3s_sigv4::Payload<'_>,
) -> TestResult<Request<()>> {
    let uri: http::Uri = uri.parse()?;
    let authority = uri
        .authority()
        .ok_or_else(|| std::io::Error::other("signed URI has no authority"))?
        .as_str();

    let amz_date = s3s_sigv4::AmzDate::parse(TEST_AMZ_DATE)?;
    let canonical_request = s3s_sigv4::create_canonical_request(
        method.as_str(),
        uri.path(),
        &[] as &[(&str, &str)],
        [
            ("host", authority),
            ("x-amz-content-sha256", content_sha256),
            ("x-amz-date", TEST_AMZ_DATE),
        ],
        payload,
    );

    let string_to_sign = s3s_sigv4::create_string_to_sign(&canonical_request, &amz_date, TEST_REGION, "s3");
    let signature = s3s_sigv4::calculate_signature(&string_to_sign, TEST_SECRET_KEY, &amz_date, TEST_REGION, "s3");

    let authorization = format!(
        "AWS4-HMAC-SHA256 Credential={TEST_ACCESS_KEY}/{}/{TEST_REGION}/s3/aws4_request, \
           SignedHeaders=host;x-amz-content-sha256;x-amz-date, Signature={signature}",
        amz_date.fmt_date(),
    );

    let mut builder = Request::builder()
        .method(method)
        .uri(uri)
        .header("authorization", authorization)
        .header("x-amz-content-sha256", content_sha256)
        .header("x-amz-date", TEST_AMZ_DATE);

    if let Some(length) = content_length {
        builder = builder.header("content-length", length);
    }

    Ok(builder.body(())?)
}

fn crc32c_base64(data: &[u8]) -> TestResult<String> {
    let mut hasher = s3s::checksum::ChecksumHasher {
        crc32c: Some(s3s::crypto::Crc32c::default()),
        ..Default::default()
    };

    hasher.update(data);

    Ok(hasher
        .finalize()
        .checksum_crc32c
        .ok_or_else(|| std::io::Error::other("CRC32C was not computed"))?)
}

fn unsigned_aws_chunked_body(data: &[u8], checksum: &str) -> Bytes {
    let mut body = Vec::new();

    body.extend_from_slice(format!("{:x}\r\n", data.len()).as_bytes());
    body.extend_from_slice(data);
    body.extend_from_slice(b"\r\n0\r\n\r\n");
    body.extend_from_slice(format!("x-amz-checksum-crc32c:{checksum}\r\n").as_bytes());

    body.into()
}

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

async fn streaming_checksum_put(client: &mut Client) -> TestResult {
    let data = Bytes::from_static(b"streamed checksum");
    let checksum = crc32c_base64(&data)?;
    let encoded_body = unsigned_aws_chunked_body(&data, &checksum);

    let mut request = signed_request(
        Method::PUT,
        "http://localhost/bucket/checksum",
        Some(encoded_body.len()),
        "STREAMING-UNSIGNED-PAYLOAD-TRAILER",
        s3s_sigv4::Payload::UnsignedMultipleChunksWithTrailer,
    )?;

    request
        .headers_mut()
        .insert("content-encoding", HeaderValue::from_static("aws-chunked"));
    request
        .headers_mut()
        .insert("x-amz-decoded-content-length", HeaderValue::from_str(&data.len().to_string())?);
    request
        .headers_mut()
        .insert("x-amz-trailer", HeaderValue::from_static("x-amz-checksum-crc32c"));
    request
        .headers_mut()
        .insert("x-amz-checksum-algorithm", HeaderValue::from_static("CRC32C"));

    let (response, body, trailers) = send(client, request, std::iter::once(encoded_body)).await?;

    assert_eq!(response.status(), StatusCode::OK);
    assert!(body.is_empty());
    assert!(trailers.is_none());
    assert_eq!(
        response
            .headers()
            .get("x-amz-checksum-crc32c")
            .and_then(|value| value.to_str().ok()),
        Some(checksum.as_str()),
    );
    assert_no_hop_by_hop_headers(&response);

    Ok(())
}

async fn streaming_truncated_put(client: &mut Client) -> TestResult {
    // Missing the CRLF after the declared chunk data.
    let truncated_body = Bytes::from_static(b"3\r\nabc");

    let mut request = signed_request(
        Method::PUT,
        "http://localhost/bucket/truncated",
        Some(truncated_body.len()),
        "STREAMING-UNSIGNED-PAYLOAD-TRAILER",
        s3s_sigv4::Payload::UnsignedMultipleChunksWithTrailer,
    )?;

    request
        .headers_mut()
        .insert("content-encoding", HeaderValue::from_static("aws-chunked"));
    request
        .headers_mut()
        .insert("x-amz-decoded-content-length", HeaderValue::from_static("3"));
    request
        .headers_mut()
        .insert("x-amz-trailer", HeaderValue::from_static("x-amz-checksum-crc32c"));
    request
        .headers_mut()
        .insert("x-amz-checksum-algorithm", HeaderValue::from_static("CRC32C"));

    let (response, body, trailers) =
        tokio::time::timeout(std::time::Duration::from_secs(2), send(client, request, std::iter::once(truncated_body))).await??;

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert!(trailers.is_none());
    assert!(
        String::from_utf8(body)?.contains("<Code>IncompleteBody</Code>"),
        "expected IncompleteBody response",
    );
    assert_no_hop_by_hop_headers(&response);

    Ok(())
}

async fn request_stream_reset(client: &mut Client) -> TestResult {
    let mut stream = client
        .send_request(
            Request::builder()
                .method(Method::PUT)
                .uri("http://localhost/bucket/reset")
                .body(())?,
        )
        .await?;

    // Reset the client-to-server request direction before sending a body.
    stream.stop_stream(h3::error::Code::H3_REQUEST_CANCELLED);
    drop(stream);

    // Confirm the connection remains usable.
    let (response, body, trailers) = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        send(
            client,
            Request::builder()
                .method(Method::GET)
                .uri("http://localhost/bucket/key")
                .body(())?,
            std::iter::empty::<Bytes>(),
        ),
    )
    .await??;

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(body, b"hello world");
    assert!(trailers.is_none());
    assert_no_hop_by_hop_headers(&response);

    Ok(())
}

struct CleanupGuard<'a> {
    path: &'a Path,
}

impl Drop for CleanupGuard<'_> {
    fn drop(&mut self) {
        if self.path.exists() {
            let _ = fs::remove_dir_all(self.path);
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn serves_put_and_get_over_http3() -> TestResult {
    let root = std::env::temp_dir().join(format!("s3s-http3-{}", std::process::id()));

    if root.exists() {
        std::fs::remove_dir_all(&root)?;
    }
    fs::create_dir_all(&root)?;
    let _guard = CleanupGuard { path: root.as_path() };

    let filesystem = FileSystem::new(&root).map_err(|error| std::io::Error::other(format!("{error:?}")))?;
    let mut service_builder = S3ServiceBuilder::new(filesystem);
    service_builder.set_host(s3s::host::SingleDomain::new("localhost")?);
    let service = service_builder.build();

    let (endpoint, certificate) = server_endpoint()?;
    let server_address = endpoint.local_addr()?;

    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
    let mut server_task = tokio::spawn(s3s_http3::serve(endpoint, service, async move {
        let _ = shutdown_rx.await;
    }));

    let client_endpoint = client_endpoint(certificate)?;
    let connection = client_endpoint.connect(server_address, "localhost")?.await?;

    let (mut h3_connection, mut send_request) = h3::client::builder().build(h3_quinn::Connection::new(connection)).await?;

    let driver = tokio::spawn(async move {
        let _ = std::future::poll_fn(|cx| h3_connection.poll_close(cx)).await;
    });

    object_operations(&mut send_request).await?;
    request_stream_reset(&mut send_request).await?;
    large_object(&mut send_request).await?;
    multipart_upload(&mut send_request).await?;
    concurrent_gets(&send_request).await?;

    // graceful shutdown test
    let mut active_stream = send_request
        .send_request(
            Request::builder()
                .method(Method::PUT)
                .uri("http://localhost/bucket/draining")
                .header("content-length", 3)
                .body(())?,
        )
        .await?;
    active_stream.send_data(Bytes::from_static(b"ab")).await?;

    // force the server to accept the active stream before shutdown.
    let (probe_response, probe_body, probe_trailers) = send(
        &mut send_request,
        Request::builder()
            .method(Method::GET)
            .uri("http://localhost/bucket/key")
            .body(())?,
        std::iter::empty::<Bytes>(),
    )
    .await?;

    assert_eq!(probe_response.status(), StatusCode::OK);
    assert_eq!(probe_body, b"hello world");
    assert!(probe_trailers.is_none());

    let _ = shutdown_tx.send(());

    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(100), &mut server_task)
            .await
            .is_err(),
        "server stopped before the active request drained",
    );

    active_stream.send_data(Bytes::from_static(b"c")).await?;
    active_stream.finish().await?;

    let (response, body, trailers) =
        tokio::time::timeout(std::time::Duration::from_secs(2), receive_response(active_stream)).await??;

    assert_eq!(response.status(), StatusCode::OK);
    assert!(body.is_empty());
    assert!(trailers.is_none());
    assert_no_hop_by_hop_headers(&response);

    drop(send_request);
    tokio::time::timeout(std::time::Duration::from_secs(2), &mut server_task).await??;

    client_endpoint.close(0u32.into(), b"test complete");
    driver.abort();
    let _ = driver.await;

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn preserves_sigv4_authority_over_http3() -> TestResult {
    let root = std::env::temp_dir().join(format!("s3s-http3-sigv4-{}", std::process::id()));

    if root.exists() {
        fs::remove_dir_all(&root)?;
    }
    fs::create_dir_all(&root)?;
    let _guard = CleanupGuard { path: root.as_path() };

    let filesystem = FileSystem::new(&root).map_err(|error| std::io::Error::other(format!("{error:?}")))?;

    let mut config = S3Config::default();
    config.presigned_url_max_skew_time_secs = u32::MAX;

    let mut builder = S3ServiceBuilder::new(filesystem);
    builder.set_host(SingleDomain::new("localhost")?);
    builder.set_auth(SimpleAuth::from_single(TEST_ACCESS_KEY, TEST_SECRET_KEY));
    builder.set_config(Arc::new(StaticConfigProvider::new(Arc::new(config))));
    let service = builder.build();

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

    let (response, body, trailers) = send(
        &mut send_request,
        signed_request(
            Method::PUT,
            "http://localhost/bucket",
            Some(0),
            "UNSIGNED-PAYLOAD",
            s3s_sigv4::Payload::Unsigned,
        )?,
        std::iter::empty::<Bytes>(),
    )
    .await?;

    assert_eq!(response.status(), StatusCode::OK);
    assert!(body.is_empty());
    assert!(trailers.is_none());
    assert_no_hop_by_hop_headers(&response);

    let object = Bytes::from_static(b"authority body");

    let (response, body, trailers) = send(
        &mut send_request,
        signed_request(
            Method::PUT,
            "http://localhost/bucket/key",
            Some(object.len()),
            "UNSIGNED-PAYLOAD",
            s3s_sigv4::Payload::Unsigned,
        )?,
        std::iter::once(object),
    )
    .await?;

    assert_eq!(response.status(), StatusCode::OK);
    assert!(body.is_empty());
    assert!(trailers.is_none());
    assert_no_hop_by_hop_headers(&response);

    let request = signed_request(
        Method::GET,
        "http://bucket.localhost/key",
        Some(0),
        "UNSIGNED-PAYLOAD",
        s3s_sigv4::Payload::Unsigned,
    )?;
    assert!(!request.headers().contains_key(http::header::HOST));

    let (response, body, trailers) = send(&mut send_request, request, std::iter::empty::<Bytes>()).await?;

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(body, b"authority body");
    assert!(trailers.is_none());
    assert_no_hop_by_hop_headers(&response);

    streaming_checksum_put(&mut send_request).await?;
    streaming_truncated_put(&mut send_request).await?;

    let _ = shutdown_tx.send(());
    drop(send_request);
    server_task.await?;

    client_endpoint.close(0u32.into(), b"test complete");
    driver.abort();
    let _ = driver.await;

    Ok(())
}
