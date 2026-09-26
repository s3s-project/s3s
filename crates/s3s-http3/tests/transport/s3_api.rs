// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

//! S3 API behaviour over HTTP/3.

use bytes::Bytes;
use http::{HeaderValue, Method, Request, Response, StatusCode};

use s3s::auth::SimpleAuth;
use s3s::config::{S3Config, StaticConfigProvider};
use s3s::host::SingleDomain;
use s3s::service::S3ServiceBuilder;
use s3s_fs::FileSystem;

use std::error::Error;
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;

use crate::common::{Client, TestResult, connect_client, receive_response, send, server_endpoint, shutdown_server};

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
    extra_headers: &[(&str, &str)],
) -> TestResult<Request<()>> {
    let uri: http::Uri = uri.parse()?;
    let authority = uri
        .authority()
        .ok_or_else(|| std::io::Error::other("signed URI has no authority"))?
        .as_str();

    let amz_date = s3s_sigv4::AmzDate::parse(TEST_AMZ_DATE)?;
    let mut signed_headers = vec![
        ("host", authority),
        ("x-amz-content-sha256", content_sha256),
        ("x-amz-date", TEST_AMZ_DATE),
    ];
    signed_headers.extend(extra_headers.iter().copied());
    signed_headers.sort_unstable_by_key(|(name, _)| *name);
    let signed_header_names = signed_headers.iter().map(|(name, _)| *name).collect::<Vec<_>>().join(";");
    let canonical_request =
        s3s_sigv4::create_canonical_request(method.as_str(), uri.path(), &[] as &[(&str, &str)], signed_headers, payload);

    let string_to_sign = s3s_sigv4::create_string_to_sign(&canonical_request, &amz_date, TEST_REGION, "s3");
    let signature = s3s_sigv4::calculate_signature(&string_to_sign, TEST_SECRET_KEY, &amz_date, TEST_REGION, "s3");

    let authorization = format!(
        "AWS4-HMAC-SHA256 Credential={TEST_ACCESS_KEY}/{}/{TEST_REGION}/s3/aws4_request, \
           SignedHeaders={signed_header_names}, Signature={signature}",
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
    for &(name, value) in extra_headers {
        builder = builder.header(name, value);
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

/// RFC 9114 §4.2 forbids connection-specific fields in HTTP/3; `TE` is the only
/// exception and may only carry the `trailers` member.
async fn connection_specific_fields(client: &mut Client) -> TestResult {
    let rejected: [(&str, HeaderValue); 7] = [
        ("connection", HeaderValue::from_static("keep-alive")),
        ("transfer-encoding", HeaderValue::from_static("chunked")),
        ("upgrade", HeaderValue::from_static("h2c")),
        ("keep-alive", HeaderValue::from_static("timeout=5")),
        ("proxy-connection", HeaderValue::from_static("keep-alive")),
        // Other members stay rejected: the only allowed value is `trailers`.
        ("te", HeaderValue::from_static("trailers, deflate")),
        // Not a valid token, and never equal to `trailers`.
        ("te", HeaderValue::from_bytes(b"trailers\xff")?),
    ];

    for (name, value) in rejected {
        let request = Request::builder()
            .method(Method::GET)
            .uri("http://localhost/bucket/key")
            .header(name, value)
            .body(())?;

        // Read the response without finishing the request: the rejection must
        // arrive as a malformed-message stream error, not as a response.
        let mut stream = client.send_request(request).await?;
        let error = stream
            .recv_response()
            .await
            .expect_err("a malformed request must not be answered");

        assert!(
            matches!(
                &error,
                h3::error::StreamError::RemoteTerminate { code, .. } if *code == h3::error::Code::H3_MESSAGE_ERROR
            ),
            "{name}: unexpected rejection: {error:?}",
        );
    }

    // `TE: trailers` is the one connection-specific field HTTP/3 permits. The
    // ABNF literal is case-insensitive (RFC 5234 §2.3) and surrounding field
    // whitespace is excluded before the value is evaluated (RFC 9110 §5.5).
    for value in ["trailers", "Trailers", "TRAILERS", "trailers ", "\ttrailers\t"] {
        let request = Request::builder()
            .method(Method::GET)
            .uri("http://localhost/bucket/key")
            .header("te", value)
            .body(())?;

        let (response, body, trailers) = send(client, request, std::iter::empty::<Bytes>()).await?;

        assert_eq!(response.status(), StatusCode::OK, "te: {value:?} must be accepted");
        assert_eq!(body, b"hello world", "te: {value:?}");
        assert!(trailers.is_none(), "te: {value:?}");
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
    assert_eq!(body, [] as [u8; 0]);
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
    assert_eq!(body, [] as [u8; 0]);
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
    let decoded_content_length = data.len().to_string();

    let mut request = signed_request(
        Method::PUT,
        "http://localhost/bucket/checksum",
        Some(encoded_body.len()),
        "STREAMING-UNSIGNED-PAYLOAD-TRAILER",
        s3s_sigv4::Payload::UnsignedMultipleChunksWithTrailer,
        &[
            ("x-amz-decoded-content-length", decoded_content_length.as_str()),
            ("x-amz-trailer", "x-amz-checksum-crc32c"),
            ("x-amz-checksum-algorithm", "CRC32C"),
        ],
    )?;

    request
        .headers_mut()
        .insert("content-encoding", HeaderValue::from_static("aws-chunked"));

    let (response, body, trailers) = send(client, request, std::iter::once(encoded_body)).await?;

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(body, [] as [u8; 0]);
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
        &[
            ("x-amz-decoded-content-length", "3"),
            ("x-amz-trailer", "x-amz-checksum-crc32c"),
            ("x-amz-checksum-algorithm", "CRC32C"),
        ],
    )?;

    request
        .headers_mut()
        .insert("content-encoding", HeaderValue::from_static("aws-chunked"));

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

async fn content_length_mismatch(client: &mut Client) -> TestResult {
    for (key, length, body) in [
        ("short", "2", Bytes::from_static(b"x")),
        ("long", "1", Bytes::from_static(b"xy")),
    ] {
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            send(
                client,
                Request::builder()
                    .method(Method::PUT)
                    .uri(format!("http://localhost/bucket/{key}"))
                    .header("content-length", length)
                    .body(())?,
                std::iter::once(body),
            ),
        )
        .await?;

        // Rejecting the upload races with the client's own sends: the client
        // either reads the S3 error response or observes the request stream
        // being reset. Both mean the request was rejected, so the assertions
        // below pin what must not happen instead: the object must not be stored
        // and the connection must stay usable.
        match result {
            Ok((response, body, trailers)) => {
                // s3s-fs maps the transport body error to an S3 InternalError.
                assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR, "{key}");
                assert!(String::from_utf8(body)?.contains("<Code>InternalError</Code>"), "{key}");
                assert!(trailers.is_none(), "{key}");
            }
            Err(error) => {
                assert!(
                    matches!(
                        error.downcast_ref::<h3::error::StreamError>(),
                        Some(h3::error::StreamError::RemoteTerminate { code, .. })
                            if *code == h3::error::Code::H3_MESSAGE_ERROR
                    ),
                    "{key}: unexpected upload error: {error:?}",
                );
            }
        }

        // A rejected upload must not be published, and the connection must remain usable.
        let (response, body, _) = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            send(
                client,
                Request::builder()
                    .method(Method::GET)
                    .uri(format!("http://localhost/bucket/{key}"))
                    .body(())?,
                std::iter::empty::<Bytes>(),
            ),
        )
        .await??;
        assert_eq!(response.status(), StatusCode::NOT_FOUND, "{key}: malformed upload was stored");
        assert!(String::from_utf8(body)?.contains("<Code>NoSuchKey</Code>"), "{key}");
    }

    Ok(())
}

/// Removes the temporary data directory when the test ends.
struct CleanupGuard {
    path: PathBuf,
}

impl Drop for CleanupGuard {
    fn drop(&mut self) {
        if self.path.exists() {
            let _ = fs::remove_dir_all(&self.path);
        }
    }
}

/// Serves an `S3Service` over HTTP/3 on a temporary data directory.
struct S3Harness {
    client_endpoint: quinn::Endpoint,
    driver: tokio::task::JoinHandle<()>,
    shutdown: tokio::sync::oneshot::Sender<()>,
    server: tokio::task::JoinHandle<()>,
    client: Client,
    _cleanup: CleanupGuard,
}

impl S3Harness {
    /// Starts a server without authentication.
    async fn new(name: &str) -> TestResult<Self> {
        Self::start(name, false).await
    }

    /// Starts a server with `SigV4` authentication enabled.
    async fn with_auth(name: &str) -> TestResult<Self> {
        Self::start(name, true).await
    }

    async fn start(name: &str, auth: bool) -> TestResult<Self> {
        let root = std::env::temp_dir().join(format!("s3s-http3-{name}-{}", std::process::id()));

        if root.exists() {
            fs::remove_dir_all(&root)?;
        }
        fs::create_dir_all(&root)?;

        let filesystem = FileSystem::new(&root).map_err(|error| std::io::Error::other(format!("{error:?}")))?;

        let mut builder = S3ServiceBuilder::new(filesystem);
        builder.set_host(SingleDomain::new("localhost")?);

        if auth {
            let mut config = S3Config::default();
            config.presigned_url_max_skew_time_secs = u32::MAX;
            builder.set_auth(SimpleAuth::from_single(TEST_ACCESS_KEY, TEST_SECRET_KEY));
            builder.set_config(Arc::new(StaticConfigProvider::new(Arc::new(config))));
        }

        let service = builder.build();

        let (endpoint, certificate) = server_endpoint()?;
        let server_address = endpoint.local_addr()?;

        let (shutdown, shutdown_rx) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(s3s_http3::serve(endpoint, service, async move {
            let _ = shutdown_rx.await;
        }));

        let (client_endpoint, driver, client) = connect_client(server_address, certificate).await?;

        Ok(Self {
            client_endpoint,
            driver,
            shutdown,
            server,
            client,
            _cleanup: CleanupGuard { path: root },
        })
    }

    async fn finish(self) -> TestResult<()> {
        shutdown_server(self.shutdown, self.client, self.server, self.client_endpoint, self.driver).await
    }
}

/// Creates the bucket and the key object with unsigned requests.
async fn seed_bucket(client: &mut Client) -> TestResult {
    let (response, body, _) = send(
        client,
        Request::builder()
            .method(Method::PUT)
            .uri("http://localhost/bucket")
            .body(())?,
        std::iter::empty::<Bytes>(),
    )
    .await?;
    assert_eq!(
        response.status(),
        StatusCode::OK,
        "create bucket failed: {}",
        String::from_utf8_lossy(&body)
    );

    let object = Bytes::from_static(b"hello world");
    let (response, _, _) = send(
        client,
        Request::builder()
            .method(Method::PUT)
            .uri("http://localhost/bucket/key")
            .header("content-length", object.len())
            .body(())?,
        [object],
    )
    .await?;
    assert_eq!(response.status(), StatusCode::OK);

    Ok(())
}

/// Creates the bucket and the key object with signed requests, for the authenticated server.
async fn seed_bucket_signed(client: &mut Client) -> TestResult {
    let (response, body, _) = send(
        client,
        signed_request(
            Method::PUT,
            "http://localhost/bucket",
            Some(0),
            "UNSIGNED-PAYLOAD",
            s3s_sigv4::Payload::Unsigned,
            &[],
        )?,
        std::iter::empty::<Bytes>(),
    )
    .await?;
    assert_eq!(
        response.status(),
        StatusCode::OK,
        "create bucket (signed) failed: {}",
        String::from_utf8_lossy(&body)
    );

    let object = Bytes::from_static(b"hello world");
    let (response, _, _) = send(
        client,
        signed_request(
            Method::PUT,
            "http://localhost/bucket/key",
            Some(object.len()),
            "UNSIGNED-PAYLOAD",
            s3s_sigv4::Payload::Unsigned,
            &[],
        )?,
        [object],
    )
    .await?;
    assert_eq!(response.status(), StatusCode::OK);

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn lists_puts_and_gets_objects() -> TestResult {
    let mut harness = S3Harness::new("object-operations").await?;
    object_operations(&mut harness.client).await?;
    harness.finish().await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rejects_connection_specific_request_fields() -> TestResult {
    let mut harness = S3Harness::new("connection-fields").await?;
    seed_bucket(&mut harness.client).await?;
    connection_specific_fields(&mut harness.client).await?;
    harness.finish().await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn handles_a_request_stream_reset() -> TestResult {
    let mut harness = S3Harness::new("stream-reset").await?;
    seed_bucket(&mut harness.client).await?;
    request_stream_reset(&mut harness.client).await?;
    harness.finish().await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reports_a_content_length_mismatch() -> TestResult {
    let mut harness = S3Harness::new("length-mismatch").await?;
    seed_bucket(&mut harness.client).await?;
    content_length_mismatch(&mut harness.client).await?;
    harness.finish().await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn transfers_a_large_object() -> TestResult {
    let mut harness = S3Harness::new("large-object").await?;
    seed_bucket(&mut harness.client).await?;
    large_object(&mut harness.client).await?;
    harness.finish().await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn uploads_a_multipart_object() -> TestResult {
    let mut harness = S3Harness::new("multipart").await?;
    seed_bucket(&mut harness.client).await?;
    multipart_upload(&mut harness.client).await?;
    harness.finish().await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn serves_concurrent_gets() -> TestResult {
    let mut harness = S3Harness::new("concurrent-gets").await?;
    seed_bucket(&mut harness.client).await?;
    concurrent_gets(&harness.client).await?;
    harness.finish().await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn accepts_a_streaming_checksum_upload() -> TestResult {
    let mut harness = S3Harness::with_auth("streaming-checksum").await?;
    seed_bucket_signed(&mut harness.client).await?;
    streaming_checksum_put(&mut harness.client).await?;
    harness.finish().await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rejects_a_truncated_streaming_upload() -> TestResult {
    let mut harness = S3Harness::with_auth("streaming-truncated").await?;
    seed_bucket_signed(&mut harness.client).await?;
    streaming_truncated_put(&mut harness.client).await?;
    harness.finish().await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn drains_the_active_request_before_shutdown() -> TestResult {
    let mut harness = S3Harness::new("drain").await?;
    seed_bucket(&mut harness.client).await?;

    let mut active_stream = harness
        .client
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
        &mut harness.client,
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

    let _ = harness.shutdown.send(());

    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(100), &mut harness.server)
            .await
            .is_err(),
        "server stopped before the active request drained",
    );

    active_stream.send_data(Bytes::from_static(b"c")).await?;
    active_stream.finish().await?;

    // Delay reading the final response: queuing it must not close the connection.
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(100), &mut harness.server)
            .await
            .is_err(),
        "server closed before the client read the final response",
    );

    let (response, body, trailers) =
        tokio::time::timeout(std::time::Duration::from_secs(2), receive_response(active_stream)).await??;

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(body, [] as [u8; 0]);
    assert!(trailers.is_none());
    assert_no_hop_by_hop_headers(&response);

    // The server finishes once the client handle is dropped and the drained request completes.
    drop(harness.client);
    tokio::time::timeout(std::time::Duration::from_secs(2), &mut harness.server).await??;

    harness.client_endpoint.close(0u32.into(), b"test complete");
    harness.driver.abort();
    let _ = harness.driver.await;

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn preserves_sigv4_authority_over_http3() -> TestResult {
    let mut harness = S3Harness::with_auth("sigv4-authority").await?;
    let client = &mut harness.client;

    let (response, body, trailers) = send(
        client,
        signed_request(
            Method::PUT,
            "http://localhost/bucket",
            Some(0),
            "UNSIGNED-PAYLOAD",
            s3s_sigv4::Payload::Unsigned,
            &[],
        )?,
        std::iter::empty::<Bytes>(),
    )
    .await?;

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(body, [] as [u8; 0]);
    assert!(trailers.is_none());
    assert_no_hop_by_hop_headers(&response);

    let object = Bytes::from_static(b"authority body");

    let (response, body, trailers) = send(
        client,
        signed_request(
            Method::PUT,
            "http://localhost/bucket/key",
            Some(object.len()),
            "UNSIGNED-PAYLOAD",
            s3s_sigv4::Payload::Unsigned,
            &[],
        )?,
        std::iter::once(object),
    )
    .await?;

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(body, [] as [u8; 0]);
    assert!(trailers.is_none());
    assert_no_hop_by_hop_headers(&response);

    let request = signed_request(
        Method::GET,
        "http://bucket.localhost/key",
        Some(0),
        "UNSIGNED-PAYLOAD",
        s3s_sigv4::Payload::Unsigned,
        &[],
    )?;
    assert!(!request.headers().contains_key(http::header::HOST));

    let (response, body, trailers) = send(client, request, std::iter::empty::<Bytes>()).await?;

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(body, b"authority body");
    assert!(trailers.is_none());
    assert_no_hop_by_hop_headers(&response);

    harness.finish().await
}
