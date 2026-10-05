// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

//! Anonymous `aws-chunked` request bodies.
//!
//! AWS decodes an unsigned `aws-chunked` body for anonymous requests
//! (`x-amz-content-sha256: STREAMING-UNSIGNED-PAYLOAD-TRAILER`) and stores the
//! decoded payload. The `SigV4` paths install the decoder while they verify the
//! chunk signatures; an unsigned declaration has no signing context, so it
//! needs its own entry point.

use super::common::*;
use crate::access::{S3Access, S3AccessContext};
use crate::auth::SimpleAuth;
use crate::config::S3ConfigProvider;
use crate::http::{Body, Request};
use crate::ops::{CallContext, Prepare};

use bytes::Bytes;
use futures::StreamExt as _;
use hyper::header::CONTENT_LENGTH;
use hyper::{Method, StatusCode, Uri, Version};
use std::sync::{Arc, Mutex};

const UNSIGNED_TRAILER: &str = "STREAMING-UNSIGNED-PAYLOAD-TRAILER";

/// Access control that accepts every request: these tests exercise the body
/// handling of an anonymous request that the deployment allows.
struct AllowAllAccess;

#[async_trait::async_trait]
impl S3Access for AllowAllAccess {
    async fn check(&self, _cx: &mut S3AccessContext<'_>) -> crate::error::S3Result<()> {
        Ok(())
    }
}

/// Records what the `S3` implementation actually reads from `PutObject`.
#[derive(Default)]
struct BodyRecordingS3 {
    received: Mutex<Option<Result<Bytes, String>>>,
}

#[async_trait::async_trait]
impl crate::s3_trait::S3 for BodyRecordingS3 {
    async fn put_object(
        &self,
        req: crate::S3Request<crate::dto::PutObjectInput>,
    ) -> crate::error::S3Result<crate::S3Response<crate::dto::PutObjectOutput>> {
        let body = req.input.body.expect("PutObject input should carry a body");
        let read = collect_stream(body).await.map_err(|error| describe(&error));
        *self.received.lock().expect("test mutex") = Some(read);
        Ok(crate::S3Response::new(crate::dto::PutObjectOutput::default()))
    }
}

/// Renders a body-stream error with the S3 code it maps to, so the tests can
/// assert the error the implementation would report to the client.
fn describe(error: &crate::error::StdError) -> String {
    match error.downcast_ref::<crate::stream::aws_chunked_stream::AwsChunkedStreamError>() {
        Some(chunked) => format!("{chunked} (s3 code {:?})", chunked.to_s3_error_code()),
        None => error.to_string(),
    }
}

async fn collect_stream<S>(mut stream: S) -> Result<Bytes, crate::error::StdError>
where
    S: futures::Stream<Item = Result<Bytes, crate::error::StdError>> + Unpin,
{
    let mut collected = Vec::new();
    while let Some(chunk) = stream.next().await {
        collected.extend_from_slice(&chunk?);
    }
    Ok(Bytes::from(collected))
}

/// Builds `aws-chunked` framing: one chunk per entry, terminated by the final
/// zero-length chunk (AWS accepts a body ending in `0\r\n` when no trailer is
/// declared).
fn frames(chunks: &[&[u8]]) -> Bytes {
    let mut out = Vec::new();
    for chunk in chunks {
        out.extend_from_slice(format!("{:x}\r\n", chunk.len()).as_bytes());
        out.extend_from_slice(chunk);
        out.extend_from_slice(b"\r\n");
    }
    out.extend_from_slice(b"0\r\n");
    Bytes::from(out)
}

fn request_uri() -> Uri {
    "http://localhost/test-bucket/test-key.txt".parse().expect("valid test URI")
}

fn anonymous_put(framed: Bytes, decoded_content_length: Option<usize>, extra_headers: &[(&str, &str)]) -> Request {
    let uri = request_uri();
    let mut builder = hyper::Request::builder()
        .method(Method::PUT)
        .version(Version::HTTP_11)
        .uri(uri.clone())
        .header(crate::header::HOST, uri.authority().expect("authority").as_str())
        .header(CONTENT_LENGTH, framed.len())
        .header("content-encoding", "aws-chunked")
        .header(crate::header::X_AMZ_CONTENT_SHA256, UNSIGNED_TRAILER);
    if let Some(decoded) = decoded_content_length {
        builder = builder.header(crate::header::X_AMZ_DECODED_CONTENT_LENGTH, decoded);
    }
    for &(name, value) in extra_headers {
        builder = builder.header(name, value);
    }
    Request::from(builder.body(Body::from(framed)).expect("valid request"))
}

fn anonymous_plain_put(body: Bytes) -> Request {
    let uri = request_uri();
    Request::from(
        hyper::Request::builder()
            .method(Method::PUT)
            .version(Version::HTTP_11)
            .uri(uri.clone())
            .header(crate::header::HOST, uri.authority().expect("authority").as_str())
            .header(CONTENT_LENGTH, body.len())
            .body(Body::from(body))
            .expect("valid request"),
    )
}

fn anonymous_ccx<'a>(
    s3: &'a Arc<dyn crate::s3_trait::S3>,
    config: &'a Arc<dyn S3ConfigProvider>,
    auth: &'a SimpleAuth,
    access: &'a AllowAllAccess,
) -> CallContext<'a> {
    CallContext {
        s3,
        config,
        host: None,
        auth: Some(auth as &dyn crate::auth::S3Auth),
        access: Some(access as &dyn S3Access),
        route: None,
        validation: None,
    }
}

/// The payload the implementation reads must be the decoded plaintext, not the
/// chunk framing: this is what AWS stores for an anonymous PUT.
#[tokio::test]
async fn anonymous_unsigned_trailer_put_reaches_the_implementation_decoded() {
    let payload = Bytes::from_static(b"anonymous streaming payload");
    let framed = frames(&[b"anonymous ", b"streaming ", b"payload"]);
    let service = Arc::new(BodyRecordingS3::default());
    let s3: Arc<dyn crate::s3_trait::S3> = service.clone();
    let config = test_config();
    let auth = SimpleAuth::from_single(ACCESS_KEY, SECRET_KEY);
    let access = AllowAllAccess;
    let ccx = anonymous_ccx(&s3, &config, &auth, &access);

    let mut req = anonymous_put(framed, Some(payload.len()), &[]);
    let response = super::call(&mut req, &ccx).await.expect("the request should be dispatched");
    assert!(response.status.is_success(), "unexpected status: {}", response.status);

    let received = service
        .received
        .lock()
        .expect("test mutex")
        .clone()
        .expect("put_object should run")
        .expect("the body should be readable");
    assert_eq!(received, payload, "an anonymous aws-chunked body must reach the implementation decoded");
}

/// A plain anonymous PUT stays untouched.
#[tokio::test]
async fn anonymous_plain_put_reaches_the_implementation_unchanged() {
    let payload = Bytes::from_static(b"plain payload");
    let service = Arc::new(BodyRecordingS3::default());
    let s3: Arc<dyn crate::s3_trait::S3> = service.clone();
    let config = test_config();
    let auth = SimpleAuth::from_single(ACCESS_KEY, SECRET_KEY);
    let access = AllowAllAccess;
    let ccx = anonymous_ccx(&s3, &config, &auth, &access);

    let mut req = anonymous_plain_put(payload.clone());
    let response = super::call(&mut req, &ccx).await.expect("the request should be dispatched");
    assert!(response.status.is_success(), "unexpected status: {}", response.status);

    let received = service
        .received
        .lock()
        .expect("test mutex")
        .clone()
        .expect("put_object should run")
        .expect("the body should be readable");
    assert_eq!(received, payload);
}

/// The signed `STREAMING-UNSIGNED-PAYLOAD-TRAILER` path is unchanged.
#[tokio::test]
async fn signed_unsigned_trailer_put_is_still_decoded() {
    const DECODED_LEN: &str = "24";
    let payload = Bytes::from_static(b"signed streaming payload");
    assert_eq!(payload.len().to_string(), DECODED_LEN);
    let framed =
        Bytes::from_static(b"7\r\nsigned \r\na\r\nstreaming \r\n7\r\npayload\r\n0\r\nx-amz-checksum-crc32:AAAAAA==\r\n\r\n");
    let service = Arc::new(BodyRecordingS3::default());
    let s3: Arc<dyn crate::s3_trait::S3> = service.clone();
    let config = test_config();
    let auth = SimpleAuth::from_single(ACCESS_KEY, SECRET_KEY);
    let access = AllowAllAccess;
    let ccx = anonymous_ccx(&s3, &config, &auth, &access);

    let mut req = signed_request(
        Method::PUT,
        Version::HTTP_11,
        "http://localhost/test-bucket/test-key.txt",
        UNSIGNED_TRAILER,
        &[
            ("content-encoding", "aws-chunked"),
            ("x-amz-decoded-content-length", DECODED_LEN),
            ("x-amz-trailer", "x-amz-checksum-crc32"),
        ],
    );
    req.headers.insert(CONTENT_LENGTH, framed.len().into());
    req.body = Body::from(framed);
    let response = super::call(&mut req, &ccx).await.expect("the request should be dispatched");
    assert!(response.status.is_success(), "unexpected status: {}", response.status);

    let received = service
        .received
        .lock()
        .expect("test mutex")
        .clone()
        .expect("put_object should run")
        .expect("the body should be readable");
    assert_eq!(received, payload, "the signed streaming path must keep decoding");
}

/// A trailer the request declared is published to the implementation.
#[tokio::test]
async fn anonymous_declared_trailer_is_exposed_to_the_implementation() {
    const BODY: &[u8] = b"f\r\ntrailer payload\r\n0\r\nx-amz-checksum-crc32:AAAAAA==\r\n\r\n";
    let service = Arc::new(BodyRecordingS3::default());
    let s3: Arc<dyn crate::s3_trait::S3> = service.clone();
    let config = test_config();
    let auth = SimpleAuth::from_single(ACCESS_KEY, SECRET_KEY);
    let access = AllowAllAccess;
    let ccx = anonymous_ccx(&s3, &config, &auth, &access);

    let mut req = anonymous_put(Bytes::from_static(BODY), Some(15), &[("x-amz-trailer", "x-amz-checksum-crc32")]);
    let Prepare::S3(op) = super::prepare(&mut req, &ccx).await.expect("prepare should succeed") else {
        panic!("an anonymous PUT should resolve to an S3 operation");
    };
    assert_eq!(op.name(), "PutObject");

    let decoded = req.body.store_all_limited(1024).await.expect("the body should be readable");
    assert_eq!(decoded, Bytes::from_static(b"trailer payload"));

    let trailers = req
        .s3ext
        .trailing_headers
        .as_ref()
        .expect("trailing headers should be wired")
        .take()
        .expect("the declared trailer should be published");
    assert_eq!(
        trailers
            .get("x-amz-checksum-crc32")
            .expect("trailer")
            .to_str()
            .expect("ASCII"),
        "AAAAAA=="
    );
}

/// A request that declared a trailer but ended without one fails the body
/// (AWS answers 400 `MalformedTrailerError`; s3s surfaces `IncompleteBody`).
#[tokio::test]
async fn anonymous_declared_trailer_without_a_trailer_block_fails() {
    let framed = frames(&[b"payload"]);
    let service = Arc::new(BodyRecordingS3::default());
    let s3: Arc<dyn crate::s3_trait::S3> = service.clone();
    let config = test_config();
    let auth = SimpleAuth::from_single(ACCESS_KEY, SECRET_KEY);
    let access = AllowAllAccess;
    let ccx = anonymous_ccx(&s3, &config, &auth, &access);

    let mut req = anonymous_put(framed, Some(7), &[("x-amz-trailer", "x-amz-checksum-crc32")]);
    let response = super::call(&mut req, &ccx).await.expect("the request should be dispatched");
    assert!(response.status.is_success(), "unexpected status: {}", response.status);

    let error = service
        .received
        .lock()
        .expect("test mutex")
        .clone()
        .expect("put_object should run")
        .expect_err("a declared but missing trailer block must fail the body");
    assert!(
        error.contains("IncompleteBody"),
        "a failing chunked body must map to IncompleteBody: {error}"
    );
}

/// `x-amz-decoded-content-length` is required for a chunked body, as on the
/// signed path (AWS documents the header as required for chunked content
/// encoding).
#[tokio::test]
async fn anonymous_unsigned_trailer_requires_decoded_content_length() {
    let framed = frames(&[b"payload"]);
    let service = Arc::new(BodyRecordingS3::default());
    let s3: Arc<dyn crate::s3_trait::S3> = service.clone();
    let config = test_config();
    let auth = SimpleAuth::from_single(ACCESS_KEY, SECRET_KEY);
    let access = AllowAllAccess;
    let ccx = anonymous_ccx(&s3, &config, &auth, &access);

    let mut req = anonymous_put(framed, None, &[]);
    let response = super::call(&mut req, &ccx).await.expect("the error should serialize");
    assert_eq!(response.status, StatusCode::LENGTH_REQUIRED);
    let body = response.body.bytes().expect("the error response should be buffered");
    let body = std::str::from_utf8(&body).expect("the error body is UTF-8");
    assert!(body.contains("<Code>MissingContentLength</Code>"), "unexpected error: {body}");
}

/// A payload-hash declaration that cannot be parsed is answered with 400 `InvalidRequest`
/// for an anonymous request, which is the code family the service answers with for the
/// shape; the `SigV4` paths answer `SignatureDoesNotMatch` because there the declaration is
/// part of the signed request.
#[tokio::test]
async fn anonymous_request_with_an_unparsable_payload_hash_is_an_invalid_request() {
    let service = Arc::new(BodyRecordingS3::default());
    let s3: Arc<dyn crate::s3_trait::S3> = service.clone();
    let config = test_config();
    let auth = SimpleAuth::from_single(ACCESS_KEY, SECRET_KEY);
    let access = AllowAllAccess;
    let ccx = anonymous_ccx(&s3, &config, &auth, &access);

    let mut req = anonymous_plain_put(Bytes::from_static(b"payload"));
    req.headers.insert(
        crate::header::X_AMZ_CONTENT_SHA256,
        "zzz-not-a-sha256".parse().expect("valid header value"),
    );
    let response = super::call(&mut req, &ccx).await.expect("the error should serialize");
    assert_eq!(response.status, StatusCode::BAD_REQUEST);
    let body = response.body.bytes().expect("the error response should be buffered");
    let body = std::str::from_utf8(&body).expect("the error body is UTF-8");
    assert!(body.contains("<Code>InvalidRequest</Code>"), "unexpected error: {body}");
}
