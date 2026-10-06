// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

//! End-to-end `Range` handling through the request pipeline.
//!
//! Expectations follow the behaviour of Amazon S3 (measured against the
//! service) together with RFC 9110: a `Range` field that cannot be served as
//! exactly one byte range is ignored and the whole object is answered with
//! `200`; a satisfiable range is answered with `206`; an unsatisfiable range
//! is answered with `416`. Details are settled in the fix plan of the
//! investigation: case-insensitive units and empty list elements follow the
//! RFC, duplicate field lines follow S3 (the first line wins), and
//! `partNumber` combined with `Range` is rejected like S3 does.

#![allow(clippy::too_many_lines)]

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use bytes::Bytes;
use http::{HeaderMap, HeaderValue, Method, Request, StatusCode};
use http_body_util::BodyExt;
use s3s::config::{S3Config, StaticConfigProvider};
use s3s::dto::{
    GetObjectInput, GetObjectOutput, HeadObjectInput, HeadObjectOutput, PutObjectInput, PutObjectOutput, Range, StreamingBlob,
};
use s3s::service::{S3Service, S3ServiceBuilder};
use s3s::{Body, S3, S3Error, S3Request, S3Response, S3Result};

const FULL_LENGTH: u64 = 10;
const FULL_BODY: &[u8] = b"0123456789";

#[derive(Clone)]
struct TestS3 {
    seen: Arc<Mutex<Vec<Option<Range>>>>,
    put_ranges: Arc<Mutex<Vec<String>>>,
}

impl TestS3 {
    fn new() -> Self {
        Self {
            seen: Arc::new(Mutex::new(Vec::new())),
            put_ranges: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn seen(&self) -> Vec<Option<Range>> {
        self.seen.lock().expect("test mutex").clone()
    }
}

#[async_trait]
impl S3 for TestS3 {
    async fn get_object(&self, req: S3Request<GetObjectInput>) -> S3Result<S3Response<GetObjectOutput>> {
        let range = req.input.range;
        self.seen.lock().expect("test mutex").push(range);

        match range {
            Some(range) => {
                let hit = range.check(FULL_LENGTH).map_err(S3Error::from)?;
                let len = hit.end - hit.start;
                let (start, end) = (
                    usize::try_from(hit.start).expect("range start"),
                    usize::try_from(hit.end).expect("range end"),
                );
                let body = FULL_BODY.get(start..end).expect("test body range");
                let content_length = i64::try_from(len).expect("test body length");
                Ok(S3Response::new(GetObjectOutput {
                    content_length: Some(content_length),
                    content_range: Some(format!("bytes {}-{}/{FULL_LENGTH}", hit.start, hit.end - 1)),
                    body: Some(StreamingBlob::from_bytes(Bytes::copy_from_slice(body))),
                    ..Default::default()
                }))
            }
            None => Ok(S3Response::new(GetObjectOutput {
                content_length: Some(i64::try_from(FULL_LENGTH).expect("test body length")),
                body: Some(StreamingBlob::from_bytes(Bytes::from_static(FULL_BODY))),
                ..Default::default()
            })),
        }
    }

    async fn head_object(&self, req: S3Request<HeadObjectInput>) -> S3Result<S3Response<HeadObjectOutput>> {
        self.seen.lock().expect("test mutex").push(req.input.range);
        Ok(S3Response::new(HeadObjectOutput {
            content_length: Some(i64::try_from(FULL_LENGTH).expect("test body length")),
            ..Default::default()
        }))
    }

    async fn put_object(&self, req: S3Request<PutObjectInput>) -> S3Result<S3Response<PutObjectOutput>> {
        let header = req
            .headers
            .get("range")
            .map_or_else(|| "<absent>".to_owned(), |value| format!("{value:?}"));
        self.put_ranges.lock().expect("test mutex").push(header);
        Ok(S3Response::new(PutObjectOutput::default()))
    }
}

fn service(s3: TestS3) -> S3Service {
    let mut builder = S3ServiceBuilder::new(s3);
    builder.set_config(Arc::new(StaticConfigProvider::new(Arc::new(S3Config::default()))));
    builder.build()
}

async fn send(service: &S3Service, req: http::Request<Body>) -> (StatusCode, HeaderMap, String) {
    let resp = service.call(req).await.expect("service call returns a response");
    let status = resp.status();
    let headers = resp.headers().clone();
    let bytes = resp.into_body().collect().await.expect("body collects").to_bytes();
    (status, headers, String::from_utf8_lossy(&bytes).into_owned())
}

fn error_code(body: &str) -> Option<&str> {
    let start = body.find("<Code>")? + "<Code>".len();
    let rest = body.get(start..)?;
    let end = rest.find("</Code>")?;
    rest.get(..end)
}

fn get(uri: &str) -> http::request::Builder {
    Request::builder().method(Method::GET).uri(uri)
}

#[derive(Debug)]
struct Case {
    label: &'static str,
    range: Option<&'static str>,
    status: StatusCode,
    error: Option<&'static str>,
    content_range: Option<&'static str>,
    seen: Option<Range>,
}

const fn case(
    label: &'static str,
    range: Option<&'static str>,
    status: StatusCode,
    error: Option<&'static str>,
    content_range: Option<&'static str>,
    seen: Option<Range>,
) -> Case {
    Case {
        label,
        range,
        status,
        error,
        content_range,
        seen,
    }
}

#[tokio::test]
async fn range_field_through_the_pipeline() {
    let cases = vec![
        // A missing or empty field is not a range.
        case("absent", None, StatusCode::OK, None, None, None),
        case("empty value", Some(""), StatusCode::OK, None, None, None),
        // A satisfiable single range is served as 206.
        case(
            "single range",
            Some("bytes=0-3"),
            StatusCode::PARTIAL_CONTENT,
            None,
            Some("bytes 0-3/10"),
            Some(Range::Int { first: 0, last: Some(3) }),
        ),
        case(
            "open-ended",
            Some("bytes=0-"),
            StatusCode::PARTIAL_CONTENT,
            None,
            Some("bytes 0-9/10"),
            Some(Range::Int { first: 0, last: None }),
        ),
        case(
            "suffix",
            Some("bytes=-5"),
            StatusCode::PARTIAL_CONTENT,
            None,
            Some("bytes 5-9/10"),
            Some(Range::Suffix { length: 5 }),
        ),
        case(
            "leading zeros",
            Some("bytes=00-03"),
            StatusCode::PARTIAL_CONTENT,
            None,
            Some("bytes 0-3/10"),
            Some(Range::Int { first: 0, last: Some(3) }),
        ),
        case(
            "last past end",
            Some("bytes=5-100"),
            StatusCode::PARTIAL_CONTENT,
            None,
            Some("bytes 5-9/10"),
            Some(Range::Int {
                first: 5,
                last: Some(100),
            }),
        ),
        // A valid but unsatisfiable range is still 416.
        case(
            "zero suffix",
            Some("bytes=-0"),
            StatusCode::RANGE_NOT_SATISFIABLE,
            Some("InvalidRange"),
            None,
            Some(Range::Suffix { length: 0 }),
        ),
        case(
            "start past end",
            Some("bytes=100-"),
            StatusCode::RANGE_NOT_SATISFIABLE,
            Some("InvalidRange"),
            None,
            Some(Range::Int { first: 100, last: None }),
        ),
        // Everything S3 ignores is ignored: the whole object with 200.
        case("multiple ranges", Some("bytes=0-1,3-4"), StatusCode::OK, None, None, None),
        case("multiple ranges spaced", Some("bytes=0-1, 3-4"), StatusCode::OK, None, None, None),
        case("repeated unit", Some("bytes=0-3,bytes=5-6"), StatusCode::OK, None, None, None),
        case("reversed", Some("bytes=5-3"), StatusCode::OK, None, None, None),
        case("garbage", Some("bytes=abc"), StatusCode::OK, None, None, None),
        case("empty suffix", Some("bytes=-"), StatusCode::OK, None, None, None),
        case("plus sign", Some("bytes=+0-3"), StatusCode::OK, None, None, None),
        case("hex", Some("bytes=0x1-3"), StatusCode::OK, None, None, None),
        case("semicolon", Some("bytes=0-3;"), StatusCode::OK, None, None, None),
        case("slash", Some("bytes=0-3/2"), StatusCode::OK, None, None, None),
        case("space before equals", Some("bytes =0-3"), StatusCode::OK, None, None, None),
        case("space after equals", Some("bytes= 0-3"), StatusCode::OK, None, None, None),
        case("tab inside", Some("bytes=0-\t3"), StatusCode::OK, None, None, None),
        case("unknown unit", Some("chars=0-1"), StatusCode::OK, None, None, None),
        case("unknown unit other-range", Some("items=0-3"), StatusCode::OK, None, None, None),
        case("empty elements only", Some("bytes=,"), StatusCode::OK, None, None, None),
        case("trailing space", Some("bytes=0-3 "), StatusCode::OK, None, None, None),
        // RFC 9110 fixes where the RFC and S3 differ (decided in favour of the RFC).
        case(
            "unit case",
            Some("BYTES=0-3"),
            StatusCode::PARTIAL_CONTENT,
            None,
            Some("bytes 0-3/10"),
            Some(Range::Int { first: 0, last: Some(3) }),
        ),
        case(
            "empty element leading",
            Some("bytes=,0-3"),
            StatusCode::PARTIAL_CONTENT,
            None,
            Some("bytes 0-3/10"),
            Some(Range::Int { first: 0, last: Some(3) }),
        ),
        case(
            "empty element trailing",
            Some("bytes=0-3,"),
            StatusCode::PARTIAL_CONTENT,
            None,
            Some("bytes 0-3/10"),
            Some(Range::Int { first: 0, last: Some(3) }),
        ),
        case("empty element middle", Some("bytes=0-1,,3-4"), StatusCode::OK, None, None, None),
    ];

    for case in &cases {
        let s3 = TestS3::new();
        let service = service(s3.clone());

        let mut builder = get("/bucket/key");
        if let Some(range) = case.range {
            builder = builder.header("range", range);
        }

        let req = builder.body(Body::empty()).expect("valid request");
        let (status, headers, body) = send(&service, req).await;

        assert_eq!(status, case.status, "{}: {body}", case.label);
        assert_eq!(error_code(&body), case.error, "{}", case.label);
        assert_eq!(
            headers.get("content-range").and_then(|value| value.to_str().ok()),
            case.content_range,
            "{}",
            case.label
        );
        assert_eq!(s3.seen(), vec![case.seen], "{}", case.label);
    }
}

#[tokio::test]
async fn duplicate_range_field_lines_use_the_first_line() {
    let s3 = TestS3::new();
    let service = service(s3.clone());

    let req = get("/bucket/key")
        .header("range", "bytes=0-3")
        .header("range", "bytes=5-6")
        .body(Body::empty())
        .expect("valid request");
    let (status, headers, _body) = send(&service, req).await;

    assert_eq!(status, StatusCode::PARTIAL_CONTENT);
    assert_eq!(headers.get("content-range").and_then(|value| value.to_str().ok()), Some("bytes 0-3/10"));
    assert_eq!(s3.seen(), vec![Some(Range::Int { first: 0, last: Some(3) })]);
}

#[tokio::test]
async fn a_non_utf8_range_value_is_ignored() {
    let s3 = TestS3::new();
    let service = service(s3.clone());

    let raw = HeaderValue::from_bytes(b"bytes=\xff0-3").expect("opaque header value");
    let req = get("/bucket/key")
        .header("range", raw)
        .body(Body::empty())
        .expect("valid request");
    let (status, _headers, body) = send(&service, req).await;

    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(s3.seen(), vec![None]);
}

#[tokio::test]
async fn range_on_a_method_without_range_handling_is_ignored() {
    let s3 = TestS3::new();
    let service = service(s3.clone());

    let req = Request::builder()
        .method(Method::PUT)
        .uri("/bucket/key")
        .header("content-length", "7")
        .header("range", "bytes=abc")
        .body(Body::from(Bytes::from_static(b"payload")))
        .expect("valid request");
    let (status, _headers, body) = send(&service, req).await;

    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(s3.put_ranges.lock().expect("test mutex").clone(), vec!["\"bytes=abc\"".to_owned()]);
}

#[tokio::test]
async fn head_object_shares_the_range_handling() {
    let s3 = TestS3::new();
    let service = service(s3.clone());

    let req = Request::builder()
        .method(Method::HEAD)
        .uri("/bucket/key")
        .header("range", "bytes=abc")
        .body(Body::empty())
        .expect("valid request");
    let (status, _headers, _body) = send(&service, req).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(s3.seen(), vec![None]);
}

#[tokio::test]
async fn part_number_combined_with_range_is_rejected() {
    let s3 = TestS3::new();
    let service = service(s3.clone());

    let req = get("/bucket/key?partNumber=1")
        .header("range", "bytes=0-3")
        .body(Body::empty())
        .expect("valid request");
    let (status, _headers, body) = send(&service, req).await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(error_code(&body), Some("InvalidRequest"));
    assert_eq!(s3.seen(), Vec::<Option<Range>>::new());
}

#[tokio::test]
async fn head_object_rejects_part_number_combined_with_range() {
    let s3 = TestS3::new();
    let service = service(s3.clone());

    let req = Request::builder()
        .method(Method::HEAD)
        .uri("/bucket/key?partNumber=1")
        .header("range", "bytes=0-3")
        .body(Body::empty())
        .expect("valid request");
    let (status, _headers, _body) = send(&service, req).await;

    // A HEAD response carries no body, so only the status is observable.
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(s3.seen(), Vec::<Option<Range>>::new());
}

#[tokio::test]
async fn part_number_alone_is_passed_to_the_implementation() {
    let s3 = TestS3::new();
    let service = service(s3.clone());

    let req = get("/bucket/key?partNumber=1").body(Body::empty()).expect("valid request");
    let (status, _headers, _body) = send(&service, req).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(s3.seen(), vec![None]);
}

#[tokio::test]
async fn overflowing_numerals_follow_the_rfc() {
    let huge = "9".repeat(30);

    let cases = [
        // An overflowing `last-pos` means "to the end".
        (
            format!("bytes=0-{huge}"),
            StatusCode::PARTIAL_CONTENT,
            None,
            Some("bytes 0-9/10"),
            Some(Range::Int { first: 0, last: None }),
        ),
        // An overflowing `first-pos` cannot be satisfied by any representation.
        (
            format!("bytes={huge}-"),
            StatusCode::RANGE_NOT_SATISFIABLE,
            Some("InvalidRange"),
            None,
            Some(Range::Int {
                first: u64::MAX,
                last: None,
            }),
        ),
        // An overflowing `suffix-length` covers the whole representation.
        (
            format!("bytes=-{huge}"),
            StatusCode::PARTIAL_CONTENT,
            None,
            Some("bytes 0-9/10"),
            Some(Range::Suffix { length: u64::MAX }),
        ),
    ];

    for (range, status, error, content_range, seen) in &cases {
        let s3 = TestS3::new();
        let service = service(s3.clone());

        let req = get("/bucket/key")
            .header("range", range.as_str())
            .body(Body::empty())
            .expect("valid request");
        let (actual_status, headers, body) = send(&service, req).await;

        assert_eq!(actual_status, *status, "{range}: {body}");
        assert_eq!(error_code(&body), *error, "{range}");
        assert_eq!(
            headers.get("content-range").and_then(|value| value.to_str().ok()),
            *content_range,
            "{range}"
        );
        assert_eq!(s3.seen(), vec![*seen], "{range}");
    }
}
