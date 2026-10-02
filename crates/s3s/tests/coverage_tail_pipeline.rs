// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

//! Coverage tests for the request pipeline: service plumbing, routing, header
//! and XML body deserialization, and the full-body extraction paths.

#![allow(clippy::too_many_lines)]

use std::collections::VecDeque;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll, Waker};

use bytes::Bytes;
use futures::Stream;
use http::{Method, Request, StatusCode};
use http_body_util::BodyExt;
use s3s::auth::SimpleAuth;
use s3s::config::{S3Config, StaticConfigProvider};
use s3s::dto::{
    CompleteMultipartUploadInput, CompleteMultipartUploadOutput, GetObjectInput, GetObjectOutput, PutObjectInput, PutObjectOutput,
};
use s3s::host::{S3Host, VirtualHost};
use s3s::service::{S3Service, S3ServiceBuilder};
use s3s::stream::{ByteStream, DynByteStream};
use s3s::{Body, HttpRequest, S3, S3Error, S3ErrorCode, S3Request, S3Response, S3Result, StdError};

#[derive(Clone)]
struct TestS3;

#[async_trait::async_trait]
impl S3 for TestS3 {
    async fn get_object(&self, _req: S3Request<GetObjectInput>) -> S3Result<S3Response<GetObjectOutput>> {
        Ok(S3Response::new(GetObjectOutput::default()))
    }

    async fn put_object(&self, mut req: S3Request<PutObjectInput>) -> S3Result<S3Response<PutObjectOutput>> {
        // Drain the streaming body so the request-body plumbing runs to the end.
        let mut len = 0_usize;
        if let Some(body) = req.input.body.as_mut() {
            while let Some(chunk) = futures::StreamExt::next(body).await {
                len += chunk.map_err(|e| S3Error::with_source(S3ErrorCode::InternalError, e))?.len();
            }
        }
        assert!(len <= 64, "unexpected payload length: {len}");
        Ok(S3Response::new(PutObjectOutput::default()))
    }

    async fn complete_multipart_upload(
        &self,
        _req: S3Request<CompleteMultipartUploadInput>,
    ) -> S3Result<S3Response<CompleteMultipartUploadOutput>> {
        // The deferred result carries a header value that cannot be serialized,
        // so the keep-alive body surfaces the failure while streaming.
        let deferred = CompleteMultipartUploadOutput {
            expiration: Some("bad\nheader".to_owned()),
            ..Default::default()
        };
        let output = CompleteMultipartUploadOutput {
            future: Some(Box::pin(async move { Ok(deferred) })),
            ..Default::default()
        };
        Ok(S3Response::new(output))
    }
}

/// A host parser that reports a fixed region for every host.
struct RegionHost;

impl S3Host for RegionHost {
    fn parse_host_header<'a>(&'a self, host: &'a str) -> S3Result<VirtualHost<'a>> {
        Ok(VirtualHost::new(host).with_region("us-east-1"))
    }
}

fn service_with(s3: impl S3, config: S3Config) -> S3Service {
    let mut builder = S3ServiceBuilder::new(s3);
    builder.set_config(Arc::new(StaticConfigProvider::new(Arc::new(config))));
    builder.build()
}

async fn send(service: &S3Service, req: HttpRequest) -> (StatusCode, String) {
    let resp = service.call(req).await.expect("service call returns a response");
    let status = resp.status();
    let bytes = resp.into_body().collect().await.expect("body collects").to_bytes();
    (status, String::from_utf8_lossy(&bytes).into_owned())
}

fn get(uri: &str) -> http::request::Builder {
    Request::builder().method(Method::GET).uri(uri)
}

/// A request body that streams the scripted chunks instead of holding them in
/// memory, so the body is never pre-buffered.
struct ChunkStream {
    items: VecDeque<Result<Bytes, StdError>>,
}

impl Stream for ChunkStream {
    type Item = Result<Bytes, StdError>;

    fn poll_next(mut self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        Poll::Ready(self.items.pop_front())
    }
}

impl ByteStream for ChunkStream {}

fn streaming_body(items: Vec<Result<Bytes, StdError>>) -> Body {
    let stream = ChunkStream {
        items: items.into_iter().collect(),
    };
    let stream: DynByteStream = Box::pin(stream);
    Body::from(stream)
}

fn chunks(items: &[&'static [u8]]) -> Body {
    let items: Vec<Result<Bytes, StdError>> = items.iter().map(|b| Ok(Bytes::from_static(b))).collect();
    streaming_body(items)
}

// ---------------------------------------------------------------------------
// Service plumbing
// ---------------------------------------------------------------------------

#[test]
fn tower_service_accepts_a_generic_body() {
    let mut service = service_with(TestS3, S3Config::default());

    let mut cx = Context::from_waker(Waker::noop());
    let ready = <S3Service as tower::Service<http::Request<http_body_util::Full<Bytes>>>>::poll_ready(&mut service, &mut cx);
    assert!(matches!(ready, Poll::Ready(Ok(()))), "{ready:?}");

    // A body type whose error is a concrete error type satisfies the tower
    // service bounds (the erased body error of `s3s::Body` does not).
    let req = Request::builder()
        .method(Method::PUT)
        .uri("/bucket/key")
        // A payload-bearing request declares its length, as the service requires.
        .header(hyper::header::CONTENT_LENGTH, "5")
        .body(http_body_util::Full::new(Bytes::from_static(b"hello")))
        .expect("valid request");
    let resp = futures::executor::block_on(<S3Service as tower::Service<http::Request<http_body_util::Full<Bytes>>>>::call(
        &mut service,
        req,
    ))
    .expect("tower call returns a response");
    assert_eq!(resp.status(), StatusCode::OK);
}

// ---------------------------------------------------------------------------
// Path, query and header extraction
// ---------------------------------------------------------------------------

#[tokio::test]
async fn request_with_an_asterisk_path_reports_invalid_uri() {
    let service = service_with(TestS3, S3Config::default());
    let req = Request::builder()
        .method(Method::OPTIONS)
        .uri("*")
        .body(Body::empty())
        .expect("valid request");
    let (status, body) = send(&service, req).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body.contains("<Code>InvalidURI</Code>"), "{body}");
}

#[tokio::test]
async fn empty_content_type_header_is_ignored() {
    let service = service_with(TestS3, S3Config::default());
    let req = get("/bucket/key")
        .header("content-type", "")
        .body(Body::empty())
        .expect("valid request");
    let (status, _) = send(&service, req).await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn invalid_optional_header_value_reports_invalid_argument() {
    let service = service_with(TestS3, S3Config::default());
    // Neither a `Range` field (see `tests/range_header.rs`) nor a conditional
    // date field is rejected any more when it cannot be served: both are ignored.
    // Use a header whose value still fails to parse, so this generic path stays
    // covered.
    let req = Request::builder()
        .method(Method::PUT)
        .uri("/bucket/key")
        .header("content-length", "0")
        .header("x-amz-object-lock-retain-until-date", "not-a-date")
        .body(Body::empty())
        .expect("valid request");
    let (status, body) = send(&service, req).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body.contains("<Code>InvalidArgument</Code>"), "{body}");
    assert!(body.contains("invalid header: x-amz-object-lock-retain-until-date"), "{body}");
}

#[tokio::test]
async fn non_utf8_string_header_reports_invalid_argument() {
    let service = service_with(TestS3, S3Config::default());
    let raw = http::HeaderValue::from_bytes(b"\xff\xfe").expect("opaque header value");
    let req = get("/bucket/key")
        .header("x-amz-expected-bucket-owner", raw)
        .body(Body::empty())
        .expect("valid request");
    let (status, body) = send(&service, req).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body.contains("<Code>InvalidArgument</Code>"), "{body}");
    assert!(body.contains("invalid header: x-amz-expected-bucket-owner"), "{body}");
}

// ---------------------------------------------------------------------------
// XML body deserialization
// ---------------------------------------------------------------------------

#[tokio::test]
async fn malformed_xml_body_reports_malformed_xml() {
    let service = service_with(TestS3, S3Config::default());
    let req = Request::builder()
        .method(Method::POST)
        .uri("/bucket?delete")
        .body(Body::from("<Delete><Object>".to_owned()))
        .expect("valid request");
    let (status, body) = send(&service, req).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body.contains("<Code>MalformedXML</Code>"), "{body}");
}

#[tokio::test]
async fn malformed_optional_xml_body_reports_malformed_xml() {
    let service = service_with(TestS3, S3Config::default());
    let req = Request::builder()
        .method(Method::PUT)
        .uri("/bucket?acl")
        .body(Body::from("definitely not xml".to_owned()))
        .expect("valid request");
    let (status, body) = send(&service, req).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body.contains("<Code>MalformedXML</Code>"), "{body}");
}

#[tokio::test]
async fn empty_optional_xml_body_is_accepted() {
    let service = service_with(TestS3, S3Config::default());
    let req = Request::builder()
        .method(Method::PUT)
        .uri("/bucket?acl")
        .header("content-length", "0")
        .body(Body::empty())
        .expect("valid request");
    let (status, body) = send(&service, req).await;
    // The operation itself is not implemented by the test double, which proves
    // the empty optional body was accepted by the deserializer.
    assert_eq!(status, StatusCode::NOT_IMPLEMENTED);
    assert!(body.contains("<Code>NotImplemented</Code>"), "{body}");
}

#[tokio::test]
async fn malformed_body_literal_requests_report_malformed_xml() {
    let service = service_with(TestS3, S3Config::default());

    let req = Request::builder()
        .method(Method::PUT)
        .uri("/bucket?versioning")
        .body(Body::from("Disabled".to_owned()))
        .expect("valid request");
    let (status, body) = send(&service, req).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body.contains("<Code>MalformedXML</Code>"), "{body}");

    let req = Request::builder()
        .method(Method::PUT)
        .uri("/bucket?object-lock")
        .body(Body::from("Disabled".to_owned()))
        .expect("valid request");
    let (status, body) = send(&service, req).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body.contains("<Code>MalformedXML</Code>"), "{body}");
}

// ---------------------------------------------------------------------------
// Full body extraction
// ---------------------------------------------------------------------------

#[tokio::test]
async fn streaming_full_body_is_read_and_validated() {
    let service = service_with(TestS3, S3Config::default());
    let req = Request::builder()
        .method(Method::POST)
        .uri("/bucket?delete")
        .header("content-length", "46")
        .body(chunks(&[b"<Delete><Object>", b"<Key>k</Key>", b"</Object></Delete>"]))
        .expect("valid request");
    let (status, body) = send(&service, req).await;
    // The body is accepted and the request reaches the operation.
    assert_eq!(status, StatusCode::NOT_IMPLEMENTED, "{body}");
    assert!(body.contains("<Code>NotImplemented</Code>"), "{body}");
}

#[tokio::test]
async fn streaming_full_body_without_content_length_is_rejected() {
    let service = service_with(TestS3, S3Config::default());
    let req = Request::builder()
        .method(Method::POST)
        .uri("/bucket?delete")
        .header("transfer-encoding", "chunked")
        .body(chunks(&[b"<Delete></Delete>"]))
        .expect("valid request");
    let (status, body) = send(&service, req).await;
    assert_eq!(status, StatusCode::LENGTH_REQUIRED);
    assert!(body.contains("<Code>MissingContentLength</Code>"), "{body}");
}

#[tokio::test]
async fn streaming_full_body_with_mismatched_content_length_is_rejected() {
    let service = service_with(TestS3, S3Config::default());
    let req = Request::builder()
        .method(Method::POST)
        .uri("/bucket?delete")
        .header("content-length", "3")
        .body(chunks(&[b"<Delete></Delete>"]))
        .expect("valid request");
    let (status, body) = send(&service, req).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body.contains("<Code>IncompleteBody</Code>"), "{body}");
}

#[tokio::test]
async fn streaming_full_body_above_the_limit_is_reported_as_internal_error() {
    let mut config = S3Config::default();
    config.xml_max_body_size = 16;
    let service = service_with(TestS3, config);
    let req = Request::builder()
        .method(Method::POST)
        .uri("/bucket?delete")
        .header("content-length", "25")
        .body(chunks(&[b"<Delete></Delete>0123456789"]))
        .expect("valid request");
    let (status, body) = send(&service, req).await;
    // The size limit is enforced by the collecting reader, whose error is not
    // the s3s body-size error, so it surfaces as an internal error.
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert!(body.contains("<Code>InternalError</Code>"), "{body}");
}

#[tokio::test]
async fn streaming_full_body_with_zero_bytes_reaches_the_operation() {
    let service = service_with(TestS3, S3Config::default());
    let req = Request::builder()
        .method(Method::POST)
        .uri("/bucket?delete")
        .header("content-length", "0")
        .body(streaming_body(Vec::new()))
        .expect("valid request");
    let (status, body) = send(&service, req).await;
    // The empty body is read without a length mismatch and rejected later by
    // the operation, which requires an XML document.
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(body.contains("MissingRequestBodyError"), "{body}");
}

#[tokio::test]
async fn streaming_full_body_failure_is_an_internal_error() {
    let service = service_with(TestS3, S3Config::default());
    let items: Vec<Result<Bytes, StdError>> = vec![Err("body stream failed".into())];
    let req = Request::builder()
        .method(Method::POST)
        .uri("/bucket?delete")
        .header("content-length", "4")
        .body(streaming_body(items))
        .expect("valid request");
    let (status, body) = send(&service, req).await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert!(body.contains("<Code>InternalError</Code>"), "{body}");
}

// ---------------------------------------------------------------------------
// Routing
// ---------------------------------------------------------------------------

#[tokio::test]
async fn malformed_multipart_post_reports_malformed_post_request() {
    let service = service_with(TestS3, S3Config::default());
    let req = Request::builder()
        .method(Method::POST)
        .uri("/bucket")
        .header("content-type", "multipart/form-data; boundary=BOUNDARY")
        .body(Body::from("--BOUNDARY--\r\n".to_owned()))
        .expect("valid request");
    let (status, body) = send(&service, req).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body.contains("MalformedPOSTRequest"), "{body}");
}
// ---------------------------------------------------------------------------
// Deferred responses and host regions
// ---------------------------------------------------------------------------

#[tokio::test]
async fn complete_multipart_upload_future_error_is_surfaced_by_the_body() {
    let service = service_with(TestS3, S3Config::default());
    let req = Request::builder()
        .method(Method::POST)
        .uri("/bucket/key?uploadId=upload-id")
        .body(Body::from("<CompleteMultipartUpload></CompleteMultipartUpload>".to_owned()))
        .expect("valid request");

    let resp = service.call(req).await.expect("the operation response is returned");
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(!http_body::Body::is_end_stream(resp.body()));

    let err = resp.into_body().collect().await.expect_err("the deferred error is surfaced");
    assert!(err.to_string().contains("InvalidHeaderValue"), "{err}");
}

#[tokio::test]
async fn credential_region_differing_from_the_host_region_is_accepted() {
    let mut builder = S3ServiceBuilder::new(TestS3);
    builder.set_config(Arc::new(StaticConfigProvider::new(Arc::new(S3Config::default()))));
    builder.set_host(RegionHost);
    builder.set_auth(SimpleAuth::from_single(ACCESS_KEY, SECRET_KEY));
    let service = builder.build();

    let amz_date = jiff::Timestamp::now().strftime("%Y%m%dT%H%M%SZ").to_string();

    let region = "us-west-2";
    let date = &amz_date[..8];
    let payload = s3s_sigv4::EMPTY_STRING_SHA256_HASH;
    let parsed = s3s_sigv4::AmzDate::parse(&amz_date).expect("valid amz date");
    let signed_headers = [
        ("host", HOST),
        ("x-amz-content-sha256", payload),
        ("x-amz-date", amz_date.as_str()),
    ];
    let canonical_request = s3s_sigv4::create_canonical_request(
        "GET",
        "/bucket/key",
        &[] as &[(&str, &str)],
        signed_headers,
        s3s_sigv4::Payload::SingleChunk(payload),
    );
    let string_to_sign = s3s_sigv4::create_string_to_sign(&canonical_request, &parsed, region, "s3");
    let signature = s3s_sigv4::calculate_signature(&string_to_sign, SECRET_KEY, &parsed, region, "s3");

    let req = Request::builder()
        .method(Method::GET)
        .uri("/bucket/key")
        .header("host", HOST)
        .header("x-amz-content-sha256", payload)
        .header("x-amz-date", amz_date.as_str())
        .header(
            "authorization",
            format!(
                "AWS4-HMAC-SHA256 Credential={ACCESS_KEY}/{date}/{region}/s3/aws4_request, SignedHeaders=host;x-amz-content-sha256;x-amz-date, Signature={}",
                signature.as_str()
            ),
        )
        .body(Body::empty())
        .expect("valid request");

    let (status, body) = send(&service, req).await;
    assert_eq!(status, StatusCode::OK, "{body}");
}

const HOST: &str = "s3.example.com";
const ACCESS_KEY: &str = "test-access";
const SECRET_KEY: &str = "test-secret";
