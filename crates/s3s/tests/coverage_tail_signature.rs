// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

//! Coverage tests for the request signature verification branches: header
//! authentication, presigned URLs and POST form signatures for both signature
//! versions.

#![allow(clippy::too_many_lines)]

use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use bytes::Bytes;
use futures::Stream;
use http::{Method, Request, StatusCode};
use http_body_util::BodyExt;
use s3s::auth::SimpleAuth;
use s3s::config::{S3Config, StaticConfigProvider};
use s3s::dto::{GetObjectInput, GetObjectOutput};
use s3s::service::{S3Service, S3ServiceBuilder};
use s3s::{Body, HttpRequest, S3, S3Request, S3Response, S3Result};
use s3s_sigv4::{AmzDate, Payload};

const ACCESS_KEY: &str = "test-access";
const SECRET_KEY: &str = "test-secret";
const REGION: &str = "us-east-1";
const SERVICE: &str = "s3";
const HOST: &str = "s3.example.com";
const EMPTY_SHA256: &str = s3s_sigv4::EMPTY_STRING_SHA256_HASH;
const STREAMING_PAYLOAD: &str = "STREAMING-AWS4-HMAC-SHA256-PAYLOAD";
const ZERO_SIGNATURE: &str = "0000000000000000000000000000000000000000000000000000000000000000";
const PAYLOAD_SHA256_X: &str = "2d711642b726b04401627ca9fbac32f5c8530fb1903cc4db02258717921a4881";
const AUTHORIZATION: &str = "authorization";

#[derive(Clone)]
struct TestS3;

#[async_trait::async_trait]
impl S3 for TestS3 {
    async fn get_object(&self, _req: S3Request<GetObjectInput>) -> S3Result<S3Response<GetObjectOutput>> {
        Ok(S3Response::new(GetObjectOutput::default()))
    }
}

fn service(config: S3Config, auth: bool) -> S3Service {
    let mut builder = S3ServiceBuilder::new(TestS3);
    builder.set_config(Arc::new(StaticConfigProvider::new(Arc::new(config))));
    if auth {
        builder.set_auth(SimpleAuth::from_single(ACCESS_KEY, SECRET_KEY));
    }
    builder.build()
}

fn sig_v2_config() -> S3Config {
    let mut config = S3Config::default();
    config.enable_sig_v2 = true;
    config
}

async fn send(service: &S3Service, req: HttpRequest) -> (StatusCode, String) {
    let resp = service.call(req).await.expect("service call returns a response");
    let status = resp.status();
    let bytes = resp.into_body().collect().await.expect("body collects").to_bytes();
    (status, String::from_utf8_lossy(&bytes).into_owned())
}

fn amz_date_now() -> AmzDate {
    let now = time::OffsetDateTime::now_utc();
    let raw = format!(
        "{:04}{:02}{:02}T{:02}{:02}{:02}Z",
        now.year(),
        u8::from(now.month()),
        now.day(),
        now.hour(),
        now.minute(),
        now.second()
    );
    AmzDate::parse(&raw).expect("current time is a valid amz date")
}

fn header_auth_value(method: &str, uri_path: &str, amz_date: &AmzDate, payload: Payload<'_>, payload_header: &str) -> String {
    let amz_date_str = amz_date.fmt_iso8601();
    let signed_headers = [
        ("host", HOST),
        ("x-amz-content-sha256", payload_header),
        ("x-amz-date", amz_date_str.as_str()),
    ];
    let canonical_request =
        s3s_sigv4::create_canonical_request(method, uri_path, &[] as &[(&str, &str)], signed_headers, payload);
    let string_to_sign = s3s_sigv4::create_string_to_sign(&canonical_request, amz_date, REGION, SERVICE);
    let signature = s3s_sigv4::calculate_signature(&string_to_sign, SECRET_KEY, amz_date, REGION, SERVICE);
    format!(
        "AWS4-HMAC-SHA256 Credential={ACCESS_KEY}/{}/{REGION}/{SERVICE}/aws4_request, \
         SignedHeaders=host;x-amz-content-sha256;x-amz-date, Signature={}",
        amz_date.fmt_date(),
        signature.as_str()
    )
}

fn presigned_signature(method: &str, uri_path: &str, query: &[(String, String)], amz_date: &AmzDate, payload: &str) -> String {
    let headers = [("host", HOST)];
    let canonical_request = s3s_sigv4::create_presigned_canonical_request_with_payload(method, uri_path, query, headers, payload);
    let string_to_sign = s3s_sigv4::create_string_to_sign(&canonical_request, amz_date, REGION, SERVICE);
    s3s_sigv4::calculate_signature(&string_to_sign, SECRET_KEY, amz_date, REGION, SERVICE)
        .as_str()
        .to_owned()
}

/// A byte stream whose remaining length is unknown.
struct UnknownLengthStream;

impl Stream for UnknownLengthStream {
    type Item = Result<Bytes, s3s::StdError>;

    fn poll_next(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        Poll::Ready(None)
    }
}

impl s3s::stream::ByteStream for UnknownLengthStream {}

fn base64(input: &str) -> String {
    base64_simd::STANDARD.encode_to_string(input)
}

fn post_policy(amz_date: &str, credential: &str, algorithm: &str) -> String {
    base64(&format!(
        "{{\"expiration\":\"2099-01-01T00:00:00Z\",\"conditions\":[{{\"x-amz-date\":\"{amz_date}\"}},{{\"x-amz-credential\":\"{credential}\"}},{{\"x-amz-algorithm\":\"{algorithm}\"}},{{\"bucket\":\"bucket\"}}]}}"
    ))
}

fn multipart_body(boundary: &str, fields: &[(&str, String)], file: Option<&str>) -> String {
    use std::fmt::Write as _;

    let mut body = String::new();
    for (name, value) in fields {
        let _ = write!(body, "--{boundary}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n");
    }
    if let Some(content) = file {
        let _ = write!(
            body,
            "--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"f.txt\"\r\nContent-Type: text/plain\r\n\r\n{content}\r\n"
        );
    }
    let _ = write!(body, "--{boundary}--\r\n");
    body
}

fn post_request(boundary: &str, fields: &[(&str, String)], file: Option<&str>) -> HttpRequest {
    Request::builder()
        .method(Method::POST)
        .uri("/bucket")
        .header("content-type", format!("multipart/form-data; boundary={boundary}"))
        .body(Body::from(multipart_body(boundary, fields, file)))
        .expect("valid request")
}

// ---------------------------------------------------------------------------
// SigV2
// ---------------------------------------------------------------------------

#[tokio::test]
async fn sig_v2_header_auth_is_rejected_when_disabled() {
    let service = service(S3Config::default(), true);
    let req = Request::builder()
        .method(Method::GET)
        .uri("/bucket/key")
        .header(AUTHORIZATION, "AWS test-access:c2lnbmF0dXJl")
        .body(Body::empty())
        .expect("valid request");

    let (status, body) = send(&service, req).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert!(body.contains("<Code>AccessDenied</Code>"), "{body}");
    assert!(body.contains("Signature Version 2 is disabled by server configuration"), "{body}");
}

#[tokio::test]
async fn sig_v2_header_auth_without_a_date_is_rejected() {
    let service = service(sig_v2_config(), true);
    let req = Request::builder()
        .method(Method::GET)
        .uri("/bucket/key")
        .header(AUTHORIZATION, "AWS test-access:c2lnbmF0dXJl")
        .body(Body::empty())
        .expect("valid request");

    let (status, body) = send(&service, req).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body.contains("<Code>InvalidRequest</Code>"), "{body}");
    assert!(body.contains("missing date"), "{body}");
}

#[tokio::test]
async fn sig_v2_presigned_url_with_a_bad_signature_is_rejected() {
    let service = service(sig_v2_config(), true);
    let expires = time::OffsetDateTime::now_utc().unix_timestamp() + 600;
    let uri = format!("/bucket/key?AWSAccessKeyId={ACCESS_KEY}&Expires={expires}&Signature=1No4mq5ETf02z8aet9voy6gui6E%3D");
    let req = Request::builder()
        .method(Method::GET)
        .uri(uri)
        .body(Body::empty())
        .expect("valid request");

    let (status, body) = send(&service, req).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert!(body.contains("<Code>SignatureDoesNotMatch</Code>"), "{body}");
}

#[tokio::test]
async fn sig_v2_post_signature_reports_invalid_policy() {
    let service = service(sig_v2_config(), true);
    let fields = vec![
        ("policy", "not-base64-%%%".to_owned()),
        ("awsaccesskeyid", ACCESS_KEY.to_owned()),
        ("signature", base64("whatever")),
    ];
    let req = post_request("BOUNDARY", &fields, Some("content"));

    let (status, body) = send(&service, req).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body.contains("invalid field: policy"), "{body}");
}

#[tokio::test]
async fn sig_v2_post_signature_with_a_bad_signature_is_rejected() {
    let service = service(sig_v2_config(), true);
    let fields = vec![
        ("policy", base64("{\"expiration\":\"2099-01-01T00:00:00Z\",\"conditions\":[]}")),
        ("awsaccesskeyid", ACCESS_KEY.to_owned()),
        ("signature", "1No4mq5ETf02z8aet9voy6gui6E=".to_owned()),
    ];
    let req = post_request("BOUNDARY", &fields, Some("content"));

    let (status, body) = send(&service, req).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert!(body.contains("<Code>SignatureDoesNotMatch</Code>"), "{body}");
}

#[tokio::test]
async fn sig_v2_post_signature_is_accepted() {
    let service = service(sig_v2_config(), true);
    let policy = base64(
        "{\"expiration\":\"2099-01-01T00:00:00Z\",\"conditions\":[{\"bucket\":\"bucket\"},[\"starts-with\",\"$key\",\"\"]]}",
    );
    let signature = s3s_sigv2::calculate_signature(SECRET_KEY, &policy);
    let fields = vec![
        ("policy", policy),
        ("awsaccesskeyid", ACCESS_KEY.to_owned()),
        ("signature", signature),
        ("key", "uploaded.txt".to_owned()),
    ];
    let req = post_request("BOUNDARY", &fields, Some("content"));

    let (status, body) = send(&service, req).await;
    // The signature is accepted and the request runs into the (unimplemented)
    // object upload rather than a signature error.
    assert!(!body.contains("SignatureDoesNotMatch"), "{status} {body}");
    assert!(!body.contains("InvalidPolicyDocument"), "{status} {body}");
}

// ---------------------------------------------------------------------------
// SigV4 header authentication
// ---------------------------------------------------------------------------

#[tokio::test]
async fn sig_v4_header_auth_without_a_date_is_rejected() {
    let service = service(S3Config::default(), true);
    let req = Request::builder()
        .method(Method::GET)
        .uri("/bucket/key")
        .header("host", HOST)
        .header("x-amz-content-sha256", EMPTY_SHA256)
        .header(
            AUTHORIZATION,
            format!(
                "AWS4-HMAC-SHA256 Credential={ACCESS_KEY}/20260101/{REGION}/{SERVICE}/aws4_request, \
                 SignedHeaders=host;x-amz-content-sha256;x-amz-date, Signature={ZERO_SIGNATURE}"
            ),
        )
        .body(Body::empty())
        .expect("valid request");

    let (status, body) = send(&service, req).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body.contains("missing header: x-amz-date"), "{body}");
}

#[tokio::test]
async fn sig_v4_header_auth_with_an_invalid_date_is_rejected() {
    let service = service(S3Config::default(), true);
    let req = Request::builder()
        .method(Method::GET)
        .uri("/bucket/key")
        .header("host", HOST)
        .header("x-amz-content-sha256", EMPTY_SHA256)
        .header("x-amz-date", "not-a-date")
        .header(
            AUTHORIZATION,
            format!(
                "AWS4-HMAC-SHA256 Credential={ACCESS_KEY}/20260101/{REGION}/{SERVICE}/aws4_request, \
                 SignedHeaders=host;x-amz-content-sha256;x-amz-date, Signature={ZERO_SIGNATURE}"
            ),
        )
        .body(Body::empty())
        .expect("valid request");

    let (status, body) = send(&service, req).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body.contains("invalid header: x-amz-date"), "{body}");
}

#[tokio::test]
async fn sig_v4_header_auth_with_a_non_hex_signature_is_rejected() {
    let service = service(S3Config::default(), true);
    let amz_date = amz_date_now();
    let req = Request::builder()
        .method(Method::GET)
        .uri("/bucket/key")
        .header("host", HOST)
        .header("x-amz-content-sha256", EMPTY_SHA256)
        .header("x-amz-date", amz_date.fmt_iso8601().as_str())
        .header(
            AUTHORIZATION,
            format!(
                "AWS4-HMAC-SHA256 Credential={ACCESS_KEY}/{}/{REGION}/{SERVICE}/aws4_request, \
                 SignedHeaders=host;x-amz-content-sha256;x-amz-date, Signature=zzzz",
                amz_date.fmt_date()
            ),
        )
        .body(Body::empty())
        .expect("valid request");

    let (status, body) = send(&service, req).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert!(body.contains("<Code>SignatureDoesNotMatch</Code>"), "{body}");
}

#[tokio::test]
async fn sig_v4_streaming_header_auth_without_decoded_length_is_rejected() {
    let service = service(S3Config::default(), true);
    let amz_date = amz_date_now();
    let authorization = header_auth_value("PUT", "/bucket/key", &amz_date, Payload::MultipleChunks, STREAMING_PAYLOAD);
    let req = Request::builder()
        .method(Method::PUT)
        .uri("/bucket/key")
        .header("host", HOST)
        .header("content-length", "100")
        .header("x-amz-content-sha256", STREAMING_PAYLOAD)
        .header("x-amz-date", amz_date.fmt_iso8601().as_str())
        .header(AUTHORIZATION, authorization)
        .body(Body::empty())
        .expect("valid request");

    let (status, body) = send(&service, req).await;
    assert_eq!(status, StatusCode::LENGTH_REQUIRED);
    assert!(body.contains("missing header: x-amz-decoded-content-length"), "{body}");
}

// ---------------------------------------------------------------------------
// SigV4 presigned URLs
// ---------------------------------------------------------------------------

fn presigned_uri(amz_date: &AmzDate, algorithm: &str, signature: &str, expires: &str) -> String {
    let credential = format!("{ACCESS_KEY}/{}/{REGION}/{SERVICE}/aws4_request", amz_date.fmt_date());
    format!(
        "/bucket/key?X-Amz-Algorithm={algorithm}&X-Amz-Credential={}&X-Amz-Date={}&X-Amz-Expires={expires}&X-Amz-SignedHeaders=host&X-Amz-Signature={signature}",
        urlencoding::encode(&credential),
        amz_date.fmt_iso8601(),
    )
}

#[tokio::test]
async fn presigned_url_with_another_algorithm_is_not_implemented() {
    let service = service(S3Config::default(), true);
    let amz_date = amz_date_now();
    let req = Request::builder()
        .method(Method::GET)
        .uri(presigned_uri(&amz_date, "AWS4-HMAC-SHA1", ZERO_SIGNATURE, "900"))
        .header("host", HOST)
        .body(Body::empty())
        .expect("valid request");

    let (status, body) = send(&service, req).await;
    assert_eq!(status, StatusCode::NOT_IMPLEMENTED);
    assert!(body.contains("X-Amz-Algorithm other than AWS4-HMAC-SHA256"), "{body}");
}

#[tokio::test]
async fn presigned_url_with_a_mismatched_credential_date_is_rejected() {
    let service = service(S3Config::default(), true);
    let amz_date = amz_date_now();
    let yesterday = amz_date.to_time().expect("timestamp") - jiff::SignedDuration::from_hours(24);
    let yesterday = jiff::Timestamp::from_second(yesterday.as_second()).expect("timestamp");
    let credential = format!(
        "{ACCESS_KEY}/{}/{REGION}/{SERVICE}/aws4_request",
        s3s_sigv4::AmzDate::parse(&yesterday.strftime("%Y%m%dT%H%M%SZ").to_string())
            .expect("valid amz date")
            .fmt_date()
    );
    let uri = format!(
        "/bucket/key?X-Amz-Algorithm=AWS4-HMAC-SHA256&X-Amz-Credential={}&X-Amz-Date={}&X-Amz-Expires=900&X-Amz-SignedHeaders=host&X-Amz-Signature={ZERO_SIGNATURE}",
        urlencoding::encode(&credential),
        amz_date.fmt_iso8601(),
    );
    let req = Request::builder()
        .method(Method::GET)
        .uri(uri)
        .header("host", HOST)
        .body(Body::empty())
        .expect("valid request");

    let (status, body) = send(&service, req).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert!(body.contains("credential scope date does not match x-amz-date"), "{body}");
}

#[tokio::test]
async fn presigned_url_from_the_future_is_rejected() {
    let service = service(S3Config::default(), true);
    let future = AmzDate::parse("20990101T000000Z").expect("valid amz date");
    let req = Request::builder()
        .method(Method::GET)
        .uri(presigned_uri(&future, "AWS4-HMAC-SHA256", ZERO_SIGNATURE, "900"))
        .header("host", HOST)
        .body(Body::empty())
        .expect("valid request");

    let (status, body) = send(&service, req).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert!(body.contains("request date is later than server time too much"), "{body}");
}

#[tokio::test]
async fn presigned_url_without_content_length_is_rejected_for_a_streaming_body() {
    let service = service(S3Config::default(), true);
    let amz_date = amz_date_now();
    let credential = format!("{ACCESS_KEY}/{}/{REGION}/{SERVICE}/aws4_request", amz_date.fmt_date());
    let mut query: Vec<(String, String)> = vec![
        ("X-Amz-Algorithm".to_owned(), "AWS4-HMAC-SHA256".to_owned()),
        ("X-Amz-Credential".to_owned(), credential.clone()),
        ("X-Amz-Date".to_owned(), amz_date.fmt_iso8601().to_string()),
        ("X-Amz-Expires".to_owned(), "900".to_owned()),
        ("X-Amz-SignedHeaders".to_owned(), "host".to_owned()),
    ];
    let signature = presigned_signature("GET", "/bucket/key", &query, &amz_date, PAYLOAD_SHA256_X);
    query.push(("X-Amz-Signature".to_owned(), signature.clone()));

    let query_string = query
        .iter()
        .map(|(name, value)| format!("{}={}", urlencoding::encode(name), urlencoding::encode(value)))
        .collect::<Vec<_>>()
        .join("&");
    let stream: s3s::stream::DynByteStream = Box::pin(UnknownLengthStream);
    let req = Request::builder()
        .method(Method::GET)
        .uri(format!("/bucket/key?{query_string}"))
        .header("host", HOST)
        .header("x-amz-content-sha256", PAYLOAD_SHA256_X)
        .body(Body::from(stream))
        .expect("valid request");

    let (status, body) = send(&service, req).await;
    assert_eq!(status, StatusCode::LENGTH_REQUIRED);
    assert!(body.contains("missing header: content-length"), "{body}");
}

#[tokio::test]
async fn sig_v4_header_auth_with_a_missing_signed_header_is_rejected() {
    let service = service(S3Config::default(), true);
    let amz_date = amz_date_now();
    let req = Request::builder()
        .method(Method::GET)
        .uri("/bucket/key")
        .header("host", HOST)
        .header("x-amz-content-sha256", EMPTY_SHA256)
        .header("x-amz-date", amz_date.fmt_iso8601().as_str())
        .header(
            AUTHORIZATION,
            format!(
                "AWS4-HMAC-SHA256 Credential={ACCESS_KEY}/{}/{REGION}/{SERVICE}/aws4_request, \
                 SignedHeaders=host;x-amz-content-sha256;x-amz-date;x-amz-meta-extra, Signature={ZERO_SIGNATURE}",
                amz_date.fmt_date()
            ),
        )
        .body(Body::empty())
        .expect("valid request");

    let (status, body) = send(&service, req).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert!(body.contains("missing signed header: x-amz-meta-extra"), "{body}");
}

#[tokio::test]
async fn multipart_post_without_a_boundary_is_rejected() {
    let service = service(S3Config::default(), true);
    let req = Request::builder()
        .method(Method::POST)
        .uri("/bucket")
        .header("content-type", "multipart/form-data")
        .body(Body::from("--boundary--\r\n".to_owned()))
        .expect("valid request");

    let (status, body) = send(&service, req).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body.contains("missing boundary"), "{body}");
}

// ---------------------------------------------------------------------------
// SigV4 POST form signatures
// ---------------------------------------------------------------------------

fn post_signature(config: &S3Config, fields: &[(&str, String)], file: Option<&str>) -> (StatusCode, String) {
    let service = service(config.clone(), true);
    let req = post_request("BOUNDARY", fields, file);
    futures::executor::block_on(send(&service, req))
}

fn post_fields(
    policy: String,
    algorithm: &str,
    credential: &str,
    amz_date: &str,
    signature: &str,
) -> Vec<(&'static str, String)> {
    vec![
        ("policy", policy),
        ("x-amz-algorithm", algorithm.to_owned()),
        ("x-amz-credential", credential.to_owned()),
        ("x-amz-date", amz_date.to_owned()),
        ("x-amz-signature", signature.to_owned()),
    ]
}

#[test]
fn sig_v4_post_signature_reports_a_non_base64_policy() {
    let amz_date = amz_date_now();
    let amz_date_str = amz_date.fmt_iso8601().to_string();
    let credential = format!("{ACCESS_KEY}/{}/{REGION}/{SERVICE}/aws4_request", amz_date.fmt_date());
    let fields = post_fields(
        "not base64 %%%".to_owned(),
        "AWS4-HMAC-SHA256",
        &credential,
        &amz_date_str,
        ZERO_SIGNATURE,
    );

    let (status, body) = post_signature(&S3Config::default(), &fields, Some("content"));
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body.contains("invalid field: policy"), "{body}");
}

#[test]
fn sig_v4_post_signature_reports_another_algorithm() {
    let amz_date = amz_date_now();
    let amz_date_str = amz_date.fmt_iso8601().to_string();
    let credential = format!("{ACCESS_KEY}/{}/{REGION}/{SERVICE}/aws4_request", amz_date.fmt_date());
    let fields = post_fields(
        post_policy(&amz_date_str, &credential, "AWS4-HMAC-SHA256"),
        "AWS4-HMAC-SHA1",
        &credential,
        &amz_date_str,
        ZERO_SIGNATURE,
    );

    let (status, body) = post_signature(&S3Config::default(), &fields, Some("content"));
    assert_eq!(status, StatusCode::NOT_IMPLEMENTED);
    assert!(body.contains("x-amz-algorithm other than AWS4-HMAC-SHA256"), "{body}");
}

#[test]
fn sig_v4_post_signature_reports_a_policy_date_mismatch() {
    let amz_date = amz_date_now();
    let amz_date_str = amz_date.fmt_iso8601().to_string();
    let credential = format!("{ACCESS_KEY}/{}/{REGION}/{SERVICE}/aws4_request", amz_date.fmt_date());
    let fields = post_fields(
        post_policy("19990101T000000Z", &credential, "AWS4-HMAC-SHA256"),
        "AWS4-HMAC-SHA256",
        &credential,
        &amz_date_str,
        ZERO_SIGNATURE,
    );

    let (status, body) = post_signature(&S3Config::default(), &fields, Some("content"));
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body.contains("x-amz-date does not match policy"), "{body}");
}

#[test]
fn sig_v4_post_signature_reports_a_policy_credential_mismatch() {
    let amz_date = amz_date_now();
    let amz_date_str = amz_date.fmt_iso8601().to_string();
    let credential = format!("{ACCESS_KEY}/{}/{REGION}/{SERVICE}/aws4_request", amz_date.fmt_date());
    let fields = post_fields(
        post_policy(&amz_date_str, "other/20990101/us-east-1/s3/aws4_request", "AWS4-HMAC-SHA256"),
        "AWS4-HMAC-SHA256",
        &credential,
        &amz_date_str,
        ZERO_SIGNATURE,
    );

    let (status, body) = post_signature(&S3Config::default(), &fields, Some("content"));
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body.contains("x-amz-credential does not match policy"), "{body}");
}

#[test]
fn sig_v4_post_signature_reports_a_policy_algorithm_mismatch() {
    let amz_date = amz_date_now();
    let amz_date_str = amz_date.fmt_iso8601().to_string();
    let credential = format!("{ACCESS_KEY}/{}/{REGION}/{SERVICE}/aws4_request", amz_date.fmt_date());
    let fields = post_fields(
        post_policy(&amz_date_str, &credential, "AWS4-HMAC-SHA1"),
        "AWS4-HMAC-SHA256",
        &credential,
        &amz_date_str,
        ZERO_SIGNATURE,
    );

    let (status, body) = post_signature(&S3Config::default(), &fields, Some("content"));
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body.contains("x-amz-algorithm does not match policy"), "{body}");
}

#[test]
fn sig_v4_post_signature_reports_a_credential_scope_date_mismatch() {
    let amz_date = amz_date_now();
    let amz_date_str = amz_date.fmt_iso8601().to_string();
    let credential = format!("{ACCESS_KEY}/19990101/{REGION}/{SERVICE}/aws4_request");
    let fields = post_fields(
        post_policy(&amz_date_str, &credential, "AWS4-HMAC-SHA256"),
        "AWS4-HMAC-SHA256",
        &credential,
        &amz_date_str,
        ZERO_SIGNATURE,
    );

    let (status, body) = post_signature(&S3Config::default(), &fields, Some("content"));
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert!(body.contains("credential scope date does not match x-amz-date"), "{body}");
}

#[test]
fn sig_v4_post_signature_mismatch_is_rejected() {
    let amz_date = amz_date_now();
    let amz_date_str = amz_date.fmt_iso8601().to_string();
    let credential = format!("{ACCESS_KEY}/{}/{REGION}/{SERVICE}/aws4_request", amz_date.fmt_date());
    let fields = post_fields(
        post_policy(&amz_date_str, &credential, "AWS4-HMAC-SHA256"),
        "AWS4-HMAC-SHA256",
        &credential,
        &amz_date_str,
        ZERO_SIGNATURE,
    );

    let (status, body) = post_signature(&S3Config::default(), &fields, Some("content"));
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert!(body.contains("<Code>SignatureDoesNotMatch</Code>"), "{body}");
}
