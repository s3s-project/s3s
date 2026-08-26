// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

//! Custom route path-validation bypass tests.
//!
//! Contract under test: when a custom route is configured, requests whose
//! paths are not valid S3 bucket/keys must still reach the route when it
//! matches, while non-matching (and no-route) behavior keeps the original
//! errors.

use super::*;
use crate::auth::{SecretKey, SimpleAuth};
use crate::config::{S3ConfigProvider, StaticConfigProvider};
use crate::error::S3ErrorCode;
use crate::host::SingleDomain;
use crate::http::{Body, Request};
use crate::protocol::S3Response;
use crate::route::S3Route;
use crate::s3_trait::S3;
use hyper::http::Extensions;
use hyper::{HeaderMap, Method, Uri};
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

/// Route matching only the root path (virtual-hosted style requests).
struct RootPathRoute;

#[async_trait::async_trait]
impl S3Route for RootPathRoute {
    fn is_match(&self, _: &Method, uri: &Uri, _: &HeaderMap, _: &mut Extensions) -> bool {
        uri.path() == "/"
    }

    async fn call(&self, _: S3Request<Body>) -> crate::error::S3Result<S3Response<Body>> {
        unreachable!("RootPathRoute test never invokes call")
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

fn bad_sig_request(path: &str) -> Request {
    use hyper::header::HeaderValue;
    Request::from(
        hyper::Request::builder()
            .method(Method::GET)
            .uri(format!("http://localhost{path}"))
            .header(crate::header::HOST, "localhost")
            .header("x-amz-date", HeaderValue::from_static("20260826T000000Z"))
            .header(
                "authorization",
                HeaderValue::from_static(
                    "AWS4-HMAC-SHA256 Credential=AKIAIOSFODNN7EXAMPLE/20260826/us-east-1/s3/aws4_request, \
                     SignedHeaders=host;x-amz-date, \
                     Signature=0000000000000000000000000000000000000000000000000000000000000000",
                ),
            )
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
    assert!(req.s3ext.path_parse_error.is_none());
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
async fn bad_signature_beats_deferred_path_error() {
    // Documented precedence shift: with a route configured, signature
    // verification runs before the deferred path error surfaces.
    let parts = ctx();
    let ccx = ccx(&parts, true, false, Some(&MatchNoneRoute));

    let mut req = bad_sig_request("/Foo/bar");
    let Err(err) = prepare(&mut req, &ccx).await else { panic!("bad signature must be rejected") };
    assert_ne!(
        *err.code(),
        S3ErrorCode::InvalidBucketName,
        "signature stage must run first when a route is configured: {err:?}"
    );
}

#[tokio::test]
async fn vhost_underscore_host_reaches_matched_route_but_rejected_without_one() {
    let parts = ctx();

    // Underscore subdomain is rejected by domain validation but must reach
    // the custom route untouched.
    let route_ctx = ccx(&parts, false, true, Some(&RootPathRoute));
    let mut req = get_request("/", "my_bucket.example.com");
    let ans = prepare(&mut req, &route_ctx)
        .await
        .unwrap_or_else(|e| panic!("vhost: unexpected {e:?}"));
    assert!(matches!(ans, Prepare::CustomRoute));

    // Same request without any route keeps the original rejection.
    let no_route_ctx = ccx(&parts, false, true, None);
    let mut req = get_request("/", "my_bucket.example.com");
    let Err(err) = prepare(&mut req, &no_route_ctx).await else {
        panic!("underscore host without route must stay rejected")
    };
    // The underscore subdomain parses as a virtual-host bucket ("my_bucket"),
    // which then fails bucket-name validation.
    assert_eq!(*err.code(), S3ErrorCode::InvalidBucketName);
}
