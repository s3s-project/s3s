// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

//! Custom route path-validation bypass tests.
//!
//! Contract under test: `is_match` runs after structural parsing but before
//! S3 naming semantics are enforced. A matching route receives requests whose
//! paths would be rejected as invalid buckets/keys; unmatched (and no-route)
//! requests keep the legacy pipeline byte-for-byte, including error ordering.

use super::*;
use crate::auth::{SecretKey, SimpleAuth};
use crate::config::{S3ConfigProvider, StaticConfigProvider};
use crate::error::S3ErrorCode;
use crate::host::SingleDomain;
use crate::protocol::S3Response;
use crate::route::S3Route;
use crate::s3_trait::S3;
use crate::sig_v4;
use hyper::http::Extensions;
use hyper::{HeaderMap, Method, Uri};
use s3s_sigv4::AmzDate;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

struct NoopS3;

#[async_trait::async_trait]
impl S3 for NoopS3 {}

/// Route matching every request; counts `call` invocations.
struct MatchAllRoute {
    calls: Arc<AtomicUsize>,
}

impl MatchAllRoute {
    fn new() -> Self {
        Self {
            calls: Arc::new(AtomicUsize::new(0)),
        }
    }

    fn call_count(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

#[async_trait::async_trait]
impl S3Route for MatchAllRoute {
    fn is_match(&self, _method: &Method, _uri: &Uri, _headers: &HeaderMap, _ext: &mut Extensions) -> bool {
        true
    }

    async fn check_access(&self, _req: &mut S3Request<Body>) -> crate::error::S3Result<()> {
        Ok(()) // allow anonymous so the end-to-end test observes `call`
    }

    async fn call(&self, _req: S3Request<Body>) -> crate::error::S3Result<S3Response<Body>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(S3Response::new(Body::from("routed".to_string())))
    }
}

/// Route that never matches.
struct MatchNoneRoute;

#[async_trait::async_trait]
impl S3Route for MatchNoneRoute {
    fn is_match(&self, _: &Method, _: &Uri, _: &HeaderMap, _: &mut Extensions) -> bool {
        false
    }

    async fn call(&self, _: S3Request<Body>) -> crate::error::S3Result<S3Response<Body>> {
        unreachable!("MatchNoneRoute never matches")
    }
}

struct CtxParts {
    s3: Arc<dyn S3>,
    config: Arc<dyn S3ConfigProvider>,
    auth: Option<SimpleAuth>,
    host: Option<SingleDomain>,
}

fn ccx<'a>(parts: &'a CtxParts, auth: bool, host: bool, route: Option<&'a dyn S3Route>) -> CallContext<'a> {
    CallContext {
        s3: &parts.s3,
        config: &parts.config,
        host: if host {
            parts.host.as_ref().map(|h| h as &dyn crate::host::S3Host)
        } else {
            None
        },
        auth: if auth {
            parts.auth.as_ref().map(|a| a as &dyn crate::auth::S3Auth)
        } else {
            None
        },
        access: None,
        route,
        validation: None,
    }
}

fn ctx() -> CtxParts {
    CtxParts {
        s3: Arc::new(NoopS3),
        config: Arc::new(StaticConfigProvider::default()),
        auth: Some(SimpleAuth::from_single(
            "AKIAIOSFODNN7EXAMPLE",
            SecretKey::from("wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY"),
        )),
        host: Some(SingleDomain::new("example.com").expect("valid domain")),
    }
}

fn get_request(path: &str, host: &str) -> Request {
    Request::from(
        hyper::Request::builder()
            .method(Method::GET)
            .uri(format!("http://localhost{path}"))
            .header(crate::header::HOST, host)
            .body(Body::empty())
            .unwrap(),
    )
}

#[tokio::test]
async fn invalid_paths_reach_matched_custom_route() {
    let parts = ctx();
    let route = MatchAllRoute::new();
    let ccx = ccx(&parts, false, false, Some(&route));

    // bucket names violating AWS rules; keys over 1024 bytes
    for path in ["/x/y", "/Foo/bar", "/api_status/x", "/b/-leading-dash"] {
        let mut req = get_request(path, "localhost");
        let ans = prepare(&mut req, &ccx)
            .await
            .unwrap_or_else(|e| panic!("{path}: unexpected {e:?}"));
        assert!(matches!(ans, Prepare::CustomRoute), "path {path}");
    }

    let long_key = "a".repeat(2000);
    let mut req = get_request(&format!("/bucket/{long_key}"), "localhost");
    let ans = prepare(&mut req, &ccx)
        .await
        .unwrap_or_else(|e| panic!("long key: unexpected {e:?}"));
    assert!(matches!(ans, Prepare::CustomRoute));
}

#[tokio::test]
async fn invalid_path_reaches_route_handler_end_to_end() {
    use crate::ops::call;
    use hyper::StatusCode;

    let parts = ctx();
    let route = MatchAllRoute::new();
    let ccx = ccx(&parts, false, false, Some(&route));

    let mut req = get_request("/Foo/bar", "localhost");
    let resp = call(&mut req, &ccx).await.expect("route handles illegal path");
    assert_eq!(resp.status, StatusCode::OK);
    assert_eq!(route.call_count(), 1);
}

#[tokio::test]
async fn unmatched_route_keeps_original_path_errors() {
    let parts = ctx();
    let ccx = ccx(&parts, false, false, Some(&MatchNoneRoute));

    for path in ["/x/y", "/Foo/bar", "/api_status/x", "/b/-leading-dash"] {
        let mut req = get_request(path, "localhost");
        let Err(err) = prepare(&mut req, &ccx).await else { panic!("{path}: expected path rejection") };
        assert_eq!(*err.code(), S3ErrorCode::InvalidBucketName, "path {path}");
    }

    let long_key = "a".repeat(2000);
    let mut req = get_request(&format!("/bucket/{long_key}"), "localhost");
    let Err(err) = prepare(&mut req, &ccx).await else { panic!("long key: expected KeyTooLongError") };
    assert_eq!(*err.code(), S3ErrorCode::KeyTooLongError);
}

#[tokio::test]
async fn no_route_configured_keeps_legacy_behavior() {
    let parts = ctx();
    let ccx = ccx(&parts, false, false, None);

    let mut req = get_request("/Foo/bar", "localhost");
    let Err(err) = prepare(&mut req, &ccx).await else { panic!("expected legacy rejection") };
    assert_eq!(*err.code(), S3ErrorCode::InvalidBucketName);
}

#[tokio::test]
async fn post_to_invalid_bucket_still_rejected_when_route_misses() {
    struct PostOnlyRoute;
    #[async_trait::async_trait]
    impl S3Route for PostOnlyRoute {
        fn is_match(&self, method: &Method, _: &Uri, _: &HeaderMap, _: &mut Extensions) -> bool {
            method == Method::PUT // deliberately mismatch POST uploads
        }

        async fn call(&self, _: S3Request<Body>) -> crate::error::S3Result<S3Response<Body>> {
            unreachable!("PostOnlyRoute never matches POST")
        }
    }

    let parts = ctx();
    let ccx = ccx(&parts, false, false, Some(&PostOnlyRoute));

    let boundary = "------------------------X";
    let body = format!(
        "--{boundary}\r\nContent-Disposition: form-data; name=\"key\"\r\n\r\nfoo.txt\r\n\
         --{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"f\"\r\n\
         Content-Type: text/plain\r\n\r\nhello\r\n--{boundary}--\r\n"
    );

    let mut req = Request::from(
        hyper::Request::builder()
            .method(Method::POST)
            .uri("http://localhost/Foo/bar")
            .header(crate::header::HOST, "localhost")
            .header(hyper::header::CONTENT_TYPE, format!("multipart/form-data; boundary={boundary}"))
            .body(Body::from(body))
            .unwrap(),
    );

    let Err(err) = prepare(&mut req, &ccx).await else {
        panic!("POST to invalid bucket must stay rejected")
    };
    assert_eq!(*err.code(), S3ErrorCode::InvalidBucketName);
}

#[tokio::test]
async fn legacy_error_precedence_preserved_on_route_miss() {
    // With a route configured but missed, path validation still happens
    // before signature verification exactly as in the no-route pipeline:
    // a bad signature must not shadow the path error.
    let parts = ctx();
    let ccx = ccx(&parts, true, false, Some(&MatchNoneRoute));

    let mut req = Request::from(
        hyper::Request::builder()
            .method(Method::GET)
            .uri("http://localhost/Foo/bar")
            .header(crate::header::HOST, "localhost")
            .header("x-amz-date", hyper::header::HeaderValue::from_static("20260826T000000Z"))
            .header(
                "authorization",
                hyper::header::HeaderValue::from_static(
                    "AWS4-HMAC-SHA256 Credential=AKIAIOSFODNN7EXAMPLE/20260826/us-east-1/s3/aws4_request, \
                     SignedHeaders=host;x-amz-date, \
                     Signature=0000000000000000000000000000000000000000000000000000000000000000",
                ),
            )
            .body(Body::empty())
            .unwrap(),
    );

    let Err(err) = prepare(&mut req, &ccx).await else { panic!("invalid path must be rejected") };
    assert_eq!(
        *err.code(),
        S3ErrorCode::InvalidBucketName,
        "validation precedes signature verification on route miss: {err:?}"
    );
}

/// Builds a correctly signed `SigV4` header-auth request against the given
/// Host, using the same credentials as [`ctx`].
fn signed_vhost_request(host: &str) -> Request {
    let now = time::OffsetDateTime::now_utc();
    let date_fmt = time::macros::format_description!("[year][month][day]T[hour][minute][second]Z");
    let scope_fmt = time::macros::format_description!("[year][month][day]");
    let date = now.format(&date_fmt).expect("format");
    let scope_date = now.format(&scope_fmt).expect("format");
    let region = "us-east-1";

    let amz_date = AmzDate::parse(date.as_str()).expect("valid date");
    let payload_hash = "UNSIGNED-PAYLOAD";

    let signed_headers = [
        ("host", host),
        ("x-amz-content-sha256", payload_hash),
        ("x-amz-date", date.as_str()),
    ];

    let canonical_request =
        sig_v4::create_canonical_request(&Method::GET, "/", &[] as &[(&str, &str)], signed_headers, sig_v4::Payload::Unsigned);
    let string_to_sign = sig_v4::create_string_to_sign(&canonical_request, &amz_date, region, "s3");
    let signature = sig_v4::calculate_signature(
        &string_to_sign,
        &SecretKey::from("wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY"),
        &amz_date,
        region,
        "s3",
    );

    let authorization = format!(
        "AWS4-HMAC-SHA256 Credential=AKIAIOSFODNN7EXAMPLE/{scope_date}/{region}/s3/aws4_request, \
         SignedHeaders=host;x-amz-content-sha256;x-amz-date, \
         Signature={}",
        signature.as_str()
    );

    let mut builder = hyper::Request::builder()
        .method(Method::GET)
        .uri(format!("http://{host}/"))
        .header(crate::header::HOST, host)
        .header("x-amz-date", hyper::header::HeaderValue::from_str(date.as_str()).unwrap())
        .header("x-amz-content-sha256", hyper::header::HeaderValue::from_static(payload_hash));
    builder = builder.header("authorization", authorization);
    Request::from(builder.body(Body::empty()).unwrap())
}

#[tokio::test]
async fn signed_vhost_underscore_subdomain_reaches_matched_route() {
    // A validly-signed virtual-hosted request with an underscore bucket must
    // both verify (full vh context reaches signature verification) and reach
    // the matched custom route despite failing AWS bucket-name rules.
    let parts = ctx();
    let route = MatchAllRoute::new();
    let route_ctx = ccx(&parts, true, true, Some(&route));

    let mut req = signed_vhost_request("my_bucket.example.com");
    let ans = prepare(&mut req, &route_ctx)
        .await
        .unwrap_or_else(|e| panic!("signed vhost: unexpected {e:?}"));
    assert!(matches!(ans, Prepare::CustomRoute));
    assert!(
        req.s3ext
            .credentials
            .as_ref()
            .is_some_and(|c| c.access_key == "AKIAIOSFODNN7EXAMPLE"),
        "handler-bound credentials must be populated"
    );

    // Same signed request with the route removed keeps the original rejection
    // (bucket-name validation), not a signature error.
    let no_route = ccx(&parts, true, true, None);
    let mut req = signed_vhost_request("my_bucket.example.com");
    let Err(err) = prepare(&mut req, &no_route).await else {
        panic!("underscore bucket without route must stay rejected")
    };
    assert_eq!(*err.code(), S3ErrorCode::InvalidBucketName);
}

#[tokio::test]
async fn valid_path_on_route_miss_resolves_operation_normally() {
    let parts = ctx();
    let ccx = ccx(&parts, false, false, Some(&MatchNoneRoute));

    // A conforming path must pass validation and continue into normal
    // operation resolution even when a route is configured.
    let mut req = get_request("/logs", "localhost");
    let ans = prepare(&mut req, &ccx)
        .await
        .unwrap_or_else(|e| panic!("valid path: unexpected {e:?}"));
    match ans {
        Prepare::S3(op) => assert_eq!(op.name(), "ListObjects"),
        Prepare::CustomRoute => panic!("missed route must not claim S3 traffic"),
    }
}
