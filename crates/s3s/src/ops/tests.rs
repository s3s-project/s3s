// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

use super::*;

pub(super) struct NeverGetSecretKeyAuth;

#[async_trait::async_trait]
impl crate::auth::S3Auth for NeverGetSecretKeyAuth {
    async fn get_secret_key(&self, _access_key: &str) -> crate::error::S3Result<crate::auth::SecretKey> {
        panic!("secret key lookup must not occur for a wrong-region request")
    }
}

use crate::service::S3Service;
use stdx::mem::output_size;

#[test]
fn future_size() {
    // Guards against accidental future-size bloat in the dispatch path.
    macro_rules! future_size {
        ($f:path, $cap:expr) => {
            (stringify!($f), output_size(&$f), $cap)
        };
    }

    #[rustfmt::skip]
    let sizes = [
        future_size!(S3Service::call,                           3300),
        future_size!(call,                                      1900),
        future_size!(prepare,                                   1850),
        future_size!(SignatureContext::check,                    880),
        future_size!(SignatureContext::v2_check,                 270),
        future_size!(SignatureContext::v2_check_presigned_url,   120),
        future_size!(SignatureContext::v2_check_header_auth,     150),
        future_size!(SignatureContext::v4_check,                 760),
        future_size!(SignatureContext::v4_check_post_signature,  580),
        future_size!(SignatureContext::v4_check_presigned_url,   535),
        future_size!(SignatureContext::v4_check_header_auth,     645),
    ];

    println!("{sizes:#?}");
    for (name, size, cap) in sizes {
        assert!(size <= cap, "{name:?} size changed: cap {cap}, now {size}");
    }
}

fn get_object_microbench_body() -> crate::dto::StreamingBlob {
    crate::dto::StreamingBlob::from_bytes(bytes::Bytes::from_static(&[b'a'; 1024]))
}

fn get_object_microbench_last_modified() -> crate::dto::Timestamp {
    crate::dto::Timestamp::parse(crate::dto::TimestampFormat::HttpDate, "Wed, 21 Oct 2015 07:28:00 GMT").unwrap()
}

fn get_object_microbench_common_output(metadata_len: usize, include_timestamps: bool) -> crate::dto::GetObjectOutput {
    let metadata = (metadata_len != 0).then(|| {
        (0..metadata_len)
            .map(|idx| (format!("bench-key-{idx}"), format!("bench-value-{idx}")))
            .collect()
    });

    crate::dto::GetObjectOutput {
        accept_ranges: Some("bytes".to_owned()),
        body: Some(get_object_microbench_body()),
        cache_control: Some("no-cache".to_owned()),
        content_length: Some(1024),
        content_type: Some("application/octet-stream".to_owned()),
        e_tag: Some(crate::dto::ETag::Strong("0123456789abcdef0123456789abcdef".to_owned())),
        last_modified: include_timestamps.then(get_object_microbench_last_modified),
        metadata,
        ..Default::default()
    }
}

fn get_object_microbench_http_request() -> crate::HttpRequest {
    hyper::Request::builder()
        .method(hyper::Method::GET)
        .uri("http://localhost/bench-bucket/bench-key")
        .body(crate::http::Body::empty())
        .unwrap()
}

fn get_object_microbench_prepared_request() -> crate::http::Request {
    let mut req = crate::http::Request::from(get_object_microbench_http_request());
    req.s3ext.s3_path = Some(crate::path::S3Path::object("bench-bucket", "bench-key"));
    req
}

fn get_object_microbench_hundredths(numerator: u128, denominator: u128) -> String {
    let scaled = numerator.saturating_mul(100) / denominator;
    format!("{}.{:02}", scaled / 100, scaled % 100)
}

fn run_get_object_microbench_case<F>(name: &'static str, iterations: u64, mut f: F)
where
    F: FnMut() -> crate::error::S3Result<crate::http::Response>,
{
    use std::hint::black_box;

    for _ in 0..1_000 {
        let res = f().unwrap();
        black_box(res.headers.len());
    }

    let start = std::time::Instant::now();
    let mut header_count = 0u128;
    for _ in 0..iterations {
        let res = f().unwrap();
        header_count = header_count.saturating_add(res.headers.len() as u128);
        black_box(res);
    }
    let elapsed = start.elapsed();
    println!(
        "s3s_get_serialize_bench case={name} iterations={iterations} total_ns={} ns_per_op={} avg_headers={}",
        elapsed.as_nanos(),
        get_object_microbench_hundredths(elapsed.as_nanos(), u128::from(iterations)),
        get_object_microbench_hundredths(header_count, u128::from(iterations))
    );
}

async fn run_get_object_async_microbench_case<F, Fut>(name: &'static str, iterations: u64, mut f: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = usize>,
{
    use std::hint::black_box;

    for _ in 0..1_000 {
        black_box(f().await);
    }

    let start = std::time::Instant::now();
    let mut value_count = 0u128;
    for _ in 0..iterations {
        value_count = value_count.saturating_add(black_box(f().await) as u128);
    }
    let elapsed = start.elapsed();
    println!(
        "s3s_get_output_path_bench case={name} iterations={iterations} total_ns={} ns_per_op={} avg_value={}",
        elapsed.as_nanos(),
        get_object_microbench_hundredths(elapsed.as_nanos(), u128::from(iterations)),
        get_object_microbench_hundredths(value_count, u128::from(iterations))
    );
}

fn get_object_microbench_drain_body(mut body: crate::http::Body) -> usize {
    use std::pin::Pin;
    use std::task::Context;
    use std::task::Poll;

    let waker = futures::task::noop_waker();
    let mut cx = Context::from_waker(&waker);
    let mut body = Pin::new(&mut body);
    let mut bytes = 0usize;
    loop {
        match http_body::Body::poll_frame(body.as_mut(), &mut cx) {
            Poll::Ready(Some(Ok(frame))) => {
                if let Ok(data) = frame.into_data() {
                    bytes = bytes.saturating_add(data.len());
                }
            }
            Poll::Ready(Some(Err(err))) => panic!("body poll failed: {err}"),
            Poll::Ready(None) => return bytes,
            Poll::Pending => panic!("microbench body unexpectedly returned Pending"),
        }
    }
}

#[test]
#[ignore = "focused microbenchmark for GET response serialization attribution"]
fn get_object_response_serialization_microbench() {
    use crate::dto::ETag;
    use crate::dto::TimestampFormat;
    use crate::http;

    const DEFAULT_ITERS: u64 = 200_000;
    let iterations = std::env::var("S3S_GET_SERIALIZE_BENCH_ITERS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(DEFAULT_ITERS);
    assert!(iterations != 0, "S3S_GET_SERIALIZE_BENCH_ITERS must be greater than 0");

    run_get_object_microbench_case("response_default", iterations, || Ok(http::Response::default()));
    run_get_object_microbench_case("set_stream_body_only", iterations, || {
        let mut res = http::Response::default();
        http::set_stream_body(&mut res, get_object_microbench_body());
        Ok(res)
    });
    run_get_object_microbench_case("generated_empty_output", iterations, || {
        generated::GetObject::serialize_http(crate::dto::GetObjectOutput::default())
    });
    run_get_object_microbench_case("generated_body_only", iterations, || {
        generated::GetObject::serialize_http(crate::dto::GetObjectOutput {
            body: Some(get_object_microbench_body()),
            ..Default::default()
        })
    });
    run_get_object_microbench_case("manual_common_headers", iterations, || {
        let mut res = http::Response::default();
        http::set_stream_body(&mut res, get_object_microbench_body());
        http::add_opt_header(&mut res, hyper::header::CONTENT_LENGTH, Some(1024_i64))?;
        http::add_opt_header(&mut res, hyper::header::CONTENT_TYPE, Some("application/octet-stream".to_owned()))?;
        http::add_opt_header(
            &mut res,
            hyper::header::ETAG,
            Some(ETag::Strong("0123456789abcdef0123456789abcdef".to_owned())),
        )?;
        Ok(res)
    });
    run_get_object_microbench_case("manual_common_headers_5", iterations, || {
        let mut res = http::Response::default();
        http::set_stream_body(&mut res, get_object_microbench_body());
        http::add_opt_header(&mut res, crate::header::ACCEPT_RANGES, Some("bytes".to_owned()))?;
        http::add_opt_header(&mut res, crate::header::CACHE_CONTROL, Some("no-cache".to_owned()))?;
        http::add_opt_header(&mut res, hyper::header::CONTENT_LENGTH, Some(1024_i64))?;
        http::add_opt_header(&mut res, hyper::header::CONTENT_TYPE, Some("application/octet-stream".to_owned()))?;
        http::add_opt_header(
            &mut res,
            hyper::header::ETAG,
            Some(ETag::Strong("0123456789abcdef0123456789abcdef".to_owned())),
        )?;
        Ok(res)
    });
    run_get_object_microbench_case("manual_common_headers_5_last_modified", iterations, || {
        let mut res = http::Response::default();
        http::set_stream_body(&mut res, get_object_microbench_body());
        http::add_opt_header(&mut res, crate::header::ACCEPT_RANGES, Some("bytes".to_owned()))?;
        http::add_opt_header(&mut res, crate::header::CACHE_CONTROL, Some("no-cache".to_owned()))?;
        http::add_opt_header(&mut res, hyper::header::CONTENT_LENGTH, Some(1024_i64))?;
        http::add_opt_header(&mut res, hyper::header::CONTENT_TYPE, Some("application/octet-stream".to_owned()))?;
        http::add_opt_header(
            &mut res,
            hyper::header::ETAG,
            Some(ETag::Strong("0123456789abcdef0123456789abcdef".to_owned())),
        )?;
        http::add_opt_header_timestamp(
            &mut res,
            hyper::header::LAST_MODIFIED,
            Some(get_object_microbench_last_modified()),
            TimestampFormat::HttpDate,
        )?;
        Ok(res)
    });
    run_get_object_microbench_case("get_object_common_no_timestamp", iterations, || {
        generated::GetObject::serialize_http(get_object_microbench_common_output(0, false))
    });
    run_get_object_microbench_case("get_object_common_timestamp", iterations, || {
        generated::GetObject::serialize_http(get_object_microbench_common_output(0, true))
    });
    run_get_object_microbench_case("get_object_common_metadata_2", iterations, || {
        generated::GetObject::serialize_http(get_object_microbench_common_output(2, true))
    });
}

struct GetObjectOutputPathMicrobenchS3;

#[async_trait::async_trait]
impl crate::s3_trait::S3 for GetObjectOutputPathMicrobenchS3 {
    async fn get_object(
        &self,
        _req: crate::S3Request<crate::dto::GetObjectInput>,
    ) -> crate::error::S3Result<crate::S3Response<crate::dto::GetObjectOutput>> {
        Ok(crate::S3Response::new(get_object_microbench_common_output(0, true)))
    }
}

fn get_object_microbench_call_context<'a>(
    s3: &'a std::sync::Arc<dyn crate::s3_trait::S3>,
    config: &'a std::sync::Arc<dyn crate::config::S3ConfigProvider>,
) -> CallContext<'a> {
    CallContext {
        s3,
        config,
        host: None,
        auth: None,
        access: None,
        route: None,
        validation: None,
    }
}

async fn run_get_object_operation_attribution_microbench_cases(
    iterations: u64,
    s3: &std::sync::Arc<dyn crate::s3_trait::S3>,
    ccx: &CallContext<'_>,
) {
    use std::hint::black_box;

    run_get_object_async_microbench_case("request_from_http", iterations, || async {
        let req = crate::http::Request::from(get_object_microbench_http_request());
        let value = req.uri.path().len();
        black_box(req);
        value
    })
    .await;
    run_get_object_async_microbench_case("prepare_path_style_get", iterations, || async {
        let mut req = crate::http::Request::from(get_object_microbench_http_request());
        let prep = super::prepare(&mut req, ccx).await.unwrap();
        let value = match prep {
            Prepare::S3(op) => op.name().len(),
            Prepare::CustomRoute => 0,
        };
        black_box(req);
        value
    })
    .await;
    run_get_object_async_microbench_case("get_object_deserialize_http", iterations, || async {
        let mut req = get_object_microbench_prepared_request();
        let input = generated::GetObject::deserialize_http(&mut req).unwrap();
        let value = input.bucket.len() + input.key.len();
        black_box((req, input));
        value
    })
    .await;
    run_get_object_async_microbench_case("s3_trait_get_object_direct", iterations, || async {
        let req = crate::S3Request {
            input: crate::dto::GetObjectInput::default(),
            method: hyper::Method::GET,
            uri: hyper::Uri::from_static("http://localhost/bench-bucket/bench-key"),
            headers: hyper::HeaderMap::default(),
            extensions: ::http::Extensions::default(),
            credentials: None,
            region: None,
            service: None,
            trailing_headers: None,
        };
        let resp = s3.get_object(req).await.unwrap();
        let value = usize::try_from(resp.output.content_length.unwrap_or_default()).unwrap_or_default();
        black_box(resp);
        value
    })
    .await;
    run_get_object_async_microbench_case("generated_get_object_operation_call", iterations, || async {
        let mut req = crate::http::Request::from(get_object_microbench_http_request());
        req.s3ext.s3_path = Some(crate::path::S3Path::object("bench-bucket", "bench-key"));
        let resp = generated::GetObject.call(ccx, &mut req).await.unwrap();
        let value = resp.headers.len();
        black_box(resp);
        value
    })
    .await;
    run_get_object_async_microbench_case("ops_call_path_style_get", iterations, || async {
        let mut req = crate::http::Request::from(get_object_microbench_http_request());
        let resp = super::call(&mut req, ccx).await.unwrap();
        let value = resp.headers.len();
        black_box(resp);
        value
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "focused microbenchmark for GET output path attribution"]
async fn get_object_output_path_microbench() {
    use std::hint::black_box;

    const DEFAULT_ITERS: u64 = 100_000;
    let iterations = std::env::var("S3S_GET_OUTPUT_PATH_BENCH_ITERS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(DEFAULT_ITERS);
    assert!(iterations != 0, "S3S_GET_OUTPUT_PATH_BENCH_ITERS must be greater than 0");

    run_get_object_async_microbench_case("http_get_request_builder", iterations, || async {
        let req = get_object_microbench_http_request();
        let value = req.uri().path().len();
        black_box(req);
        value
    })
    .await;
    run_get_object_async_microbench_case("body_once_poll_frame", iterations, || async {
        let body = black_box(crate::http::Body::from(bytes::Bytes::from_static(&[b'a'; 1024])));
        black_box(get_object_microbench_drain_body(body))
    })
    .await;
    run_get_object_async_microbench_case("body_streaming_blob_poll_frame", iterations, || async {
        let body = black_box(crate::http::Body::from(get_object_microbench_body()));
        black_box(get_object_microbench_drain_body(body))
    })
    .await;
    run_get_object_async_microbench_case("serialize_common_and_poll_body", iterations, || async {
        let resp = black_box(generated::GetObject::serialize_http(get_object_microbench_common_output(0, true)).unwrap());
        let header_count = resp.headers.len();
        let body_bytes = get_object_microbench_drain_body(resp.body);
        black_box(header_count + body_bytes)
    })
    .await;

    let s3: std::sync::Arc<dyn crate::s3_trait::S3> = std::sync::Arc::new(GetObjectOutputPathMicrobenchS3);
    let config: std::sync::Arc<dyn crate::config::S3ConfigProvider> =
        std::sync::Arc::new(crate::config::StaticConfigProvider::default());
    let ccx = get_object_microbench_call_context(&s3, &config);
    run_get_object_operation_attribution_microbench_cases(iterations, &s3, &ccx).await;

    let service = crate::service::S3ServiceBuilder::new(GetObjectOutputPathMicrobenchS3).build();
    run_get_object_async_microbench_case("s3service_call_path_style_get", iterations, || {
        let service = service.clone();
        async move {
            let resp = service.call(get_object_microbench_http_request()).await.unwrap();
            let value = resp.headers().len();
            black_box(resp);
            value
        }
    })
    .await;
    run_get_object_async_microbench_case("s3service_call_and_poll_body", iterations, || {
        let service = service.clone();
        async move {
            let resp = service.call(get_object_microbench_http_request()).await.unwrap();
            let (parts, body) = resp.into_parts();
            parts.headers.len() + get_object_microbench_drain_body(body)
        }
    })
    .await;
}

/// Verifies that when an anonymous (unauthenticated) request is processed, the `None`
/// branch of the credential match **explicitly clears** `region` and `service`.
///
/// This is a regression guard for the fix to Problem 1: previously only `credentials`
/// was cleared, leaving stale values if the fields had been pre-set.
#[tokio::test]
async fn anonymous_request_clears_region_and_service() {
    use crate::config::{S3ConfigProvider, StaticConfigProvider};
    use crate::http::{Body, Request};
    use std::sync::Arc;

    struct NoOpS3;
    #[async_trait::async_trait]
    impl crate::s3_trait::S3 for NoOpS3 {}

    let s3: Arc<dyn crate::s3_trait::S3> = Arc::new(NoOpS3);
    let config: Arc<dyn S3ConfigProvider> = Arc::new(StaticConfigProvider::default());
    let ccx = CallContext {
        s3: &s3,
        config: &config,
        host: None,
        auth: None,
        access: None,
        route: None,
        validation: None,
    };

    let mut req = Request::from(
        hyper::Request::builder()
            .method(Method::GET)
            .uri("http://localhost/test-bucket/test-key")
            .body(Body::empty())
            .unwrap(),
    );

    // Pre-populate the fields to simulate hypothetical stale state and confirm
    // the explicit clearing in the None branch.
    req.s3ext.region = Some("leftover-region".parse().unwrap());
    req.s3ext.service = Some("leftover-service".into());

    // Auth processing (and thus field assignment) happens before route resolution,
    // so the fields are cleared regardless of whether prepare() succeeds overall.
    let _ = super::prepare(&mut req, &ccx).await;

    assert_eq!(req.s3ext.region, None, "anonymous request must clear region");
    assert_eq!(req.s3ext.service, None, "anonymous request must clear service");
}

/// Verifies that when the signature credential carries no region (`SigV2` or anonymous)
/// the region from `VirtualHost` (provided by `S3Host`) is used as a fallback.
///
/// Covers the `VirtualHost` region integration added in Problem 3.
#[tokio::test]
async fn vh_region_fallback_for_anonymous_request() {
    use crate::config::{S3ConfigProvider, StaticConfigProvider};
    use crate::error::S3Result;
    use crate::host::{S3Host, VirtualHost};
    use crate::http::{Body, Request};
    use std::sync::Arc;

    struct NoOpS3;
    #[async_trait::async_trait]
    impl crate::s3_trait::S3 for NoOpS3 {}

    /// A test `S3Host` that always emits region "us-west-2" regardless of the Host value.
    struct RegionHost;
    impl S3Host for RegionHost {
        fn parse_host_header<'a>(&'a self, _host: &'a str) -> S3Result<VirtualHost<'a>> {
            Ok(VirtualHost::new("example.com").with_bucket("bucket").with_region("us-west-2"))
        }
    }

    let s3: Arc<dyn crate::s3_trait::S3> = Arc::new(NoOpS3);
    let config: Arc<dyn S3ConfigProvider> = Arc::new(StaticConfigProvider::default());
    let host = RegionHost;
    let ccx = CallContext {
        s3: &s3,
        config: &config,
        host: Some(&host),
        auth: None,
        access: None,
        route: None,
        validation: None,
    };

    // Virtual-hosted style request: Host header "bucket.example.com", path is the key.
    let mut req = Request::from(
        hyper::Request::builder()
            .method(Method::GET)
            .uri("http://bucket.example.com/test-key")
            .header(crate::header::HOST, "bucket.example.com")
            .body(Body::empty())
            .unwrap(),
    );

    let _ = super::prepare(&mut req, &ccx).await;

    assert_eq!(
        req.s3ext.region.as_ref().map(crate::region::Region::as_str),
        Some("us-west-2"),
        "S3Host region should be the fallback when credential provides no region"
    );
}

/// A repeated `Host` header must be rejected by `extract_host` instead of
/// silently picking the first value: signature verification signs every
/// value of a repeated header, so accepting only one would let routing and
/// the signature disagree about the host.
#[test]
fn extract_host_rejects_duplicate_host_header() {
    use crate::http::{Body, Request};

    let req = Request::from(
        hyper::Request::builder()
            .method(Method::GET)
            .uri("http://example.com/test-key")
            .header(crate::header::HOST, "example.com")
            .header(crate::header::HOST, "attacker.example.com")
            .body(Body::empty())
            .unwrap(),
    );

    let err = super::extract_host(&req).expect_err("duplicate Host must be rejected");
    assert_eq!(err.code(), &crate::S3ErrorCode::InvalidRequest);
    assert_eq!(err.message(), Some("duplicate header: Host"));
}

/// `extract_host` keeps resolving a single Host value and the HTTP/2/3
/// authority fallback.
#[test]
fn extract_host_accepts_single_value_and_authority() {
    use crate::http::{Body, Request};

    let req = Request::from(
        hyper::Request::builder()
            .method(Method::GET)
            .uri("http://example.com/test-key")
            .header(crate::header::HOST, "example.com")
            .body(Body::empty())
            .unwrap(),
    );
    assert_eq!(super::extract_host(&req).unwrap().as_deref(), Some("example.com"));

    let req = Request::from(
        hyper::Request::builder()
            .method(Method::GET)
            .version(::http::Version::HTTP_2)
            .uri("http://bucket.example.com:19000/test-key")
            .body(Body::empty())
            .unwrap(),
    );
    assert_eq!(super::extract_host(&req).unwrap().as_deref(), Some("bucket.example.com:19000"));
}

/// With an `S3Host` configured, an unrecognized host carrying a port
/// (e.g. `localhost:8014`) can never be a CNAME bucket and must fall back
/// to path-style parsing; a portless host that is a valid bucket name keeps
/// the CNAME fallback. See
/// [s3s-project/s3s#643](https://github.com/s3s-project/s3s/issues/643).
#[tokio::test]
async fn host_fallback_path_style_and_cname() {
    use crate::config::{S3ConfigProvider, StaticConfigProvider};
    use crate::host::MultiDomain;
    use crate::http::{Body, Request};
    use crate::path::S3Path;
    use std::sync::Arc;

    struct NoOpS3;
    #[async_trait::async_trait]
    impl crate::s3_trait::S3 for NoOpS3 {}

    let s3: Arc<dyn crate::s3_trait::S3> = Arc::new(NoOpS3);
    let config: Arc<dyn S3ConfigProvider> = Arc::new(StaticConfigProvider::default());
    let host = MultiDomain::new(["s3.example.com"]).unwrap();
    let ccx = CallContext {
        s3: &s3,
        config: &config,
        host: Some(&host),
        auth: None,
        access: None,
        route: None,
        validation: None,
    };

    // `localhost:8014` can never be a CNAME bucket -> path-style: `GET /`
    // parses as the root path (ListBuckets), not as a bucket named
    // "localhost:8014".
    let mut req = Request::from(
        hyper::Request::builder()
            .method(Method::GET)
            .uri("http://localhost:8014/")
            .header(crate::header::HOST, "localhost:8014")
            .body(Body::empty())
            .unwrap(),
    );
    let _ = super::prepare(&mut req, &ccx).await;
    assert!(
        matches!(req.s3ext.s3_path, Some(S3Path::Root)),
        "host with port must fall back to path-style"
    );

    // `localhost` is a valid bucket name -> CNAME fallback keeps the bucket.
    let mut req = Request::from(
        hyper::Request::builder()
            .method(Method::GET)
            .uri("http://localhost/")
            .header(crate::header::HOST, "localhost")
            .body(Body::empty())
            .unwrap(),
    );
    let _ = super::prepare(&mut req, &ccx).await;
    assert!(
        matches!(req.s3ext.s3_path, Some(S3Path::Bucket { ref bucket }) if bucket.as_ref() == "localhost"),
        "portless valid-bucket host must keep the CNAME fallback"
    );
}

/// With a path-style host rule configured, an unrecognized host matching it
/// is parsed as path-style even when it is a valid bucket name. See
/// [s3s-project/s3s#643](https://github.com/s3s-project/s3s/issues/643).
#[tokio::test]
async fn host_path_style_rule_is_path_style() {
    use crate::config::{S3ConfigProvider, StaticConfigProvider};
    use crate::host::MultiDomain;
    use crate::http::{Body, Request};
    use crate::path::S3Path;
    use regex::RegexSet;
    use std::sync::Arc;

    struct NoOpS3;
    #[async_trait::async_trait]
    impl crate::s3_trait::S3 for NoOpS3 {}

    let s3: Arc<dyn crate::s3_trait::S3> = Arc::new(NoOpS3);
    let config: Arc<dyn S3ConfigProvider> = Arc::new(StaticConfigProvider::default());
    let host = MultiDomain::new(["s3.example.com"])
        .unwrap()
        .with_path_style_hosts(RegexSet::new([r"^localhost$"]).unwrap());
    let ccx = CallContext {
        s3: &s3,
        config: &config,
        host: Some(&host),
        auth: None,
        access: None,
        route: None,
        validation: None,
    };

    // `localhost` would be a valid CNAME bucket, but the path-style rule
    // matches, so `GET /` parses as the root path (ListBuckets).
    let mut req = Request::from(
        hyper::Request::builder()
            .method(Method::GET)
            .uri("http://localhost/")
            .header(crate::header::HOST, "localhost")
            .body(Body::empty())
            .unwrap(),
    );
    let _ = super::prepare(&mut req, &ccx).await;
    assert!(
        matches!(req.s3ext.s3_path, Some(S3Path::Root)),
        "path-style host rule must parse matching hosts as path-style"
    );
}

/// With the CNAME-style fallback disabled on `SingleDomain`, an
/// unrecognized portless host is parsed as path-style. See
/// [s3s-project/s3s#643](https://github.com/s3s-project/s3s/issues/643).
#[tokio::test]
async fn single_domain_fallback_disabled_is_path_style() {
    use crate::config::{S3ConfigProvider, StaticConfigProvider};
    use crate::host::SingleDomain;
    use crate::http::{Body, Request};
    use crate::path::S3Path;
    use std::sync::Arc;

    struct NoOpS3;
    #[async_trait::async_trait]
    impl crate::s3_trait::S3 for NoOpS3 {}

    let s3: Arc<dyn crate::s3_trait::S3> = Arc::new(NoOpS3);
    let config: Arc<dyn S3ConfigProvider> = Arc::new(StaticConfigProvider::default());
    let host = SingleDomain::new("s3.example.com").unwrap().with_cname_fallback(false);
    let ccx = CallContext {
        s3: &s3,
        config: &config,
        host: Some(&host),
        auth: None,
        access: None,
        route: None,
        validation: None,
    };

    let mut req = Request::from(
        hyper::Request::builder()
            .method(Method::GET)
            .uri("http://localhost/")
            .header(crate::header::HOST, "localhost")
            .body(Body::empty())
            .unwrap(),
    );
    let _ = super::prepare(&mut req, &ccx).await;
    assert!(
        matches!(req.s3ext.s3_path, Some(S3Path::Root)),
        "disabled CNAME fallback must parse unrecognized hosts as path-style"
    );
}

#[test]
fn error_custom_headers() {
    fn redirect307(location: &str) -> S3Error {
        let mut err = S3Error::new(S3ErrorCode::TemporaryRedirect);

        err.set_headers({
            let mut headers = HeaderMap::new();
            headers.insert(crate::header::LOCATION, location.parse().unwrap());
            headers
        });

        err
    }

    let res = serialize_error(redirect307("http://example.com"), false).unwrap();
    assert_eq!(res.status, StatusCode::TEMPORARY_REDIRECT);
    assert_eq!(res.headers.get("location").unwrap(), "http://example.com");

    let body = res.body.bytes().unwrap();
    let body = std::str::from_utf8(&body).unwrap();
    assert_eq!(
        body,
        concat!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>",
            "<Error><Code>TemporaryRedirect</Code></Error>"
        )
    );
}

#[test]
fn extract_host_from_uri() {
    use crate::http::Request;
    use crate::ops::extract_host;

    let mut req = Request::from(
        hyper::Request::builder()
            .method(Method::GET)
            .version(::http::Version::HTTP_2)
            .uri("https://test.example.com:9001/rust.pdf?X-Amz-Algorithm=AWS4-HMAC-SHA256&X-Amz-Date=20251213T084305Z&X-Amz-SignedHeaders=host&X-Amz-Credential=rustfsadmin%2F20251213%2Fus-east-1%2Fs3%2Faws4_request&X-Amz-Expires=3600&X-Amz-Signature=57133ee54dab71c00a10106c33cde2615b301bd2cf00e2439f3ddb4bc999ec66")
            .body(Body::empty())
            .unwrap(),
    );

    let host = extract_host(&req).unwrap();
    assert_eq!(host, Some("test.example.com:9001".to_string()));

    req.version = ::http::Version::HTTP_11;
    let host = extract_host(&req).unwrap();
    assert_eq!(host, None);

    req.version = ::http::Version::HTTP_3;
    let host = extract_host(&req).unwrap();
    assert_eq!(host, Some("test.example.com:9001".to_string()));

    let mut req = Request::from(
        hyper::Request::builder()
            .version(::http::Version::HTTP_10)
            .method(Method::GET)
            .uri("http://another.example.org/resource")
            .body(Body::empty())
            .unwrap(),
    );
    let host = extract_host(&req).unwrap();
    assert_eq!(host, None);

    req.version = ::http::Version::HTTP_2;
    let host = extract_host(&req).unwrap();
    assert_eq!(host, Some("another.example.org".to_string()));

    req.version = ::http::Version::HTTP_3;
    let host = extract_host(&req).unwrap();
    assert_eq!(host, Some("another.example.org".to_string()));

    let req = Request::from(
        hyper::Request::builder()
            .method(Method::GET)
            .uri("/no/host/header")
            .header("Host", "header.example.com:8080")
            .body(Body::empty())
            .unwrap(),
    );
    let host = extract_host(&req).unwrap();
    assert_eq!(host, Some("header.example.com:8080".to_string()));

    let req = Request::from(
        hyper::Request::builder()
            .method(Method::GET)
            .uri("/no/host/header")
            .body(Body::empty())
            .unwrap(),
    );
    let host = extract_host(&req).unwrap();
    assert_eq!(host, None);
}

/// HTTP/2 requests lack a `Host` header (`:authority` is used instead).
/// Verify that `prepare()` injects `Host` from `uri.authority()`.
#[tokio::test]
async fn http2_authority_injected_as_host_header() {
    use crate::config::{S3ConfigProvider, StaticConfigProvider};
    use crate::http::{Body, Request};
    use std::sync::Arc;

    struct NoOpS3;
    #[async_trait::async_trait]
    impl crate::s3_trait::S3 for NoOpS3 {}

    let s3: Arc<dyn crate::s3_trait::S3> = Arc::new(NoOpS3);
    let config: Arc<dyn S3ConfigProvider> = Arc::new(StaticConfigProvider::default());
    let ccx = CallContext {
        s3: &s3,
        config: &config,
        host: None,
        auth: None,
        access: None,
        route: None,
        validation: None,
    };

    let mut req = Request::from(
        hyper::Request::builder()
            .method(Method::GET)
            .version(::http::Version::HTTP_2)
            .uri("http://s3.example.com/test-bucket/test-key")
            .body(Body::empty())
            .unwrap(),
    );

    assert!(req.headers.get(crate::header::HOST).is_none());
    let _ = super::prepare(&mut req, &ccx).await;

    let host = req
        .headers
        .get(crate::header::HOST)
        .expect("Host must be injected for HTTP/2");
    assert_eq!(host.to_str().unwrap(), "s3.example.com");
}

/// HTTP/1.1 request with an absolute URI (authority present) but no Host header.
/// `prepare()` must NOT inject Host in this case — injection is only for HTTP/2+.
#[tokio::test]
async fn http1_absolute_uri_no_host_injection() {
    use crate::config::{S3ConfigProvider, StaticConfigProvider};
    use crate::http::{Body, Request};
    use std::sync::Arc;

    struct NoOpS3;
    #[async_trait::async_trait]
    impl crate::s3_trait::S3 for NoOpS3 {}

    let s3: Arc<dyn crate::s3_trait::S3> = Arc::new(NoOpS3);
    let config: Arc<dyn S3ConfigProvider> = Arc::new(StaticConfigProvider::default());
    let ccx = CallContext {
        s3: &s3,
        config: &config,
        host: None,
        auth: None,
        access: None,
        route: None,
        validation: None,
    };

    let mut req = Request::from(
        hyper::Request::builder()
            .method(Method::GET)
            .version(::http::Version::HTTP_11)
            .uri("http://s3.example.com/test-bucket/test-key")
            .body(Body::empty())
            .unwrap(),
    );

    // Authority is present in the URI but this is HTTP/1.1 — no injection expected.
    assert!(req.uri.authority().is_some());
    assert!(req.headers.get(crate::header::HOST).is_none());

    let _ = super::prepare(&mut req, &ccx).await;

    // Host should still be absent (not injected for HTTP/1.x).
    assert!(req.headers.get(crate::header::HOST).is_none());
}

/// Existing Host header (HTTP/1.1) must not be overwritten.
#[tokio::test]
async fn http1_host_header_not_overwritten() {
    use crate::config::{S3ConfigProvider, StaticConfigProvider};
    use crate::http::{Body, Request};
    use std::sync::Arc;

    struct NoOpS3;
    #[async_trait::async_trait]
    impl crate::s3_trait::S3 for NoOpS3 {}

    let s3: Arc<dyn crate::s3_trait::S3> = Arc::new(NoOpS3);
    let config: Arc<dyn S3ConfigProvider> = Arc::new(StaticConfigProvider::default());
    let ccx = CallContext {
        s3: &s3,
        config: &config,
        host: None,
        auth: None,
        access: None,
        route: None,
        validation: None,
    };

    let mut req = Request::from(
        hyper::Request::builder()
            .method(Method::GET)
            .version(::http::Version::HTTP_11)
            .uri("/test-bucket/test-key")
            .header("host", "my-custom-host.example.com")
            .body(Body::empty())
            .unwrap(),
    );

    let _ = super::prepare(&mut req, &ccx).await;

    let host = req.headers.get(crate::header::HOST).unwrap();
    assert_eq!(host.to_str().unwrap(), "my-custom-host.example.com");
}

#[tokio::test]
async fn presigned_url_expires_0_should_be_expired() {
    use crate::S3ErrorCode;
    use crate::config::{S3ConfigProvider, StaticConfigProvider};
    use crate::http::{Body, OrderedQs};
    use crate::ops::signature::SignatureContext;
    use hyper::{HeaderMap, Method, Uri};
    use std::sync::Arc;

    let qs = OrderedQs::parse(concat!(
        "X-Amz-Algorithm=AWS4-HMAC-SHA256",
        "&X-Amz-Credential=AKIAIOSFODNN7EXAMPLE%2F20130524%2Fus-east-1%2Fs3%2Faws4_request",
        "&X-Amz-Date=20130524T000000Z",
        "&X-Amz-Expires=0",
        "&X-Amz-SignedHeaders=host",
        "&X-Amz-Signature=aeeed9bbccd4d02ee5c0109b86d86835f995330da4c265957d157751f604d404"
    ))
    .unwrap();

    let config: Arc<dyn S3ConfigProvider> = Arc::new(StaticConfigProvider::default());

    let method = Method::GET;
    let uri = Uri::from_static("https://s3.amazonaws.com/test.txt");
    let headers = HeaderMap::new();
    let mut body = Body::empty();

    let mut cx = SignatureContext {
        auth: None,
        config: &config,
        req_version: ::http::Version::HTTP_11,
        req_method: &method,
        req_uri: &uri,
        req_body: &mut body,
        qs: Some(&qs),
        hs: &headers,
        decoded_uri_path: "/test.txt",
        raw_uri_path: "/test.txt",
        vh_bucket: None,
        content_length: None,
        mime: None,
        decoded_content_length: None,
        transformed_body: None,
        multipart: None,
        trailing_headers: None,
    };

    let result = cx.v4_check_presigned_url().await;
    assert!(result.is_err());
    let err = result.unwrap_err();
    assert_eq!(err.code(), &S3ErrorCode::AccessDenied);
}

#[allow(clippy::too_many_lines)]
#[tokio::test]
async fn post_multipart_bucket_routes_to_post_object() {
    use crate::S3Request;
    use crate::auth::{SecretKey, SimpleAuth};
    use crate::config::{S3Config, S3ConfigProvider, StaticConfigProvider};
    use crate::http::{Body, Request};
    use crate::ops::CallContext;
    use bytes::Bytes;
    use hyper::Method;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct TestS3 {
        put_calls: AtomicUsize,
        post_calls: AtomicUsize,
    }

    #[async_trait::async_trait]
    impl crate::s3_trait::S3 for TestS3 {
        async fn put_object(
            &self,
            _req: S3Request<crate::dto::PutObjectInput>,
        ) -> crate::error::S3Result<crate::protocol::S3Response<crate::dto::PutObjectOutput>> {
            self.put_calls.fetch_add(1, Ordering::SeqCst);
            Ok(crate::protocol::S3Response::new(crate::dto::PutObjectOutput::default()))
        }

        async fn post_object(
            &self,
            _req: S3Request<crate::dto::PostObjectInput>,
        ) -> crate::error::S3Result<crate::protocol::S3Response<crate::dto::PostObjectOutput>> {
            self.post_calls.fetch_add(1, Ordering::SeqCst);
            Ok(crate::protocol::S3Response::new(crate::dto::PostObjectOutput::default()))
        }
    }

    let test_s3 = Arc::new(TestS3 {
        put_calls: AtomicUsize::new(0),
        post_calls: AtomicUsize::new(0),
    });
    let s3: Arc<dyn crate::s3_trait::S3> = test_s3.clone();
    let s3_config = S3Config {
        presigned_url_max_skew_time_secs: u32::MAX,
        ..Default::default()
    };
    let config: Arc<dyn S3ConfigProvider> = Arc::new(StaticConfigProvider::new(Arc::new(s3_config)));

    let access_key = "AKIAIOSFODNN7EXAMPLE";
    let secret_key: SecretKey = "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY".into();
    let auth = SimpleAuth::from_single(access_key, secret_key.clone());

    let ccx = CallContext {
        s3: &s3,
        config: &config,
        host: None,
        auth: Some(&auth),
        access: None,
        route: None,
        validation: None,
    };

    // Build a minimal multipart/form-data POST object request.
    // Signature is validated by v4_check_post_signature using the policy blob.
    let boundary = "------------------------c634190ccaebbc34";
    let bucket = "mc-test-bucket-32569";
    let key = "mc-test-object-7658";
    let policy_b64 = "eyJleHBpcmF0aW9uIjoiMjAyMC0xMC0wM1QxMzoyNTo0Ny4yMThaIiwiY29uZGl0aW9ucyI6W1siZXEiLCIkYnVja2V0IiwibWMtdGVzdC1idWNrZXQtMzI1NjkiXSxbImVxIiwiJGtleSIsIm1jLXRlc3Qtb2JqZWN0LTc2NTgiXSxbImVxIiwiJHgtYW16LWRhdGUiLCIyMDIwMDkyNlQxMzI1NDdaIl0sWyJlcSIsIiR4LWFtei1hbGdvcml0aG0iLCJBV1M0LUhNQUMtU0hBMjU2Il0sWyJlcSIsIiR4LWFtei1jcmVkZW50aWFsIiwiQUtJQUlPU0ZPRE5ON0VYQU1QTEUvMjAyMDA5MjYvdXMtZWFzdC0xL3MzL2F3czRfcmVxdWVzdCJdXX0=";
    let algorithm = "AWS4-HMAC-SHA256";
    let amz_date = s3s_sigv4::AmzDate::parse("20200926T132547Z").unwrap();
    let amz_date_str = amz_date.fmt_iso8601();
    let credential = "AKIAIOSFODNN7EXAMPLE/20200926/us-east-1/s3/aws4_request";
    let region = "us-east-1";
    let service = "s3";
    let signature = s3s_sigv4::calculate_signature(policy_b64, secret_key.expose(), &amz_date, region, service);

    let body = format!(
        concat!(
            "--{b}\r\n",
            "Content-Disposition: form-data; name=\"x-amz-signature\"\r\n\r\n",
            "{signature}\r\n",
            "--{b}\r\n",
            "Content-Disposition: form-data; name=\"bucket\"\r\n\r\n",
            "{bucket}\r\n",
            "--{b}\r\n",
            "Content-Disposition: form-data; name=\"policy\"\r\n\r\n",
            "{policy_b64}\r\n",
            "--{b}\r\n",
            "Content-Disposition: form-data; name=\"x-amz-algorithm\"\r\n\r\n",
            "{algorithm}\r\n",
            "--{b}\r\n",
            "Content-Disposition: form-data; name=\"x-amz-credential\"\r\n\r\n",
            "{credential}\r\n",
            "--{b}\r\n",
            "Content-Disposition: form-data; name=\"x-amz-date\"\r\n\r\n",
            "{amz_date}\r\n",
            "--{b}\r\n",
            "Content-Disposition: form-data; name=\"key\"\r\n\r\n",
            "{key}\r\n",
            "--{b}\r\n",
            "Content-Disposition: form-data; name=\"file\"; filename=\"a.txt\"\r\n",
            "Content-Type: text/plain\r\n\r\n",
            "hello\r\n",
            "--{b}--\r\n"
        ),
        amz_date = amz_date_str,
        b = boundary,
        signature = signature.as_str(),
        bucket = bucket,
        policy_b64 = policy_b64,
        algorithm = algorithm,
        credential = credential,
        key = key,
    );

    let mut req = Request::from(
        hyper::Request::builder()
            .method(Method::POST)
            .uri(format!("http://localhost/{bucket}"))
            .header(crate::header::HOST, "localhost")
            .header(
                crate::header::CONTENT_TYPE,
                hyper::header::HeaderValue::from_str(&format!("multipart/form-data; boundary={boundary}")).unwrap(),
            )
            .body(Body::from(Bytes::from(body)))
            .unwrap(),
    );

    // POST Object with `policy` field now validates the policy.
    // The test policy has expired (2020-10-03), so we expect AccessDenied.
    let result = super::prepare(&mut req, &ccx).await;
    match result {
        Err(err) => assert_eq!(*err.code(), crate::error::S3ErrorCode::AccessDenied),
        Ok(_) => panic!("expected AccessDenied error for expired policy"),
    }
}

/// A multipart POST whose path names an object is not a modeled S3
/// operation: `PostObject` binds to `/{Bucket}` only — the key is carried by
/// the form fields, never the URL path. AWS and `MinIO` reject such requests
/// with `MethodNotAllowed`; this test locks the behavior.
#[tokio::test]
async fn post_multipart_object_path_rejected_as_method_not_allowed() {
    use crate::auth::SecretKey;
    use std::sync::Arc;

    let s3: Arc<dyn crate::s3_trait::S3> = Arc::new(post_policy_test_helpers::TestS3NoOp);
    let config = post_policy_test_helpers::create_test_config(1024 * 1024);
    let auth = post_policy_test_helpers::create_test_auth();
    let ccx = post_policy_test_helpers::create_test_context(&s3, &config, &auth);

    let secret_key: SecretKey = "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY".into();
    let policy_json = &format!(
        r#"{{"expiration":"2030-01-01T00:00:00.000Z","conditions":[{}]}}"#,
        post_policy_test_helpers::BASE_CONDITIONS,
    );
    let mut req = post_policy_test_helpers::build_post_object_request(policy_json, "hello", &secret_key, false);
    // Reroute the request to the object level: `POST /bucket/key`.
    req.uri = req
        .uri
        .to_string()
        .replace("/test-bucket", "/test-bucket/test-key")
        .parse()
        .expect("valid test URI");

    let Err(err) = super::prepare(&mut req, &ccx).await else {
        panic!("multipart POST to an object path must be rejected");
    };
    assert_eq!(err.code(), &crate::error::S3ErrorCode::MethodNotAllowed);
}

// Helper functions for POST policy resource exhaustion tests

/// Helper to create a test S3 service that tracks POST calls
mod post_policy_test_helpers {
    use std::fmt::Write;

    use crate::S3Request;
    use crate::auth::{S3Auth, SecretKey, SimpleAuth};
    use crate::config::{S3Config, S3ConfigProvider, StaticConfigProvider};
    use crate::http::{Body, Request};
    use crate::ops::CallContext;
    use bytes::Bytes;
    use hyper::Method;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    pub struct TestS3WithPostTracking {
        pub post_calls: AtomicUsize,
    }

    #[async_trait::async_trait]
    impl crate::s3_trait::S3 for TestS3WithPostTracking {
        async fn post_object(
            &self,
            _req: S3Request<crate::dto::PostObjectInput>,
        ) -> crate::error::S3Result<crate::protocol::S3Response<crate::dto::PostObjectOutput>> {
            self.post_calls.fetch_add(1, Ordering::SeqCst);
            Ok(crate::protocol::S3Response::new(crate::dto::PostObjectOutput::default()))
        }
    }

    pub struct TestS3NoOp;

    #[async_trait::async_trait]
    impl crate::s3_trait::S3 for TestS3NoOp {}

    /// Create a test config with custom `post_object_max_file_size`
    pub fn create_test_config(post_object_max_file_size: u64) -> Arc<dyn S3ConfigProvider> {
        let config = S3Config {
            presigned_url_max_skew_time_secs: u32::MAX,
            post_object_max_file_size,
            expected_region: Some("us-east-1".parse().expect("valid test region")),
            ..Default::default()
        };
        Arc::new(StaticConfigProvider::new(Arc::new(config)))
    }

    /// Create auth and `CallContext` for testing
    pub fn create_test_context<'a>(
        s3: &'a Arc<dyn crate::s3_trait::S3>,
        config: &'a Arc<dyn S3ConfigProvider>,
        auth: &'a dyn S3Auth,
    ) -> CallContext<'a> {
        CallContext {
            s3,
            config,
            host: None,
            auth: Some(auth),
            access: None,
            route: None,
            validation: None,
        }
    }

    /// Create a `SimpleAuth` for testing
    pub fn create_test_auth() -> SimpleAuth {
        let access_key = "AKIAIOSFODNN7EXAMPLE";
        let secret_key: SecretKey = "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY".into();
        SimpleAuth::from_single(access_key, secret_key)
    }

    /// Standard conditions that cover the form fields added by `build_post_object_request`.
    ///
    /// Per the S3 spec, each non-exempt form field must have a matching condition in the policy.
    /// The fields added by the helper are: bucket, key, x-amz-algorithm, x-amz-credential,
    /// x-amz-date.  (x-amz-signature, policy, and file are exempt.)
    pub const BASE_CONDITIONS: &str = r#"{"bucket":"test-bucket"},["eq","$key","test-key"],["starts-with","$x-amz-algorithm",""],["starts-with","$x-amz-credential",""],["starts-with","$x-amz-date",""]"#;

    pub fn build_multipart_fields(list: &[(&str, &str)], boundary: &str) -> String {
        let mut d = String::new();
        for (name, value) in list {
            write!(
                &mut d,
                "--{boundary}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n"
            )
            .unwrap();
        }
        d
    }

    pub fn build_multipart_file_field(
        field_name: &str,
        filename: &str,
        content_type: &str,
        file_content: &str,
        boundary: &str,
    ) -> String {
        format!(
            concat!(
                "--{boundary}\r\n",
                "Content-Disposition: form-data; name=\"{field_name}\"; filename=\"{filename}\"\r\n",
                "Content-Type: {content_type}\r\n\r\n",
                "{file_content}\r\n",
                "--{boundary}--\r\n",
            ),
            boundary = boundary,
            field_name = field_name,
            filename = filename,
            content_type = content_type,
            file_content = file_content,
        )
    }

    /// Augment a test POST policy JSON with the required `SigV4` eq conditions
    /// (`x-amz-date`, `x-amz-credential`, `x-amz-algorithm`) so the request
    /// passes the verifier's policy-field-matching checks.
    pub fn augment_post_policy_for_test(policy_json: &str, amz_date: &str, credential: &str, algorithm: &str) -> String {
        let mut policy: serde_json::Value = serde_json::from_str(policy_json).expect("invalid test policy JSON");
        let conditions = policy["conditions"]
            .as_array_mut()
            .expect("policy must have a conditions array");
        conditions.push(serde_json::json!({"x-amz-date": amz_date}));
        conditions.push(serde_json::json!({"x-amz-credential": credential}));
        conditions.push(serde_json::json!({"x-amz-algorithm": algorithm}));
        policy.to_string()
    }

    /// Build a POST object request with a policy.
    ///
    /// The provided `policy_json` is augmented with the required `SigV4` POST
    /// policy eq conditions (`x-amz-date`, `x-amz-credential`, `x-amz-algorithm`)
    /// so the request passes the verifier's policy-field-matching checks.
    pub fn build_post_object_request(
        policy_json: &str,
        file_content: &str,
        secret_key: &SecretKey,
        with_content_type: bool,
    ) -> Request {
        let boundary = "------------------------test12345678";
        let bucket = "test-bucket";
        let key = "test-key";
        let amz_date = s3s_sigv4::AmzDate::parse("20250101T000000Z").unwrap();
        let amz_date_str = amz_date.fmt_iso8601();
        let region = "us-east-1";
        let service = "s3";
        let content_type = "text/plain";
        let algorithm = "AWS4-HMAC-SHA256";
        let credential = "AKIAIOSFODNN7EXAMPLE/20250101/us-east-1/s3/aws4_request";

        let policy_json = augment_post_policy_for_test(policy_json, amz_date_str.as_str(), credential, algorithm);
        let policy_b64 = base64_simd::STANDARD.encode_to_string(&policy_json);
        let signature = s3s_sigv4::calculate_signature(&policy_b64, secret_key.expose(), &amz_date, region, service);

        let fields = {
            let mut f = vec![
                ("x-amz-signature", signature.as_str()),
                ("bucket", bucket),
                ("policy", policy_b64.as_str()),
                ("x-amz-algorithm", algorithm),
                ("x-amz-credential", credential),
                ("x-amz-date", amz_date_str.as_str()),
                ("key", key),
            ];
            if with_content_type {
                f.push(("Content-Type", content_type));
            }
            f
        };

        let body = build_multipart_fields(&fields, boundary)
            + build_multipart_file_field("file", "test.txt", content_type, file_content, boundary).as_str();

        Request::from(
            hyper::Request::builder()
                .method(Method::POST)
                .uri(format!("http://localhost/{bucket}"))
                .header(crate::header::HOST, "localhost")
                .header(
                    crate::header::CONTENT_TYPE,
                    hyper::header::HeaderValue::from_str(&format!("multipart/form-data; boundary={boundary}")).unwrap(),
                )
                .header(hyper::header::CONTENT_LENGTH, body.len())
                .body(Body::from(Bytes::from(body)))
                .unwrap(),
        )
    }

    /// Build a POST object request whose body is split into many small chunks.
    ///
    /// This ensures that `aggregate_file_stream_limited` returns a `Vec<Bytes>`
    /// with multiple entries, so tests can distinguish between
    /// `vec_bytes.len()` (chunk count) and the total byte count.
    ///
    /// The provided `policy_json` is augmented with the required `SigV4` POST
    /// policy eq conditions so the request passes the verifier's checks.
    pub fn build_post_object_request_chunked(
        policy_json: &str,
        file_content: &str,
        secret_key: &SecretKey,
        chunk_size: usize,
    ) -> Request {
        let boundary = "------------------------test12345678";
        let bucket = "test-bucket";
        let key = "test-key";
        let amz_date = s3s_sigv4::AmzDate::parse("20250101T000000Z").unwrap();
        let amz_date_str = amz_date.fmt_iso8601();
        let region = "us-east-1";
        let service = "s3";
        let content_type = "text/plain";
        let algorithm = "AWS4-HMAC-SHA256";
        let credential = "AKIAIOSFODNN7EXAMPLE/20250101/us-east-1/s3/aws4_request";

        let policy_json = augment_post_policy_for_test(policy_json, amz_date_str.as_str(), credential, algorithm);
        let policy_b64 = base64_simd::STANDARD.encode_to_string(&policy_json);
        let signature = s3s_sigv4::calculate_signature(&policy_b64, secret_key.expose(), &amz_date, region, service);

        let body = build_multipart_fields(
            &[
                ("x-amz-signature", signature.as_str()),
                ("bucket", bucket),
                ("policy", &policy_b64),
                ("x-amz-algorithm", algorithm),
                ("x-amz-credential", credential),
                ("x-amz-date", amz_date_str.as_str()),
                ("key", key),
            ],
            boundary,
        ) + &build_multipart_file_field("file", "test.txt", content_type, file_content, boundary);

        // Split the body into small chunks to simulate a multi-chunk stream
        let body_bytes: Vec<u8> = body.into_bytes();
        let chunks: Vec<Result<http_body::Frame<Bytes>, std::convert::Infallible>> = body_bytes
            .chunks(chunk_size)
            .map(|c| Ok(http_body::Frame::data(Bytes::copy_from_slice(c))))
            .collect();

        let stream_body = http_body_util::StreamBody::new(futures::stream::iter(chunks));

        Request::from(
            hyper::Request::builder()
                .method(Method::POST)
                .uri(format!("http://localhost/{bucket}"))
                .header(crate::header::HOST, "localhost")
                .header(
                    crate::header::CONTENT_TYPE,
                    hyper::header::HeaderValue::from_str(&format!("multipart/form-data; boundary={boundary}")).unwrap(),
                )
                .body(Body::http_body(stream_body))
                .unwrap(),
        )
    }
}

mod put_object_max_size_tests {
    use super::*;

    use crate::auth::{SecretKey, SimpleAuth};
    use crate::config::{S3Config, S3ConfigProvider, StaticConfigProvider};
    use crate::dto::StreamingBlob;
    use crate::error::StdError;
    use crate::s3_trait::S3;
    use bytes::Bytes;
    use futures::StreamExt;
    use std::sync::Arc;

    /// The streaming-body predicate is model-driven: exactly the operations
    /// whose input carries a `StreamingBlob` payload (`PutObject`,
    /// `UploadPart`, `WriteGetObjectResponse`). `PostObject` is excluded —
    /// its body is the multipart file stream governed by
    /// `post_object_max_file_size`.
    #[test]
    fn has_streaming_body_matches_streaming_payload_operations() {
        assert!(PutObject.has_streaming_body());
        assert!(UploadPart.has_streaming_body());
        assert!(WriteGetObjectResponse.has_streaming_body());
        assert!(!PostObject.has_streaming_body());
        assert!(!GetObject.has_streaming_body());
        assert!(!DeleteObjects.has_streaming_body());
        assert!(!PutBucketPolicy.has_streaming_body());
    }

    fn test_config(put_object_max_size: Option<u64>) -> Arc<dyn S3ConfigProvider> {
        Arc::new(StaticConfigProvider::new(Arc::new(S3Config {
            put_object_max_size,
            presigned_url_max_skew_time_secs: u32::MAX,
            ..Default::default()
        })))
    }

    fn test_context<'a>(
        s3: &'a Arc<dyn S3>,
        config: &'a Arc<dyn S3ConfigProvider>,
        auth: Option<&'a dyn crate::auth::S3Auth>,
    ) -> CallContext<'a> {
        CallContext {
            s3,
            config,
            host: None,
            auth,
            access: None,
            route: None,
            validation: None,
        }
    }

    fn plain_put_request(body: Bytes) -> Request {
        Request::from(
            hyper::Request::builder()
                .method(Method::PUT)
                .uri("http://localhost/test-bucket/test-key")
                .header(crate::header::HOST, "localhost")
                .header(hyper::header::CONTENT_LENGTH, body.len())
                .body(Body::from(body))
                .unwrap(),
        )
    }

    fn upload_part_request(body: Bytes) -> Request {
        Request::from(
            hyper::Request::builder()
                .method(Method::PUT)
                .uri("http://localhost/test-bucket/test-key?partNumber=1&uploadId=test-upload")
                .header(crate::header::HOST, "localhost")
                .header(hyper::header::CONTENT_LENGTH, body.len())
                .body(Body::from(body))
                .unwrap(),
        )
    }

    async fn collect_stream<S>(mut stream: S) -> Result<Bytes, StdError>
    where
        S: futures::Stream<Item = Result<Bytes, StdError>> + Unpin,
    {
        let mut collected = Vec::new();
        while let Some(chunk) = stream.next().await {
            collected.extend_from_slice(&chunk?);
        }
        Ok(Bytes::from(collected))
    }

    async fn expect_limit_error(blob: StreamingBlob) -> StdError {
        let err = collect_stream(blob)
            .await
            .expect_err("stream should fail once the object-size limit is exceeded");
        assert!(err.to_string().contains("exceeds limit"), "limit error should be clear, got: {err}");
        err
    }

    fn signed_aws_chunked_put_request(chunk_data: &Bytes, access_key: &str, secret_key: &SecretKey) -> Request {
        let method = Method::PUT;
        let uri_path = "/test-bucket/test-key";
        let amz_date = s3s_sigv4::AmzDate::parse("20130524T000000Z").unwrap();
        let decoded_content_length = chunk_data.len().to_string();
        let headers_for_signing = [
            ("host", "s3.amazonaws.com"),
            ("x-amz-content-sha256", "STREAMING-AWS4-HMAC-SHA256-PAYLOAD"),
            ("x-amz-date", "20130524T000000Z"),
            ("x-amz-decoded-content-length", decoded_content_length.as_str()),
        ];
        let canonical_request = s3s_sigv4::create_canonical_request(
            method.as_str(),
            uri_path,
            &[] as &[(&str, &str)],
            headers_for_signing,
            s3s_sigv4::Payload::MultipleChunks,
        );
        let seed_string_to_sign = s3s_sigv4::create_string_to_sign(&canonical_request, &amz_date, "us-east-1", "s3");
        let seed_signature =
            s3s_sigv4::calculate_signature(&seed_string_to_sign, secret_key.expose(), &amz_date, "us-east-1", "s3");

        let chunk_string_to_sign = s3s_sigv4::create_chunk_string_to_sign(
            &amz_date,
            "us-east-1",
            "s3",
            seed_signature.as_str(),
            std::slice::from_ref(chunk_data),
        );
        let chunk_signature =
            s3s_sigv4::calculate_signature(&chunk_string_to_sign, secret_key.expose(), &amz_date, "us-east-1", "s3");
        let final_string_to_sign =
            s3s_sigv4::create_chunk_string_to_sign(&amz_date, "us-east-1", "s3", chunk_signature.as_str(), &[] as &[Vec<u8>]);
        let final_signature =
            s3s_sigv4::calculate_signature(&final_string_to_sign, secret_key.expose(), &amz_date, "us-east-1", "s3");

        let mut streaming_body = Vec::new();
        streaming_body
            .extend_from_slice(format!("{:x};chunk-signature={}\r\n", chunk_data.len(), chunk_signature.as_str()).as_bytes());
        streaming_body.extend_from_slice(chunk_data);
        streaming_body.extend_from_slice(b"\r\n");
        streaming_body.extend_from_slice(format!("0;chunk-signature={}\r\n\r\n", final_signature.as_str()).as_bytes());

        let authorization = format!(
            "AWS4-HMAC-SHA256 Credential={access_key}/20130524/us-east-1/s3/aws4_request, SignedHeaders=host;x-amz-content-sha256;x-amz-date;x-amz-decoded-content-length, Signature={}",
            seed_signature.as_str(),
        );

        Request::from(
            hyper::Request::builder()
                .method(method)
                .uri("https://s3.amazonaws.com/test-bucket/test-key")
                .header(crate::header::HOST, "s3.amazonaws.com")
                .header(hyper::header::CONTENT_LENGTH, streaming_body.len())
                .header("content-encoding", "aws-chunked")
                .header(crate::header::AUTHORIZATION, authorization)
                .header(crate::header::X_AMZ_CONTENT_SHA256, "STREAMING-AWS4-HMAC-SHA256-PAYLOAD")
                .header(crate::header::X_AMZ_DATE, "20130524T000000Z")
                .header(crate::header::X_AMZ_DECODED_CONTENT_LENGTH, decoded_content_length)
                .body(Body::from(Bytes::from(streaming_body)))
                .unwrap(),
        )
    }

    #[tokio::test]
    async fn none_leaves_plain_put_stream_unchanged() {
        let s3: Arc<dyn S3> = Arc::new(post_policy_test_helpers::TestS3NoOp);
        let config = test_config(None);
        let ccx = test_context(&s3, &config, None);
        let expected_body = Bytes::from_static(b"hello");
        let mut req = plain_put_request(expected_body.clone());

        let Prepare::S3(op) = super::prepare(&mut req, &ccx).await.expect("prepare should succeed") else {
            panic!("plain PUT should resolve to an S3 operation");
        };
        assert_eq!(op.name(), "PutObject");

        let input = generated::PutObject::deserialize_http(&mut req).expect("deserialize should succeed");
        let body = input.body.expect("put object input should carry a body");
        let collected = collect_stream(body).await.expect("unlimited stream should be readable");
        assert_eq!(collected, expected_body);
    }

    #[tokio::test]
    async fn rejects_oversized_plain_put_at_read_time() {
        let s3: Arc<dyn S3> = Arc::new(post_policy_test_helpers::TestS3NoOp);
        let config = test_config(Some(4));
        let ccx = test_context(&s3, &config, None);
        let mut req = plain_put_request(Bytes::from_static(b"hello"));

        let Prepare::S3(op) = super::prepare(&mut req, &ccx).await.expect("prepare should succeed") else {
            panic!("plain PUT should resolve to an S3 operation");
        };
        assert_eq!(op.name(), "PutObject");

        let input = generated::PutObject::deserialize_http(&mut req).expect("deserialize should succeed");
        expect_limit_error(input.body.expect("put object input should carry a body")).await;
    }

    #[tokio::test]
    async fn rejects_oversized_upload_part_at_read_time() {
        let s3: Arc<dyn S3> = Arc::new(post_policy_test_helpers::TestS3NoOp);
        let config = test_config(Some(4));
        let ccx = test_context(&s3, &config, None);
        let mut req = upload_part_request(Bytes::from_static(b"hello"));

        let Prepare::S3(op) = super::prepare(&mut req, &ccx).await.expect("prepare should succeed") else {
            panic!("upload part should resolve to an S3 operation");
        };
        assert_eq!(op.name(), "UploadPart");

        let input = generated::UploadPart::deserialize_http(&mut req).expect("deserialize should succeed");
        expect_limit_error(input.body.expect("upload part input should carry a body")).await;
    }

    #[tokio::test]
    async fn empty_body_operations_are_unaffected_when_limit_is_set() {
        let s3: Arc<dyn S3> = Arc::new(post_policy_test_helpers::TestS3NoOp);
        let config = test_config(Some(0));
        let ccx = test_context(&s3, &config, None);
        let mut req = Request::from(
            hyper::Request::builder()
                .method(Method::GET)
                .uri("http://localhost/test-bucket/test-key")
                .header(crate::header::HOST, "localhost")
                .body(Body::empty())
                .unwrap(),
        );

        let Prepare::S3(op) = super::prepare(&mut req, &ccx).await.expect("prepare should succeed") else {
            panic!("GET object should resolve to an S3 operation");
        };
        assert_eq!(op.name(), "GetObject");

        let input = generated::GetObject::deserialize_http(&mut req).expect("deserialize should succeed");
        assert_eq!(input.bucket, "test-bucket");
        assert_eq!(input.key, "test-key");
        assert!(
            req.body
                .store_all_limited(0)
                .await
                .expect("empty limited body should be readable")
                .is_empty()
        );
    }

    /// A `ListObjects` response that echoes a request-controlled object key
    /// containing a control character must not produce an invalid XML body:
    /// the serialization layer rejects it with an internal error.
    #[tokio::test]
    async fn list_objects_with_control_character_key_rejected_at_serialization() {
        use crate::config::{S3ConfigProvider, StaticConfigProvider};
        use crate::http::{Body, Request};

        struct ControlCharS3;
        #[async_trait::async_trait]
        impl crate::s3_trait::S3 for ControlCharS3 {
            async fn list_objects(
                &self,
                _req: crate::S3Request<crate::dto::ListObjectsInput>,
            ) -> crate::error::S3Result<crate::protocol::S3Response<crate::dto::ListObjectsOutput>> {
                let output = crate::dto::ListObjectsOutput {
                    name: Some("bucket".into()),
                    contents: Some(vec![crate::dto::Object {
                        key: Some("a\x01b".into()),
                        ..Default::default()
                    }]),
                    ..Default::default()
                };
                Ok(crate::protocol::S3Response::new(output))
            }
        }

        let s3: Arc<dyn crate::s3_trait::S3> = Arc::new(ControlCharS3);
        let config: Arc<dyn S3ConfigProvider> = Arc::new(StaticConfigProvider::default());
        let ccx = CallContext {
            s3: &s3,
            config: &config,
            host: None,
            auth: None,
            access: None,
            route: None,
            validation: None,
        };

        let mut req = Request::from(
            hyper::Request::builder()
                .method(Method::GET)
                .uri("http://localhost/bucket")
                .header(crate::header::HOST, "localhost")
                .body(Body::empty())
                .unwrap(),
        );

        let resp = super::call(&mut req, &ccx).await.unwrap();
        assert_eq!(resp.status, hyper::StatusCode::INTERNAL_SERVER_ERROR);
    }

    #[tokio::test]
    async fn post_object_keeps_post_object_max_file_size_when_put_limit_is_set() {
        let s3: Arc<dyn S3> = Arc::new(post_policy_test_helpers::TestS3NoOp);
        let config: Arc<dyn S3ConfigProvider> = Arc::new(StaticConfigProvider::new(Arc::new(S3Config {
            post_object_max_file_size: 1024,
            put_object_max_size: Some(1),
            expected_region: Some("us-east-1".parse().expect("valid test region")),
            presigned_url_max_skew_time_secs: u32::MAX,
            ..Default::default()
        })));
        let auth = post_policy_test_helpers::create_test_auth();
        let ccx = test_context(&s3, &config, Some(&auth));
        let secret_key: SecretKey = "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY".into();
        let policy_json = &format!(
            r#"{{"expiration":"2030-01-01T00:00:00.000Z","conditions":[{}]}}"#,
            post_policy_test_helpers::BASE_CONDITIONS,
        );
        let file_content = "hello";
        let mut req = post_policy_test_helpers::build_post_object_request(policy_json, file_content, &secret_key, false);

        let Prepare::S3(op) = super::prepare(&mut req, &ccx).await.expect("prepare should succeed") else {
            panic!("POST Object should resolve to an S3 operation");
        };
        assert_eq!(op.name(), "PostObject");

        let stream = req
            .s3ext
            .post_object_stream
            .take()
            .expect("post object stream should be prepared");
        let collected = collect_stream(stream)
            .await
            .expect("POST Object stream should use its own limit");
        assert_eq!(collected, Bytes::from_static(b"hello"));
    }

    #[tokio::test]
    async fn aws_chunked_put_limit_applies_after_decoding() {
        let s3: Arc<dyn S3> = Arc::new(post_policy_test_helpers::TestS3NoOp);
        let access_key = "AKIAIOSFODNN7EXAMPLE";
        let secret_key: SecretKey = "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY".into();
        let auth = SimpleAuth::from_single(access_key, secret_key.clone());

        let config = test_config(Some(5));
        let ccx = test_context(&s3, &config, Some(&auth));
        let decoded = Bytes::from_static(b"hello");
        let mut req = signed_aws_chunked_put_request(&decoded, access_key, &secret_key);

        let Prepare::S3(op) = super::prepare(&mut req, &ccx)
            .await
            .expect("prepare should decode before limiting")
        else {
            panic!("aws-chunked PUT should resolve to an S3 operation");
        };
        assert_eq!(op.name(), "PutObject");

        let input = generated::PutObject::deserialize_http(&mut req).expect("deserialize should succeed");
        let body = input.body.expect("put object input should carry a body");
        let collected = collect_stream(body)
            .await
            .expect("decoded stream at limit should be readable");
        assert_eq!(collected, decoded);

        let config = test_config(Some(4));
        let ccx = test_context(&s3, &config, Some(&auth));
        let mut req = signed_aws_chunked_put_request(&decoded, access_key, &secret_key);
        let Prepare::S3(op) = super::prepare(&mut req, &ccx)
            .await
            .expect("prepare should still finish before read-time limit")
        else {
            panic!("aws-chunked PUT should resolve to an S3 operation");
        };
        assert_eq!(op.name(), "PutObject");

        let input = generated::PutObject::deserialize_http(&mut req).expect("deserialize should succeed");
        expect_limit_error(input.body.expect("put object input should carry a body")).await;
    }
}

#[tokio::test]
async fn post_object_rejects_wrong_region() {
    use crate::auth::SecretKey;
    use crate::config::{S3Config, S3ConfigProvider, StaticConfigProvider};
    use std::sync::Arc;

    let s3: Arc<dyn crate::s3_trait::S3> = Arc::new(post_policy_test_helpers::TestS3NoOp);
    let s3_config = S3Config {
        presigned_url_max_skew_time_secs: u32::MAX,
        expected_region: Some("us-west-2".parse().expect("valid test region")),
        ..Default::default()
    };
    let config: Arc<dyn S3ConfigProvider> = Arc::new(StaticConfigProvider::new(Arc::new(s3_config)));
    let auth = NeverGetSecretKeyAuth;
    let ccx = post_policy_test_helpers::create_test_context(&s3, &config, &auth);
    let secret_key: SecretKey = "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY".into();
    let policy_json = &format!(
        r#"{{"expiration":"2030-01-01T00:00:00.000Z","conditions":[{}]}}"#,
        post_policy_test_helpers::BASE_CONDITIONS,
    );
    let mut req = post_policy_test_helpers::build_post_object_request(policy_json, "test", &secret_key, false);

    let Err(err) = super::prepare(&mut req, &ccx).await else {
        panic!("POST policy signed for another region should be rejected");
    };
    assert_eq!(err.code(), &crate::error::S3ErrorCode::AuthorizationHeaderMalformed);
}

/// Test that policy max < config max results in using policy max for file size limit
#[tokio::test]
async fn post_object_policy_max_smaller_than_config_max() {
    use crate::auth::SecretKey;
    use std::sync::Arc;
    use std::sync::atomic::AtomicUsize;

    let test_s3 = Arc::new(post_policy_test_helpers::TestS3WithPostTracking {
        post_calls: AtomicUsize::new(0),
    });
    let s3: Arc<dyn crate::s3_trait::S3> = test_s3.clone();

    // Set config max to 1MB
    let config = post_policy_test_helpers::create_test_config(1024 * 1024);

    let auth = post_policy_test_helpers::create_test_auth();
    let ccx = post_policy_test_helpers::create_test_context(&s3, &config, &auth);

    let secret_key: SecretKey = "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY".into();

    // Create a policy with content-length-range max of 100 bytes (< config max of 1MB)
    let policy_json = &format!(
        r#"{{"expiration":"2030-01-01T00:00:00.000Z","conditions":[["content-length-range",0,100],["eq","$Content-Type","text/plain"],{}]}}"#,
        post_policy_test_helpers::BASE_CONDITIONS,
    );
    let file_content = "a".repeat(50); // 50 bytes (within policy limit of 100 bytes)

    let mut req = post_policy_test_helpers::build_post_object_request(policy_json, &file_content, &secret_key, true);

    // This should succeed because file size (50 bytes) is within policy limit (100 bytes)
    // The important part is that the aggregation limit used is 100 bytes (policy max), not 1MB (config max)
    let result = super::prepare(&mut req, &ccx).await;
    assert!(result.is_ok(), "expected success for file within policy limit");
}

#[tokio::test]
async fn post_object_without_content_type_field_but_with_policy() {
    use crate::auth::SecretKey;
    use std::sync::Arc;
    use std::sync::atomic::AtomicUsize;

    let test_s3 = Arc::new(post_policy_test_helpers::TestS3WithPostTracking {
        post_calls: AtomicUsize::new(0),
    });
    let s3: Arc<dyn crate::s3_trait::S3> = test_s3.clone();

    // Set config max to 1MB
    let config = post_policy_test_helpers::create_test_config(1024 * 1024);

    let auth = post_policy_test_helpers::create_test_auth();
    let ccx = post_policy_test_helpers::create_test_context(&s3, &config, &auth);

    let secret_key: SecretKey = "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY".into();

    // Create a policy with content-length-range max of 100 bytes (< config max of 1MB)
    let policy_json = &format!(
        r#"{{"expiration":"2030-01-01T00:00:00.000Z","conditions":[["content-length-range",0,100],["eq","$Content-Type","text/plain"],{}]}}"#,
        post_policy_test_helpers::BASE_CONDITIONS,
    );
    let file_content = "a".repeat(50); // 50 bytes (within policy limit of 100 bytes)

    let mut req = post_policy_test_helpers::build_post_object_request(policy_json, &file_content, &secret_key, false);

    // This should fail because the request omits the Content-Type form field required by the policy,
    // even though the file size (50 bytes) is within the policy's content-length-range limit (0–100 bytes).
    let result = super::prepare(&mut req, &ccx).await;

    // Assert that we get the specific policy error for the missing Content-Type field.
    let Err(err) = result else {
        panic!("expected error for missing Content-Type field required by policy")
    };
    assert_eq!(
        *err.code(),
        S3ErrorCode::InvalidPolicyDocument,
        "unexpected error code for missing Content-Type field required by policy"
    );

    // The error message (or debug representation) should indicate that the `eq` condition
    // on Content-Type failed because the field was missing or mismatched.
    let msg = format!("{err:?}");
    let msg_lower = msg.to_lowercase();
    assert!(
        msg_lower.contains("content-type") || msg_lower.contains("content type"),
        "error message should mention Content-Type requirement, got: {msg}"
    );
    assert!(
        msg_lower.contains("eq"),
        "error message should indicate failure of the `eq` condition, got: {msg}"
    );
}

/// Test that file exceeding policy max but under config max is rejected
#[tokio::test]
async fn post_object_file_exceeds_policy_max_but_under_config_max() {
    use crate::auth::SecretKey;
    use std::sync::Arc;

    let s3: Arc<dyn crate::s3_trait::S3> = Arc::new(post_policy_test_helpers::TestS3NoOp);

    // Set config max to 10KB
    let config = post_policy_test_helpers::create_test_config(10 * 1024);

    let auth = post_policy_test_helpers::create_test_auth();
    let ccx = post_policy_test_helpers::create_test_context(&s3, &config, &auth);

    let secret_key: SecretKey = "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY".into();

    // Create a policy with content-length-range max of 100 bytes
    let policy_json = &format!(
        r#"{{"expiration":"2030-01-01T00:00:00.000Z","conditions":[["content-length-range",0,100],{}]}}"#,
        post_policy_test_helpers::BASE_CONDITIONS,
    );
    // Create a file with 150 bytes (exceeds policy max of 100 bytes, but under config max of 10KB)
    // This is the critical security test: file should be rejected before consuming memory
    let file_content = "a".repeat(150);

    let mut req = post_policy_test_helpers::build_post_object_request(policy_json, &file_content, &secret_key, false);

    // This should fail because file size (150 bytes) exceeds policy limit (100 bytes)
    // The key security improvement: file is rejected during aggregation (at 100 bytes limit),
    // not after reading the full 150 bytes (or potentially larger files)
    let result = super::prepare(&mut req, &ccx).await;
    assert!(result.is_err(), "expected error for file exceeding policy limit");

    // MultipartError::FileTooLarge is mapped to EntityTooLarge
    match result {
        Err(err) => {
            let code = err.code();
            assert!(
                matches!(code, crate::error::S3ErrorCode::EntityTooLarge),
                "expected EntityTooLarge error, got {code:?}",
            );
        }
        Ok(_) => panic!("expected error for file exceeding policy limit"),
    }
}

/// Test that policy max > config max results in using config max for file size limit
#[tokio::test]
async fn post_object_policy_max_larger_than_config_max() {
    use crate::auth::SecretKey;
    use std::sync::Arc;
    use std::sync::atomic::AtomicUsize;

    let test_s3 = Arc::new(post_policy_test_helpers::TestS3WithPostTracking {
        post_calls: AtomicUsize::new(0),
    });
    let s3: Arc<dyn crate::s3_trait::S3> = test_s3.clone();

    // Set config max to 200 bytes (smaller than policy max)
    let config = post_policy_test_helpers::create_test_config(200);

    let auth = post_policy_test_helpers::create_test_auth();
    let ccx = post_policy_test_helpers::create_test_context(&s3, &config, &auth);

    let secret_key: SecretKey = "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY".into();

    // Create a policy with content-length-range max of 10KB (> config max of 200 bytes)
    let policy_json = &format!(
        r#"{{"expiration":"2030-01-01T00:00:00.000Z","conditions":[["content-length-range",0,10240],["eq","$Content-Type","text/plain"],{}]}}"#,
        post_policy_test_helpers::BASE_CONDITIONS,
    );
    // Create a file with 150 bytes (within config max of 200 bytes, within policy max of 10KB)
    let file_content = "a".repeat(150);

    let mut req = post_policy_test_helpers::build_post_object_request(policy_json, &file_content, &secret_key, true);

    // This should succeed because file size (150 bytes) is within config max (200 bytes)
    // The aggregation limit used is min(policy_max=10KB, config_max=200) = 200 bytes
    let result = super::prepare(&mut req, &ccx).await;
    assert!(result.is_ok(), "expected success for file within config limit");
}

/// Regression test for rustfs/rustfs#984:
/// POST Object with content-length-range [0, 10] should reject files larger than 10 bytes
/// with `EntityTooLarge` error code.
#[tokio::test]
async fn post_object_content_length_range_rejects_oversized_file() {
    use crate::auth::SecretKey;
    use std::sync::Arc;

    let s3: Arc<dyn crate::s3_trait::S3> = Arc::new(post_policy_test_helpers::TestS3NoOp);

    let config = post_policy_test_helpers::create_test_config(5 * 1024 * 1024 * 1024);

    let auth = post_policy_test_helpers::create_test_auth();
    let ccx = post_policy_test_helpers::create_test_context(&s3, &config, &auth);

    let secret_key: SecretKey = "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY".into();

    // Exact scenario from the issue: content-length-range [0, 10]
    let policy_json = &format!(
        r#"{{"expiration":"2030-01-01T00:00:00.000Z","conditions":[["content-length-range",0,10],{}]}}"#,
        post_policy_test_helpers::BASE_CONDITIONS,
    );
    // File content is much larger than 10 bytes
    let file_content = "very long contents, longer than 10 bytes";

    let mut req = post_policy_test_helpers::build_post_object_request(policy_json, file_content, &secret_key, false);

    let result = super::prepare(&mut req, &ccx).await;
    assert!(result.is_err(), "expected error for file exceeding content-length-range");

    let Err(err) = result else {
        panic!("expected error for file exceeding content-length-range");
    };
    assert_eq!(
        *err.code(),
        crate::error::S3ErrorCode::EntityTooLarge,
        "expected EntityTooLarge error, got {:?}",
        err.code()
    );
}

/// POST Object with `Content-Length` forwards the file as a stream with an
/// exact known length, without aggregating the file into memory.
#[tokio::test]
async fn post_object_with_content_length_streams_file() {
    use crate::auth::SecretKey;
    use futures::StreamExt;
    use std::sync::Arc;

    let s3: Arc<dyn crate::s3_trait::S3> = Arc::new(post_policy_test_helpers::TestS3NoOp);

    let config = post_policy_test_helpers::create_test_config(1024 * 1024);
    let auth = post_policy_test_helpers::create_test_auth();
    let ccx = post_policy_test_helpers::create_test_context(&s3, &config, &auth);

    let secret_key: SecretKey = "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY".into();
    let policy_json = &format!(
        r#"{{"expiration":"2030-01-01T00:00:00.000Z","conditions":[{}]}}"#,
        post_policy_test_helpers::BASE_CONDITIONS,
    );
    let file_content = "file content for streaming";
    let mut req = post_policy_test_helpers::build_post_object_request(policy_json, file_content, &secret_key, false);

    let result = super::prepare(&mut req, &ccx).await;
    assert!(result.is_ok(), "expected prepare to succeed");

    let mut stream = req
        .s3ext
        .post_object_stream
        .take()
        .expect("post object stream should be present");
    assert_eq!(
        stream.remaining_length().exact(),
        Some(file_content.len()),
        "the stream must report the exact file length"
    );

    // Partial consumption decrements the reported remaining length.
    let first = stream.next().await.unwrap().expect("stream should not error");
    assert_eq!(
        stream.remaining_length().exact(),
        Some(file_content.len() - first.len()),
        "remaining length must track consumption"
    );

    let mut collected = first.to_vec();
    while let Some(chunk) = stream.next().await {
        collected.extend_from_slice(&chunk.expect("stream should not error"));
    }
    assert_eq!(collected, file_content.as_bytes());
}

/// An empty file part streams as a zero-length object: the derived claim is
/// zero and the stream ends cleanly without yielding anything.
#[tokio::test]
async fn post_object_empty_file_streams_zero_length() {
    use crate::auth::SecretKey;
    use futures::StreamExt;
    use std::sync::Arc;

    let s3: Arc<dyn crate::s3_trait::S3> = Arc::new(post_policy_test_helpers::TestS3NoOp);

    let config = post_policy_test_helpers::create_test_config(1024 * 1024);
    let auth = post_policy_test_helpers::create_test_auth();
    let ccx = post_policy_test_helpers::create_test_context(&s3, &config, &auth);

    let secret_key: SecretKey = "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY".into();
    let policy_json = &format!(
        r#"{{"expiration":"2030-01-01T00:00:00.000Z","conditions":[{}]}}"#,
        post_policy_test_helpers::BASE_CONDITIONS,
    );
    let mut req = post_policy_test_helpers::build_post_object_request(policy_json, "", &secret_key, false);

    let result = super::prepare(&mut req, &ccx).await;
    assert!(result.is_ok(), "expected prepare to succeed");

    let mut stream = req
        .s3ext
        .post_object_stream
        .take()
        .expect("post object stream should be present");
    assert_eq!(stream.remaining_length().exact(), Some(0));

    while let Some(chunk) = stream.next().await {
        let bytes = chunk.expect("stream should not error");
        assert!(bytes.is_empty(), "zero-length file must yield no content");
    }
}

/// File content containing CRLF runs and a near-miss boundary prefix (one
/// character short of the real boundary) is delivered byte-exactly under the
/// strict streaming path instead of being cut at the near miss.
#[tokio::test]
async fn post_object_content_with_near_miss_boundary_streams_exactly() {
    use crate::auth::SecretKey;
    use futures::StreamExt;
    use std::sync::Arc;

    let s3: Arc<dyn crate::s3_trait::S3> = Arc::new(post_policy_test_helpers::TestS3NoOp);

    let config = post_policy_test_helpers::create_test_config(1024 * 1024);
    let auth = post_policy_test_helpers::create_test_auth();
    let ccx = post_policy_test_helpers::create_test_context(&s3, &config, &auth);

    let secret_key: SecretKey = "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY".into();
    let policy_json = &format!(
        r#"{{"expiration":"2030-01-01T00:00:00.000Z","conditions":[{}]}}"#,
        post_policy_test_helpers::BASE_CONDITIONS,
    );
    // The helper's boundary is "------------------------test12345678"; the
    // content embeds CRLFs and a truncated copy of it (missing the final 8).
    let file_content = "alpha\r\nbeta\r\n\r\n--------------------------test1234567X tail\r\nmore content\r\n";
    let mut req = post_policy_test_helpers::build_post_object_request(policy_json, file_content, &secret_key, false);

    let result = super::prepare(&mut req, &ccx).await;
    assert!(result.is_ok(), "expected prepare to succeed");

    let mut stream = req
        .s3ext
        .post_object_stream
        .take()
        .expect("post object stream should be present");
    assert_eq!(stream.remaining_length().exact(), Some(file_content.len()));

    let mut collected = Vec::new();
    while let Some(chunk) = stream.next().await {
        collected.extend_from_slice(&chunk.expect("stream should not error"));
    }
    assert_eq!(collected, file_content.as_bytes());
}

/// POST Object without `Content-Length` (chunked body) falls back to
/// aggregating the file; the operation still receives an exact length.
#[tokio::test]
async fn post_object_chunked_aggregates_file() {
    use crate::auth::SecretKey;
    use futures::StreamExt;
    use std::sync::Arc;

    let s3: Arc<dyn crate::s3_trait::S3> = Arc::new(post_policy_test_helpers::TestS3NoOp);

    let config = post_policy_test_helpers::create_test_config(1024 * 1024);
    let auth = post_policy_test_helpers::create_test_auth();
    let ccx = post_policy_test_helpers::create_test_context(&s3, &config, &auth);

    let secret_key: SecretKey = "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY".into();
    let policy_json = &format!(
        r#"{{"expiration":"2030-01-01T00:00:00.000Z","conditions":[{}]}}"#,
        post_policy_test_helpers::BASE_CONDITIONS,
    );
    let file_content = "file content for chunked upload";
    let mut req = post_policy_test_helpers::build_post_object_request_chunked(policy_json, file_content, &secret_key, 1024);

    let result = super::prepare(&mut req, &ccx).await;
    if let Err(err) = &result {
        panic!("expected prepare to succeed, got {err:?}");
    }

    let mut stream = req
        .s3ext
        .post_object_stream
        .take()
        .expect("post object stream should be present");
    assert_eq!(stream.remaining_length().exact(), Some(file_content.len()),);

    let mut collected = Vec::new();
    while let Some(chunk) = stream.next().await {
        collected.extend_from_slice(&chunk.expect("stream should not error"));
    }
    assert_eq!(collected, file_content.as_bytes());
}

/// A chunked POST Object whose file exceeds the configured maximum must be
/// rejected with `EntityTooLarge` while aggregating on the fallback path.
#[tokio::test]
async fn post_object_chunked_rejects_oversized_file() {
    use crate::auth::SecretKey;
    use std::sync::Arc;

    let s3: Arc<dyn crate::s3_trait::S3> = Arc::new(post_policy_test_helpers::TestS3NoOp);

    let config = post_policy_test_helpers::create_test_config(1024 * 1024);
    let auth = post_policy_test_helpers::create_test_auth();
    let ccx = post_policy_test_helpers::create_test_context(&s3, &config, &auth);

    let secret_key: SecretKey = "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY".into();
    let policy_json = &format!(
        r#"{{"expiration":"2030-01-01T00:00:00.000Z","conditions":[{}]}}"#,
        post_policy_test_helpers::BASE_CONDITIONS,
    );
    // No content-length-range condition in the policy: the config maximum
    // (1 MiB) applies. The file is twice that size.
    let file_content = "x".repeat(2 * 1024 * 1024);

    let mut req = post_policy_test_helpers::build_post_object_request_chunked(policy_json, &file_content, &secret_key, 1024);

    let result = super::prepare(&mut req, &ccx).await;
    let Err(err) = result else {
        panic!("expected prepare to fail for an oversized chunked upload");
    };
    assert_eq!(
        *err.code(),
        crate::error::S3ErrorCode::EntityTooLarge,
        "expected EntityTooLarge error, got {:?}",
        err.code()
    );
}

/// A chunked POST Object whose body stream errors mid-file must be rejected
/// with `InvalidRequest` while aggregating on the fallback path.
#[tokio::test]
async fn post_object_chunked_rejects_broken_body() {
    use crate::auth::SecretKey;
    use std::sync::Arc;

    let s3: Arc<dyn crate::s3_trait::S3> = Arc::new(post_policy_test_helpers::TestS3NoOp);

    let config = post_policy_test_helpers::create_test_config(1024 * 1024);
    let auth = post_policy_test_helpers::create_test_auth();
    let ccx = post_policy_test_helpers::create_test_context(&s3, &config, &auth);

    let secret_key: SecretKey = "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY".into();
    let policy_json = &format!(
        r#"{{"expiration":"2030-01-01T00:00:00.000Z","conditions":[{}]}}"#,
        post_policy_test_helpers::BASE_CONDITIONS,
    );

    // Mirror the helper's request construction, but end the body stream with
    // an error right after the file part headers: aggregation hits a
    // non-FileTooLarge underlying error.
    let boundary = "------------------------test12345678";
    let bucket = "test-bucket";
    let key = "test-key";
    let amz_date = s3s_sigv4::AmzDate::parse("20250101T000000Z").unwrap();
    let amz_date_str = amz_date.fmt_iso8601();
    let credential = "AKIAIOSFODNN7EXAMPLE/20250101/us-east-1/s3/aws4_request";
    let algorithm = "AWS4-HMAC-SHA256";

    let augmented = post_policy_test_helpers::augment_post_policy_for_test(policy_json, &amz_date_str, credential, algorithm);
    let policy_b64 = base64_simd::STANDARD.encode_to_string(&augmented);
    let signature = s3s_sigv4::calculate_signature(&policy_b64, secret_key.expose(), &amz_date, "us-east-1", "s3");

    let fields = post_policy_test_helpers::build_multipart_fields(
        &[
            ("x-amz-signature", signature.as_str()),
            ("bucket", bucket),
            ("policy", policy_b64.as_str()),
            ("x-amz-algorithm", algorithm),
            ("x-amz-credential", credential),
            ("x-amz-date", amz_date_str.as_str()),
            ("key", key),
        ],
        boundary,
    );
    let file_field_header = format!(
        "\r\n--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"test.txt\"\r\nContent-Type: text/plain\r\n\r\n"
    );

    let frames: Vec<Result<http_body::Frame<Bytes>, crate::error::StdError>> = vec![
        Ok(http_body::Frame::data(Bytes::from(fields + &file_field_header))),
        Ok(http_body::Frame::data(Bytes::from_static(b"partial content"))),
        Err("boom".into()),
    ];
    let stream_body = http_body_util::StreamBody::new(futures::stream::iter(frames));

    let mut req = Request::from(
        hyper::Request::builder()
            .method(Method::POST)
            .uri(format!("http://localhost/{bucket}"))
            .header(crate::header::HOST, "localhost")
            .header(
                crate::header::CONTENT_TYPE,
                hyper::header::HeaderValue::from_str(&format!("multipart/form-data; boundary={boundary}")).unwrap(),
            )
            .body(Body::http_body_unsync(stream_body))
            .unwrap(),
    );

    let result = super::prepare(&mut req, &ccx).await;
    let Err(err) = result else {
        panic!("expected prepare to fail for a broken body stream");
    };
    assert_eq!(
        *err.code(),
        crate::error::S3ErrorCode::InvalidRequest,
        "expected InvalidRequest error, got {:?}",
        err.code()
    );
}

/// A `Content-Length` request whose multipart trailer is not canonical must
/// fail while the file stream is consumed, instead of silently truncating or
/// hanging.
#[tokio::test]
async fn post_object_with_content_length_rejects_bad_trailer() {
    use crate::auth::SecretKey;
    use futures::StreamExt;
    use std::sync::Arc;

    let s3: Arc<dyn crate::s3_trait::S3> = Arc::new(post_policy_test_helpers::TestS3NoOp);

    let config = post_policy_test_helpers::create_test_config(1024 * 1024);
    let auth = post_policy_test_helpers::create_test_auth();
    let ccx = post_policy_test_helpers::create_test_context(&s3, &config, &auth);

    let secret_key: SecretKey = "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY".into();
    let policy_json = &format!(
        r#"{{"expiration":"2030-01-01T00:00:00.000Z","conditions":[{}]}}"#,
        post_policy_test_helpers::BASE_CONDITIONS,
    );
    let mut req = post_policy_test_helpers::build_post_object_request(policy_json, "content", &secret_key, false);

    // Strip the final CRLF from the multipart body to produce a non-canonical
    // trailer (`--{boundary}--` directly followed by EOF).
    let mut full = req.body.bytes().expect("body should be buffered").to_vec();
    assert!(full.ends_with(b"--\r\n"));
    full.pop();
    full.pop();
    req.body = crate::http::Body::from(bytes::Bytes::from(full.clone()));
    req.headers
        .insert(hyper::header::CONTENT_LENGTH, hyper::header::HeaderValue::from(full.len()));

    let result = super::prepare(&mut req, &ccx).await;
    assert!(result.is_ok(), "dispatch is not affected");

    let mut stream = req
        .s3ext
        .post_object_stream
        .take()
        .expect("post object stream should be present");

    // The stream must report an error instead of yielding a truncated body.
    let mut errored = false;
    while let Some(chunk) = stream.next().await {
        match chunk {
            Ok(_) => {}
            Err(_) => errored = true,
        }
    }
    assert!(errored, "stream must reject the non-canonical trailer");
}

// ========================================
// Access Control Tests
// ========================================

// Helper module for access control tests
mod access_control_test_helpers {
    use crate::S3Request;
    use std::sync::atomic::{AtomicUsize, Ordering};

    pub struct TestS3WithGetObject {
        pub get_object_calls: AtomicUsize,
    }

    impl TestS3WithGetObject {
        pub fn new() -> Self {
            Self {
                get_object_calls: AtomicUsize::new(0),
            }
        }

        pub fn get_call_count(&self) -> usize {
            self.get_object_calls.load(Ordering::SeqCst)
        }
    }

    #[async_trait::async_trait]
    impl crate::s3_trait::S3 for TestS3WithGetObject {
        async fn get_object(
            &self,
            _req: S3Request<crate::dto::GetObjectInput>,
        ) -> crate::error::S3Result<crate::protocol::S3Response<crate::dto::GetObjectOutput>> {
            self.get_object_calls.fetch_add(1, Ordering::SeqCst);
            Ok(crate::protocol::S3Response::new(crate::dto::GetObjectOutput::default()))
        }
    }
}

/// Test S3 route denies anonymous access when auth is configured
#[tokio::test]
async fn test_s3_route_anonymous_access_denied() {
    use crate::auth::{SecretKey, SimpleAuth};
    use crate::config::{S3ConfigProvider, StaticConfigProvider};
    use crate::http::{Body, Request};
    use crate::ops::CallContext;
    use hyper::Method;
    use std::sync::Arc;

    let test_s3 = Arc::new(access_control_test_helpers::TestS3WithGetObject::new());
    let s3: Arc<dyn crate::s3_trait::S3> = test_s3.clone();
    let config: Arc<dyn S3ConfigProvider> = Arc::new(StaticConfigProvider::default());

    let access_key = "AKIAIOSFODNN7EXAMPLE";
    let secret_key: SecretKey = "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY".into();
    let auth = SimpleAuth::from_single(access_key, secret_key);

    let ccx = CallContext {
        s3: &s3,
        config: &config,
        host: None,
        auth: Some(&auth),
        access: None,
        route: None,
        validation: None,
    };

    // Create an anonymous GET object request (no auth headers or query params)
    let mut req = Request::from(
        hyper::Request::builder()
            .method(Method::GET)
            .uri("http://localhost/test-bucket/test-key.txt")
            .header(crate::header::HOST, "localhost")
            .body(Body::empty())
            .unwrap(),
    );

    // This should fail with AccessDenied because the request has no authentication.
    // Use `call` so that we exercise the full request lifecycle, and ensure that
    // access is denied before the S3 backend is invoked.
    let response = super::call(&mut req, &ccx).await.unwrap();

    // Verify that the response indicates access is denied
    assert_eq!(response.status, hyper::StatusCode::FORBIDDEN, "Anonymous request should have been denied");

    // Verify that the S3 service was never called (access was denied before dispatch)
    assert_eq!(test_s3.get_call_count(), 0);
}

/// Test S3 route with custom `S3Access` that allows anonymous access
#[tokio::test]
async fn test_s3_route_custom_access_allows_anonymous() {
    use crate::access::{S3Access, S3AccessContext};
    use crate::auth::{SecretKey, SimpleAuth};
    use crate::config::{S3ConfigProvider, StaticConfigProvider};
    use crate::http::{Body, Request};
    use crate::ops::CallContext;
    use hyper::Method;
    use std::sync::Arc;

    /// Custom `S3Access` that allows anonymous access
    struct AnonymousAccess;

    #[async_trait::async_trait]
    impl S3Access for AnonymousAccess {
        async fn check(&self, _cx: &mut S3AccessContext<'_>) -> crate::error::S3Result<()> {
            // Allow all access, including anonymous
            Ok(())
        }
    }

    let test_s3 = Arc::new(access_control_test_helpers::TestS3WithGetObject::new());
    let s3: Arc<dyn crate::s3_trait::S3> = test_s3.clone();
    let config: Arc<dyn S3ConfigProvider> = Arc::new(StaticConfigProvider::default());

    let access_key = "AKIAIOSFODNN7EXAMPLE";
    let secret_key: SecretKey = "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY".into();
    let auth = SimpleAuth::from_single(access_key, secret_key);

    let anonymous_access = AnonymousAccess;

    let ccx = CallContext {
        s3: &s3,
        config: &config,
        host: None,
        auth: Some(&auth),
        access: Some(&anonymous_access),
        route: None,
        validation: None,
    };

    // Create an anonymous GET object request
    let mut req = Request::from(
        hyper::Request::builder()
            .method(Method::GET)
            .uri("http://localhost/test-bucket/test-key.txt")
            .header(crate::header::HOST, "localhost")
            .body(Body::empty())
            .unwrap(),
    );

    // Call the full operation which should pass access control and invoke the handler
    let result = super::call(&mut req, &ccx).await;

    // Should succeed with a successful response
    match result {
        Ok(resp) => {
            // Should get a successful response (2xx status code)
            assert!(
                resp.status.is_success(),
                "Anonymous request should succeed when custom access control allows it, got status: {:?}",
                resp.status
            );
        }
        Err(err) => {
            panic!("Anonymous request should succeed when custom access control allows it, got error: {err:?}");
        }
    }

    // Verify that the S3 handler was actually invoked
    assert_eq!(test_s3.get_call_count(), 1, "S3 handler should have been invoked once");
}

/// Test custom route denies anonymous access by default
#[tokio::test]
async fn test_custom_route_anonymous_access_denied() {
    use crate::S3Request;
    use crate::auth::{SecretKey, SimpleAuth};
    use crate::config::{S3ConfigProvider, StaticConfigProvider};
    use crate::http::{Body, Request};
    use crate::ops::CallContext;
    use crate::protocol::S3Response;
    use crate::route::S3Route;
    use hyper::header::HeaderValue;
    use hyper::http::Extensions;
    use hyper::{HeaderMap, Method, StatusCode, Uri};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct TestS3;

    #[async_trait::async_trait]
    impl crate::s3_trait::S3 for TestS3 {}

    /// Custom route that uses default `check_access` (requires authentication)
    #[derive(Debug, Clone)]
    struct TestCustomRoute {
        call_count: Arc<AtomicUsize>,
    }

    impl TestCustomRoute {
        fn new() -> Self {
            Self {
                call_count: Arc::new(AtomicUsize::new(0)),
            }
        }

        fn get_call_count(&self) -> usize {
            self.call_count.load(Ordering::SeqCst)
        }
    }

    #[async_trait::async_trait]
    impl S3Route for TestCustomRoute {
        fn is_match(&self, method: &Method, uri: &Uri, headers: &HeaderMap, _: &mut Extensions) -> bool {
            // Match POST requests to /custom-route
            method == Method::POST
                && uri.path() == "/custom-route"
                && headers
                    .get(hyper::header::CONTENT_TYPE)
                    .is_some_and(|v| v.as_bytes() == b"application/x-custom")
        }

        // Use default check_access which requires authentication

        async fn call(&self, _req: S3Request<Body>) -> crate::error::S3Result<S3Response<Body>> {
            self.call_count.fetch_add(1, Ordering::SeqCst);
            Ok(S3Response::new(Body::from("Custom route response".to_string())))
        }
    }

    let s3: Arc<dyn crate::s3_trait::S3> = Arc::new(TestS3);
    let config: Arc<dyn S3ConfigProvider> = Arc::new(StaticConfigProvider::default());

    let access_key = "AKIAIOSFODNN7EXAMPLE";
    let secret_key: SecretKey = "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY".into();
    let auth = SimpleAuth::from_single(access_key, secret_key);

    let custom_route = TestCustomRoute::new();

    let ccx = CallContext {
        s3: &s3,
        config: &config,
        host: None,
        auth: Some(&auth),
        access: None,
        route: Some(&custom_route),
        validation: None,
    };

    // Create an anonymous request to the custom route
    let mut req = Request::from(
        hyper::Request::builder()
            .method(Method::POST)
            .uri("http://localhost/custom-route")
            .header(crate::header::HOST, "localhost")
            .header(hyper::header::CONTENT_TYPE, HeaderValue::from_static("application/x-custom"))
            .body(Body::empty())
            .unwrap(),
    );

    // Call the operation (which will internally call prepare then check access on the custom route)
    let result = super::call(&mut req, &ccx).await;

    // call() serializes S3Errors into HTTP responses, so we check the status code
    match result {
        Ok(resp) => {
            // AccessDenied should result in a 403 Forbidden response
            assert_eq!(
                resp.status,
                StatusCode::FORBIDDEN,
                "Anonymous request to custom route should return 403 Forbidden"
            );
        }
        Err(err) => {
            // Shouldn't get here for normal S3 errors
            panic!("Unexpected error that wasn't serialized: {err:?}");
        }
    }

    // Verify that the custom route was never actually called (because access was denied)
    assert_eq!(custom_route.get_call_count(), 0);
}

/// Test custom route that overrides `check_access` to allow anonymous access
#[tokio::test]
async fn test_custom_route_anonymous_access_allowed_when_overridden() {
    use crate::S3Request;
    use crate::auth::{SecretKey, SimpleAuth};
    use crate::config::{S3ConfigProvider, StaticConfigProvider};
    use crate::http::{Body, Request};
    use crate::ops::CallContext;
    use crate::protocol::S3Response;
    use crate::route::S3Route;
    use hyper::header::HeaderValue;
    use hyper::http::Extensions;
    use hyper::{HeaderMap, Method, Uri};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct TestS3;

    #[async_trait::async_trait]
    impl crate::s3_trait::S3 for TestS3 {}

    /// Custom route that allows anonymous access
    #[derive(Debug, Clone)]
    struct AnonymousCustomRoute {
        call_count: Arc<AtomicUsize>,
    }

    impl AnonymousCustomRoute {
        fn new() -> Self {
            Self {
                call_count: Arc::new(AtomicUsize::new(0)),
            }
        }

        fn get_call_count(&self) -> usize {
            self.call_count.load(Ordering::SeqCst)
        }
    }

    #[async_trait::async_trait]
    impl S3Route for AnonymousCustomRoute {
        fn is_match(&self, method: &Method, uri: &Uri, headers: &HeaderMap, _: &mut Extensions) -> bool {
            // Match GET requests to /public-route
            method == Method::GET
                && uri.path() == "/public-route"
                && headers
                    .get(hyper::header::CONTENT_TYPE)
                    .is_some_and(|v| v.as_bytes() == b"application/x-public")
        }

        async fn check_access(&self, _req: &mut S3Request<Body>) -> crate::error::S3Result<()> {
            // Allow anonymous access
            Ok(())
        }

        async fn call(&self, _req: S3Request<Body>) -> crate::error::S3Result<S3Response<Body>> {
            self.call_count.fetch_add(1, Ordering::SeqCst);
            Ok(S3Response::new(Body::from("Public route response".to_string())))
        }
    }

    let s3: Arc<dyn crate::s3_trait::S3> = Arc::new(TestS3);
    let config: Arc<dyn S3ConfigProvider> = Arc::new(StaticConfigProvider::default());

    let access_key = "AKIAIOSFODNN7EXAMPLE";
    let secret_key: SecretKey = "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY".into();
    let auth = SimpleAuth::from_single(access_key, secret_key);

    // Use a custom route that allows anonymous access
    let anonymous_route = AnonymousCustomRoute::new();

    let ccx = CallContext {
        s3: &s3,
        config: &config,
        host: None,
        auth: Some(&auth),
        access: None,
        route: Some(&anonymous_route),
        validation: None,
    };

    // Create an anonymous request to the public route
    let mut req = Request::from(
        hyper::Request::builder()
            .method(Method::GET)
            .uri("http://localhost/public-route")
            .header(crate::header::HOST, "localhost")
            .header(hyper::header::CONTENT_TYPE, HeaderValue::from_static("application/x-public"))
            .body(Body::empty())
            .unwrap(),
    );

    // Call the operation (which will internally call prepare then check access on the custom route)
    let result = super::call(&mut req, &ccx).await;

    // This should succeed because the custom route allows anonymous access
    match result {
        Ok(resp) => {
            // Should get a successful response (2xx status code)
            assert!(
                resp.status.is_success(),
                "Anonymous request should be allowed when custom route permits it, got status: {:?}",
                resp.status
            );
        }
        Err(err) => {
            panic!("Anonymous request should succeed on public route, got error: {err:?}");
        }
    }

    // Verify that the custom route was actually called
    assert_eq!(anonymous_route.get_call_count(), 1);
}

mod custom_route_body_limit_tests {
    use super::*;

    use crate::config::{S3Config, S3ConfigProvider, StaticConfigProvider};
    use crate::http::{Body, Request};
    use crate::protocol::S3Response;
    use crate::route::S3Route;
    use bytes::Bytes;
    use http_body_util::BodyExt as _;
    use hyper::http::Extensions;
    use hyper::{HeaderMap, Method, StatusCode, Uri};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    struct TestS3;

    #[async_trait::async_trait]
    impl crate::s3_trait::S3 for TestS3 {}

    #[derive(Debug, Clone, Default)]
    struct ReadBodyRoute {
        call_count: Arc<AtomicUsize>,
        body_seen: Arc<Mutex<Option<Bytes>>>,
    }

    impl ReadBodyRoute {
        fn call_count(&self) -> usize {
            self.call_count.load(Ordering::SeqCst)
        }

        fn body_seen(&self) -> Option<Bytes> {
            self.body_seen.lock().unwrap().clone()
        }
    }

    #[async_trait::async_trait]
    impl S3Route for ReadBodyRoute {
        fn is_match(&self, method: &Method, uri: &Uri, _headers: &HeaderMap, _extensions: &mut Extensions) -> bool {
            method == Method::POST && uri.path() == "/custom-route"
        }

        async fn check_access(&self, _req: &mut crate::S3Request<Body>) -> crate::error::S3Result {
            Ok(())
        }

        async fn call(&self, req: crate::S3Request<Body>) -> crate::error::S3Result<S3Response<Body>> {
            self.call_count.fetch_add(1, Ordering::SeqCst);
            let bytes = req
                .input
                .collect()
                .await
                .map_err(|err| crate::error::S3Error::with_source(crate::error::S3ErrorCode::EntityTooLarge, err))?
                .to_bytes();
            *self.body_seen.lock().unwrap() = Some(bytes.clone());
            Ok(S3Response::new(Body::from(bytes)))
        }
    }

    fn config(custom_route_max_body_size: Option<u64>) -> Arc<dyn S3ConfigProvider> {
        Arc::new(StaticConfigProvider::new(Arc::new(S3Config {
            custom_route_max_body_size,
            ..Default::default()
        })))
    }

    fn context<'a>(
        s3: &'a Arc<dyn crate::s3_trait::S3>,
        config: &'a Arc<dyn S3ConfigProvider>,
        route: &'a dyn S3Route,
    ) -> CallContext<'a> {
        CallContext {
            s3,
            config,
            host: None,
            auth: None,
            access: None,
            route: Some(route),
            validation: None,
        }
    }

    fn custom_route_request(body: Body, content_length: Option<usize>) -> Request {
        let mut builder = hyper::Request::builder()
            .method(Method::POST)
            .uri("http://localhost/custom-route")
            .header(crate::header::HOST, "localhost");
        if let Some(content_length) = content_length {
            builder = builder.header(hyper::header::CONTENT_LENGTH, content_length);
        }
        Request::from(builder.body(body).unwrap())
    }

    fn response_body(resp: &Response) -> String {
        let bytes = resp.body.bytes().expect("response body should be buffered");
        String::from_utf8(bytes.to_vec()).expect("response body should be UTF-8")
    }

    #[tokio::test]
    async fn custom_route_body_under_limit_passes_through() {
        let s3: Arc<dyn crate::s3_trait::S3> = Arc::new(TestS3);
        let config = config(Some(16));
        let route = ReadBodyRoute::default();
        let ccx = context(&s3, &config, &route);
        let body = Bytes::from_static(b"hello");
        let mut req = custom_route_request(Body::from(body.clone()), Some(body.len()));

        let resp = super::call(&mut req, &ccx).await.expect("custom route call should succeed");

        assert_eq!(resp.status, StatusCode::OK);
        assert_eq!(resp.body.bytes().expect("response body should be buffered"), body);
        assert_eq!(route.call_count(), 1);
        assert_eq!(route.body_seen(), Some(body));
    }

    #[tokio::test]
    async fn custom_route_content_length_over_limit_is_rejected_before_dispatch() {
        let s3: Arc<dyn crate::s3_trait::S3> = Arc::new(TestS3);
        let config = config(Some(4));
        let route = ReadBodyRoute::default();
        let ccx = context(&s3, &config, &route);
        let mut req = custom_route_request(Body::from(Bytes::from_static(b"hello")), Some(5));

        let resp = super::call(&mut req, &ccx)
            .await
            .expect("oversized route request should serialize error");

        assert_eq!(resp.status, StatusCode::BAD_REQUEST);
        assert!(response_body(&resp).contains("<Code>EntityTooLarge</Code>"));
        assert_eq!(route.call_count(), 0);
    }

    #[tokio::test]
    async fn custom_route_streaming_body_over_limit_fails_when_read() {
        let s3: Arc<dyn crate::s3_trait::S3> = Arc::new(TestS3);
        let config = config(Some(4));
        let route = ReadBodyRoute::default();
        let ccx = context(&s3, &config, &route);
        let mut req = custom_route_request(Body::from(Bytes::from_static(b"hello")), None);

        let resp = super::call(&mut req, &ccx)
            .await
            .expect("oversized route request should serialize error");

        assert_eq!(resp.status, StatusCode::BAD_REQUEST);
        assert!(response_body(&resp).contains("<Code>EntityTooLarge</Code>"));
        assert_eq!(route.call_count(), 1);
        assert_eq!(route.body_seen(), None);
    }

    #[tokio::test]
    async fn custom_route_body_limit_disabled_when_none() {
        let s3: Arc<dyn crate::s3_trait::S3> = Arc::new(TestS3);
        let config = config(None);
        let route = ReadBodyRoute::default();
        let ccx = context(&s3, &config, &route);
        let body = Bytes::from_static(b"hello world");
        let mut req = custom_route_request(Body::from(body.clone()), Some(body.len()));

        let resp = super::call(&mut req, &ccx).await.expect("disabled limit should pass through");

        assert_eq!(resp.status, StatusCode::OK);
        assert_eq!(resp.body.bytes().expect("response body should be buffered"), body);
        assert_eq!(route.call_count(), 1);
        assert_eq!(route.body_seen(), Some(body));
    }

    #[tokio::test]
    async fn custom_route_limit_is_configurable_above_default() {
        let s3: Arc<dyn crate::s3_trait::S3> = Arc::new(TestS3);
        let body = Bytes::from(vec![b'a'; 1024 * 1024 + 1]);
        let config = config(Some(u64::try_from(body.len()).unwrap()));
        let route = ReadBodyRoute::default();
        let ccx = context(&s3, &config, &route);
        let mut req = custom_route_request(Body::from(body.clone()), Some(body.len()));

        let resp = super::call(&mut req, &ccx)
            .await
            .expect("configured route body limit should allow request");

        assert_eq!(resp.status, StatusCode::OK);
        assert_eq!(resp.body.bytes().expect("response body should be buffered"), body);
        assert_eq!(route.call_count(), 1);
    }

    #[tokio::test]
    async fn xml_operation_uses_xml_limit_not_custom_route_limit() {
        let s3: Arc<dyn crate::s3_trait::S3> = Arc::new(TestS3);
        let config: Arc<dyn S3ConfigProvider> = Arc::new(StaticConfigProvider::new(Arc::new(S3Config {
            custom_route_max_body_size: Some(1),
            xml_max_body_size: 1024,
            ..Default::default()
        })));
        let ccx = CallContext {
            s3: &s3,
            config: &config,
            host: None,
            auth: None,
            access: None,
            route: None,
            validation: None,
        };
        let body = Bytes::from_static(b"<Delete><Object><Key>k</Key></Object></Delete>");
        let mut req = Request::from(
            hyper::Request::builder()
                .method(Method::POST)
                .uri("http://localhost/test-bucket?delete")
                .header(crate::header::HOST, "localhost")
                .header(hyper::header::CONTENT_LENGTH, body.len())
                .body(Body::from(body.clone()))
                .unwrap(),
        );

        let result = super::prepare(&mut req, &ccx).await;

        match result.expect("DeleteObjects prepare should ignore custom route body limit") {
            Prepare::S3(op) => assert_eq!(op.name(), "DeleteObjects"),
            Prepare::CustomRoute => panic!("S3 operation should not dispatch to a custom route"),
        }
        assert_eq!(req.body.bytes().expect("XML body should remain buffered"), body);
    }

    #[tokio::test]
    async fn post_object_uses_post_limit_not_custom_route_limit() {
        use crate::auth::SecretKey;

        let s3: Arc<dyn crate::s3_trait::S3> = Arc::new(post_policy_test_helpers::TestS3NoOp);
        let config: Arc<dyn S3ConfigProvider> = Arc::new(StaticConfigProvider::new(Arc::new(S3Config {
            custom_route_max_body_size: Some(1),
            post_object_max_file_size: 1024,
            presigned_url_max_skew_time_secs: u32::MAX,
            expected_region: Some("us-east-1".parse().expect("valid test region")),
            ..Default::default()
        })));
        let auth = post_policy_test_helpers::create_test_auth();
        let ccx = post_policy_test_helpers::create_test_context(&s3, &config, &auth);
        let secret_key: SecretKey = "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY".into();
        let policy_json = &format!(
            r#"{{"expiration":"2030-01-01T00:00:00.000Z","conditions":[{}]}}"#,
            post_policy_test_helpers::BASE_CONDITIONS,
        );
        let mut req = post_policy_test_helpers::build_post_object_request(policy_json, "hello", &secret_key, false);

        let result = super::prepare(&mut req, &ccx).await;

        match result.expect("POST Object prepare should ignore custom route body limit") {
            Prepare::S3(op) => assert_eq!(op.name(), "PostObject"),
            Prepare::CustomRoute => panic!("POST Object should not dispatch to a custom route"),
        }
    }
}

/// Test S3 route allows access when no auth provider is configured
///
/// When `CallContext.auth` is `None`, access checks are skipped for S3 operations,
/// allowing unsigned requests to succeed. This tests that behavior.
#[tokio::test]
async fn test_s3_route_no_auth_provider_allows_unsigned_requests() {
    use crate::config::{S3ConfigProvider, StaticConfigProvider};
    use crate::http::{Body, Request};
    use crate::ops::CallContext;
    use hyper::Method;
    use std::sync::Arc;

    let test_s3 = Arc::new(access_control_test_helpers::TestS3WithGetObject::new());
    let s3: Arc<dyn crate::s3_trait::S3> = test_s3.clone();
    let config: Arc<dyn S3ConfigProvider> = Arc::new(StaticConfigProvider::default());

    // No auth provider configured - access checks are skipped for S3 operations
    let ccx = CallContext {
        s3: &s3,
        config: &config,
        host: None,
        auth: None,
        access: None,
        route: None,
        validation: None,
    };

    // Create an unsigned request
    let mut req = Request::from(
        hyper::Request::builder()
            .method(Method::GET)
            .uri("http://localhost/test-bucket/test-key.txt")
            .header(crate::header::HOST, "localhost")
            .body(Body::empty())
            .unwrap(),
    );

    // Call the full operation which should succeed when no auth provider is configured
    let result = super::call(&mut req, &ccx).await;

    // Should succeed with a successful response
    match result {
        Ok(resp) => {
            assert!(
                resp.status.is_success(),
                "Unsigned request should succeed when no auth provider is configured, got status: {:?}",
                resp.status
            );
        }
        Err(err) => {
            panic!("Unsigned request should succeed when no auth provider is configured, got error: {err:?}");
        }
    }

    // Verify that the S3 handler was invoked
    assert_eq!(test_s3.get_call_count(), 1, "S3 handler should have been invoked");
}

/// Test custom route with overridden `check_access` allows unsigned requests
///
/// Custom routes always call `check_access()`, even when no auth provider is configured.
/// This test verifies that a custom route can override `check_access` to allow access
/// without credentials, regardless of the auth provider configuration.
#[tokio::test]
async fn test_custom_route_override_check_access_allows_unsigned_requests() {
    use crate::S3Request;
    use crate::config::{S3ConfigProvider, StaticConfigProvider};
    use crate::http::{Body, Request};
    use crate::ops::CallContext;
    use crate::protocol::S3Response;
    use crate::route::S3Route;
    use hyper::header::HeaderValue;
    use hyper::http::Extensions;
    use hyper::{HeaderMap, Method, Uri};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct TestS3;

    #[async_trait::async_trait]
    impl crate::s3_trait::S3 for TestS3 {}

    /// Custom route for testing
    #[derive(Debug, Clone)]
    struct TestRoute {
        call_count: Arc<AtomicUsize>,
    }

    impl TestRoute {
        fn new() -> Self {
            Self {
                call_count: Arc::new(AtomicUsize::new(0)),
            }
        }

        fn get_call_count(&self) -> usize {
            self.call_count.load(Ordering::SeqCst)
        }
    }

    #[async_trait::async_trait]
    impl S3Route for TestRoute {
        fn is_match(&self, method: &Method, uri: &Uri, headers: &HeaderMap, _: &mut Extensions) -> bool {
            method == Method::POST
                && uri.path() == "/test-route"
                && headers
                    .get(hyper::header::CONTENT_TYPE)
                    .is_some_and(|v| v.as_bytes() == b"application/x-test")
        }

        // Override check_access to allow access without credentials
        async fn check_access(&self, _req: &mut S3Request<Body>) -> crate::error::S3Result<()> {
            Ok(())
        }

        async fn call(&self, _req: S3Request<Body>) -> crate::error::S3Result<S3Response<Body>> {
            self.call_count.fetch_add(1, Ordering::SeqCst);
            Ok(S3Response::new(Body::from("Test route response".to_string())))
        }
    }

    let s3: Arc<dyn crate::s3_trait::S3> = Arc::new(TestS3);
    let config: Arc<dyn S3ConfigProvider> = Arc::new(StaticConfigProvider::default());

    let test_route = TestRoute::new();

    // Custom route's check_access is always called, even without an auth provider
    let ccx = CallContext {
        s3: &s3,
        config: &config,
        host: None,
        auth: None,
        access: None,
        route: Some(&test_route),
        validation: None,
    };

    // Create an unsigned request to the custom route
    let mut req = Request::from(
        hyper::Request::builder()
            .method(Method::POST)
            .uri("http://localhost/test-route")
            .header(crate::header::HOST, "localhost")
            .header(hyper::header::CONTENT_TYPE, HeaderValue::from_static("application/x-test"))
            .body(Body::empty())
            .unwrap(),
    );

    // Call the operation which should succeed because check_access is overridden to allow it
    let result = super::call(&mut req, &ccx).await;

    // Should succeed with a successful response
    match result {
        Ok(resp) => {
            assert!(
                resp.status.is_success(),
                "Unsigned request should succeed with overridden check_access, got status: {:?}",
                resp.status
            );
        }
        Err(err) => {
            panic!("Unsigned request should succeed with overridden check_access, got error: {err:?}");
        }
    }

    // Verify that the custom route was invoked
    assert_eq!(test_route.get_call_count(), 1, "Custom route should have been invoked");
}

/// Test custom route with default `check_access` denies unsigned requests even without auth provider
///
/// This test verifies a key difference between S3 operations and custom routes:
/// - S3 operations skip access checks when `CallContext.auth` is `None`
/// - Custom routes always call `check_access()`, and the default implementation denies
///   requests without credentials, even when no auth provider is configured
#[tokio::test]
async fn test_custom_route_default_check_access_denies_unsigned_without_auth_provider() {
    use crate::S3Request;
    use crate::config::{S3ConfigProvider, StaticConfigProvider};
    use crate::http::{Body, Request};
    use crate::ops::CallContext;
    use crate::protocol::S3Response;
    use crate::route::S3Route;
    use hyper::header::HeaderValue;
    use hyper::http::Extensions;
    use hyper::{HeaderMap, Method, StatusCode, Uri};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct TestS3;

    #[async_trait::async_trait]
    impl crate::s3_trait::S3 for TestS3 {}

    /// Custom route that uses default `check_access` (requires credentials)
    #[derive(Debug, Clone)]
    struct TestRoute {
        call_count: Arc<AtomicUsize>,
    }

    impl TestRoute {
        fn new() -> Self {
            Self {
                call_count: Arc::new(AtomicUsize::new(0)),
            }
        }

        fn get_call_count(&self) -> usize {
            self.call_count.load(Ordering::SeqCst)
        }
    }

    #[async_trait::async_trait]
    impl S3Route for TestRoute {
        fn is_match(&self, method: &Method, uri: &Uri, headers: &HeaderMap, _: &mut Extensions) -> bool {
            method == Method::POST
                && uri.path() == "/auth-route"
                && headers
                    .get(hyper::header::CONTENT_TYPE)
                    .is_some_and(|v| v.as_bytes() == b"application/x-auth")
        }

        // Use default check_access (requires credentials)

        async fn call(&self, _req: S3Request<Body>) -> crate::error::S3Result<S3Response<Body>> {
            self.call_count.fetch_add(1, Ordering::SeqCst);
            Ok(S3Response::new(Body::from("Auth route response".to_string())))
        }
    }

    let s3: Arc<dyn crate::s3_trait::S3> = Arc::new(TestS3);
    let config: Arc<dyn S3ConfigProvider> = Arc::new(StaticConfigProvider::default());

    let test_route = TestRoute::new();

    // No auth provider configured - but custom routes still check access
    let ccx = CallContext {
        s3: &s3,
        config: &config,
        host: None,
        auth: None,
        access: None,
        route: Some(&test_route),
        validation: None,
    };

    // Create an unsigned request to the custom route
    let mut req = Request::from(
        hyper::Request::builder()
            .method(Method::POST)
            .uri("http://localhost/auth-route")
            .header(crate::header::HOST, "localhost")
            .header(hyper::header::CONTENT_TYPE, HeaderValue::from_static("application/x-auth"))
            .body(Body::empty())
            .unwrap(),
    );

    // Call the operation - should fail because default check_access requires credentials
    let result = super::call(&mut req, &ccx).await;

    // Should return 403 Forbidden
    match result {
        Ok(resp) => {
            assert_eq!(
                resp.status,
                StatusCode::FORBIDDEN,
                "Unsigned request should be denied by default check_access, got status: {:?}",
                resp.status
            );
        }
        Err(err) => {
            panic!("Expected 403 response, got error: {err:?}");
        }
    }

    // Verify that the custom route handler was never invoked (access denied before dispatch)
    assert_eq!(test_route.get_call_count(), 0, "Custom route handler should not have been invoked");
}

/// Regression test: `file_size` for post policy validation must be the total
/// byte count of all chunks, NOT the number of chunks in `Vec<Bytes>`.
///
/// Previously the code used `vec_bytes.len()` which returns the chunk count
/// instead of summing the byte lengths of all chunks.
/// This caused `content-length-range` policy validation to use a wrong value.
///
/// This test uses `build_post_object_request_chunked` to split the HTTP body
/// into many small chunks (1 KiB each). With a 30 KB file the multipart body
/// stream yields ~30 chunks, so `aggregate_file_stream_limited` returns a
/// `Vec<Bytes>` with ~30 entries. The buggy `vec_bytes.len()` would report
/// the file size as ~30, which is below the policy minimum of 100 and would
/// cause the request to be rejected. The correct code sums the byte lengths
/// and reports 30 000, which passes the `[100, 50000]` range check.
#[tokio::test]
async fn post_policy_file_size_is_total_bytes_not_chunk_count() {
    use crate::auth::SecretKey;
    use std::sync::Arc;

    let s3: Arc<dyn crate::s3_trait::S3> = Arc::new(post_policy_test_helpers::TestS3NoOp);

    // Set config max to 1MB to allow our test file
    let config = post_policy_test_helpers::create_test_config(1024 * 1024);

    let auth = post_policy_test_helpers::create_test_auth();
    let ccx = post_policy_test_helpers::create_test_context(&s3, &config, &auth);

    let secret_key: SecretKey = "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY".into();

    // Create a policy with content-length-range [100, 50000]
    // This will accept files between 100 and 50000 bytes
    let policy_json = &format!(
        r#"{{"expiration":"2030-01-01T00:00:00.000Z","conditions":[["content-length-range",100,50000],{}]}}"#,
        post_policy_test_helpers::BASE_CONDITIONS,
    );

    // Create a 30 KB file (30 000 bytes) within policy limits.
    // Use 1 KiB chunks so the body stream yields ~30 chunks for the file part.
    // With the buggy code (vec_bytes.len()), file_size would be ~30 (chunk count),
    // which is < 100 (policy minimum) and would incorrectly fail.
    let file_content = "a".repeat(30_000);
    let chunk_size = 1024;

    let mut req =
        post_policy_test_helpers::build_post_object_request_chunked(policy_json, &file_content, &secret_key, chunk_size);

    let result = super::prepare(&mut req, &ccx).await;

    // This must succeed: the file is 30 000 bytes, within [100, 50000].
    match result {
        Ok(_) => {}
        Err(err) => panic!("POST object with 30 KB file should pass content-length-range [100, 50000] validation, got: {err:?}"),
    }

    // Now test with a file that's too small (should fail)
    let small_file_content = "a".repeat(50); // 50 bytes, less than minimum of 100
    let mut req_small =
        post_policy_test_helpers::build_post_object_request_chunked(policy_json, &small_file_content, &secret_key, chunk_size);

    let result_small = super::prepare(&mut req_small, &ccx).await;
    match result_small {
        Err(err) => {
            assert_eq!(
                *err.code(),
                crate::error::S3ErrorCode::EntityTooSmall,
                "Expected EntityTooSmall error for content-length-range violation"
            );
            let msg = err.message().unwrap_or("");
            assert!(
                msg.contains("smaller than the minimum"),
                "Error message should mention file is too small, got: {msg}"
            );
        }
        Ok(_) => panic!("POST object with 50-byte file should fail content-length-range [100, 50000] validation"),
    }
}

#[test]
fn create_session_route_resolved() {
    use crate::http::{Body, OrderedQs};
    use crate::path::S3Path;

    let req = crate::http::Request::from(
        hyper::Request::builder()
            .method(Method::GET)
            .uri("http://localhost/my-bucket?session")
            .body(Body::empty())
            .unwrap(),
    );

    let s3_path = S3Path::Bucket {
        bucket: "my-bucket".into(),
    };
    let qs = OrderedQs::parse("session").unwrap();
    let op = generated::resolve_route(&req, &s3_path, Some(&qs)).unwrap();

    assert_eq!(op.name(), "CreateSession");
    assert!(!op.needs_full_body());
}

#[test]
fn create_session_deserialize_http() {
    use crate::http::Body;
    use crate::path::S3Path;

    let mut req = crate::http::Request::from(
        hyper::Request::builder()
            .method(Method::GET)
            .uri("http://localhost/my-bucket?session")
            .header("x-amz-create-session-mode", "ReadWrite")
            .body(Body::empty())
            .unwrap(),
    );

    req.s3ext.s3_path = Some(S3Path::Bucket {
        bucket: "my-bucket".into(),
    });

    let input = generated::CreateSession::deserialize_http(&mut req).unwrap();

    assert_eq!(input.bucket, "my-bucket");
    assert_eq!(input.session_mode.as_ref().map(crate::dto::SessionMode::as_str), Some("ReadWrite"));
    assert!(input.server_side_encryption.is_none());
    assert!(input.ssekms_key_id.is_none());
    assert!(input.ssekms_encryption_context.is_none());
    assert!(input.bucket_key_enabled.is_none());
}

#[test]
fn create_session_serialize_http() {
    use crate::dto::{CreateSessionOutput, SessionCredentials, Timestamp, TimestampFormat};

    let creds = SessionCredentials {
        access_key_id: "AKIAIOSFODNN7EXAMPLE".to_owned(),
        expiration: Timestamp::parse(TimestampFormat::DateTime, "2024-01-01T00:05:00.000Z").unwrap(),
        secret_access_key: "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY".to_owned(),
        session_token: "FwoGZXIvYXdzEBYaDHqa0A".to_owned(),
    };

    let output = CreateSessionOutput {
        credentials: creds,
        ..Default::default()
    };

    let resp = generated::CreateSession::serialize_http(output).unwrap();
    assert_eq!(resp.status, hyper::StatusCode::OK);
}

#[test]
fn list_directory_buckets_route_resolved() {
    use crate::http::{Body, OrderedQs};
    use crate::path::S3Path;

    let req = crate::http::Request::from(
        hyper::Request::builder()
            .method(Method::GET)
            .uri("http://localhost/?x-id=ListDirectoryBuckets")
            .body(Body::empty())
            .unwrap(),
    );

    let s3_path = S3Path::Root;
    let qs = OrderedQs::parse("x-id=ListDirectoryBuckets").unwrap();
    let op = generated::resolve_route(&req, &s3_path, Some(&qs)).unwrap();

    assert_eq!(op.name(), "ListDirectoryBuckets");
    assert!(!op.needs_full_body());
}

#[test]
fn list_buckets_route_still_default() {
    use crate::http::{Body, OrderedQs};
    use crate::path::S3Path;

    // With x-id=ListBuckets
    let req = crate::http::Request::from(
        hyper::Request::builder()
            .method(Method::GET)
            .uri("http://localhost/?x-id=ListBuckets")
            .body(Body::empty())
            .unwrap(),
    );

    let s3_path = S3Path::Root;
    let qs = OrderedQs::parse("x-id=ListBuckets").unwrap();
    let op = generated::resolve_route(&req, &s3_path, Some(&qs)).unwrap();
    assert_eq!(op.name(), "ListBuckets");

    // Without any query string
    let req2 = crate::http::Request::from(
        hyper::Request::builder()
            .method(Method::GET)
            .uri("http://localhost/")
            .body(Body::empty())
            .unwrap(),
    );
    let op2 = generated::resolve_route(&req2, &s3_path, None).unwrap();
    assert_eq!(op2.name(), "ListBuckets");
}

#[test]
fn list_directory_buckets_deserialize_http() {
    use crate::http::{Body, OrderedQs};

    let mut req = crate::http::Request::from(
        hyper::Request::builder()
            .method(Method::GET)
            .uri("http://localhost/?continuation-token=abc123&max-directory-buckets=10")
            .body(Body::empty())
            .unwrap(),
    );

    req.s3ext.s3_path = Some(crate::path::S3Path::Root);
    req.s3ext.qs = Some(OrderedQs::parse("continuation-token=abc123&max-directory-buckets=10").unwrap());

    let input = generated::ListDirectoryBuckets::deserialize_http(&mut req).unwrap();

    assert_eq!(input.continuation_token.as_deref(), Some("abc123"));
    assert_eq!(input.max_directory_buckets, Some(10));
}

#[test]
fn list_directory_buckets_serialize_http() {
    use crate::dto::ListDirectoryBucketsOutput;

    let output = ListDirectoryBucketsOutput { ..Default::default() };

    let resp = generated::ListDirectoryBuckets::serialize_http(output).unwrap();
    assert_eq!(resp.status, hyper::StatusCode::OK);
}

mod bodyless_content_length_tests {
    use super::*;
    use crate::auth::SimpleAuth;
    use crate::config::{S3Config, S3ConfigProvider, StaticConfigProvider};
    use crate::http::{Body, OrderedQs, Request};
    use crate::protocol::S3Response;
    use bytes::Bytes;
    use hyper::header::HeaderValue;
    use hyper::{Method, StatusCode, Uri, Version};
    use std::sync::Arc;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    const ACCESS_KEY: &str = "test-access";
    const SECRET_KEY: &str = "test-secret";
    const AMZ_DATE: &str = "20260828T000000Z";
    const REGION: &str = "us-east-1";
    const SERVICE: &str = "s3";
    const EMPTY_SHA256: &str = s3s_sigv4::EMPTY_STRING_SHA256_HASH;

    #[derive(Default)]
    struct TestS3 {
        get_object: AtomicUsize,
        head_object: AtomicUsize,
        delete_object: AtomicUsize,
        copy_object: AtomicUsize,
        upload_part_copy: AtomicUsize,
        put_object: AtomicUsize,
    }

    #[async_trait::async_trait]
    impl crate::s3_trait::S3 for TestS3 {
        async fn get_object(
            &self,
            _req: crate::S3Request<crate::dto::GetObjectInput>,
        ) -> crate::error::S3Result<S3Response<crate::dto::GetObjectOutput>> {
            self.get_object.fetch_add(1, Ordering::SeqCst);
            Ok(S3Response::new(crate::dto::GetObjectOutput::default()))
        }

        async fn head_object(
            &self,
            _req: crate::S3Request<crate::dto::HeadObjectInput>,
        ) -> crate::error::S3Result<S3Response<crate::dto::HeadObjectOutput>> {
            self.head_object.fetch_add(1, Ordering::SeqCst);
            Ok(S3Response::new(crate::dto::HeadObjectOutput::default()))
        }

        async fn delete_object(
            &self,
            _req: crate::S3Request<crate::dto::DeleteObjectInput>,
        ) -> crate::error::S3Result<S3Response<crate::dto::DeleteObjectOutput>> {
            self.delete_object.fetch_add(1, Ordering::SeqCst);
            Ok(S3Response::new(crate::dto::DeleteObjectOutput::default()))
        }

        async fn copy_object(
            &self,
            _req: crate::S3Request<crate::dto::CopyObjectInput>,
        ) -> crate::error::S3Result<S3Response<crate::dto::CopyObjectOutput>> {
            self.copy_object.fetch_add(1, Ordering::SeqCst);
            Ok(S3Response::new(crate::dto::CopyObjectOutput::default()))
        }

        async fn upload_part_copy(
            &self,
            _req: crate::S3Request<crate::dto::UploadPartCopyInput>,
        ) -> crate::error::S3Result<S3Response<crate::dto::UploadPartCopyOutput>> {
            self.upload_part_copy.fetch_add(1, Ordering::SeqCst);
            Ok(S3Response::new(crate::dto::UploadPartCopyOutput::default()))
        }

        async fn put_object(
            &self,
            _req: crate::S3Request<crate::dto::PutObjectInput>,
        ) -> crate::error::S3Result<S3Response<crate::dto::PutObjectOutput>> {
            self.put_object.fetch_add(1, Ordering::SeqCst);
            Ok(S3Response::new(crate::dto::PutObjectOutput::default()))
        }
    }

    impl TestS3 {
        fn total_bodyless_calls(&self) -> usize {
            self.get_object.load(Ordering::SeqCst)
                + self.head_object.load(Ordering::SeqCst)
                + self.delete_object.load(Ordering::SeqCst)
                + self.copy_object.load(Ordering::SeqCst)
                + self.upload_part_copy.load(Ordering::SeqCst)
        }
    }

    fn empty_unknown_length_body() -> Body {
        let stream = futures::stream::empty::<Result<http_body::Frame<Bytes>, std::convert::Infallible>>();
        Body::http_body(http_body_util::StreamBody::new(stream))
    }

    fn test_config() -> Arc<dyn S3ConfigProvider> {
        Arc::new(StaticConfigProvider::new(Arc::new(S3Config {
            presigned_url_max_skew_time_secs: u32::MAX,
            expected_region: Some(REGION.parse().expect("valid test region")),
            ..Default::default()
        })))
    }

    fn test_context<'a>(
        s3: &'a Arc<dyn crate::s3_trait::S3>,
        config: &'a Arc<dyn S3ConfigProvider>,
        auth: &'a SimpleAuth,
    ) -> CallContext<'a> {
        CallContext {
            s3,
            config,
            host: None,
            auth: Some(auth),
            access: None,
            route: None,
            validation: None,
        }
    }

    fn sign_request(method: &Method, uri: &Uri, payload_sha256: &str) -> String {
        let amz_date = s3s_sigv4::AmzDate::parse(AMZ_DATE).unwrap();
        let amz_date_str = amz_date.fmt_iso8601();
        let host = uri.authority().expect("test URI has authority").as_str();
        let qs = uri.query().map(OrderedQs::parse).transpose().unwrap();
        let empty_query = &[] as &[(String, String)];
        let query = qs.as_ref().map_or(empty_query, AsRef::as_ref);
        let signed_headers = [
            ("host", host),
            ("x-amz-content-sha256", payload_sha256),
            ("x-amz-date", amz_date_str.as_str()),
        ];

        let canonical_request = s3s_sigv4::create_canonical_request(
            method.as_str(),
            uri.path(),
            query,
            signed_headers,
            s3s_sigv4::Payload::SingleChunk(payload_sha256),
        );
        let string_to_sign = s3s_sigv4::create_string_to_sign(&canonical_request, &amz_date, REGION, SERVICE);
        let signature = s3s_sigv4::calculate_signature(&string_to_sign, SECRET_KEY, &amz_date, REGION, SERVICE);

        format!(
            "AWS4-HMAC-SHA256 Credential={ACCESS_KEY}/{}/{REGION}/{SERVICE}/aws4_request, \
             SignedHeaders=host;x-amz-content-sha256;x-amz-date, Signature={}",
            amz_date.fmt_date(),
            signature.as_str()
        )
    }

    fn signed_request(method: Method, version: Version, uri: &str, extra_headers: &[(&'static str, &'static str)]) -> Request {
        let uri = uri.parse::<Uri>().unwrap();
        let authorization = sign_request(&method, &uri, EMPTY_SHA256);
        let mut builder = hyper::Request::builder()
            .method(method)
            .version(version)
            .uri(uri.clone())
            .header(crate::header::X_AMZ_CONTENT_SHA256, EMPTY_SHA256)
            .header(crate::header::X_AMZ_DATE, AMZ_DATE)
            .header(crate::header::AUTHORIZATION, authorization);

        if version == Version::HTTP_11 {
            builder = builder.header(crate::header::HOST, uri.authority().unwrap().as_str());
        }

        for &(name, value) in extra_headers {
            builder = builder.header(name, value);
        }

        Request::from(builder.body(empty_unknown_length_body()).unwrap())
    }

    struct BodylessCase {
        name: &'static str,
        method: Method,
        uri: &'static str,
        extra_headers: &'static [(&'static str, &'static str)],
    }

    impl BodylessCase {
        fn calls(&self, s3: &TestS3) -> usize {
            match self.name {
                "GetObject" => s3.get_object.load(Ordering::SeqCst),
                "HeadObject" => s3.head_object.load(Ordering::SeqCst),
                "DeleteObject" => s3.delete_object.load(Ordering::SeqCst),
                "CopyObject" => s3.copy_object.load(Ordering::SeqCst),
                "UploadPartCopy" => s3.upload_part_copy.load(Ordering::SeqCst),
                _ => unreachable!("unknown test case"),
            }
        }
    }

    #[tokio::test]
    async fn signed_bodyless_operations_accept_missing_content_length() {
        const COPY_SOURCE: &[(&str, &str)] = &[("x-amz-copy-source", "/source-bucket/source-key")];
        let cases = [
            BodylessCase {
                name: "GetObject",
                method: Method::GET,
                uri: "http://localhost/test-bucket/test-key.txt",
                extra_headers: &[],
            },
            BodylessCase {
                name: "HeadObject",
                method: Method::HEAD,
                uri: "http://localhost/test-bucket/test-key.txt",
                extra_headers: &[],
            },
            BodylessCase {
                name: "DeleteObject",
                method: Method::DELETE,
                uri: "http://localhost/test-bucket/test-key.txt",
                extra_headers: &[],
            },
            BodylessCase {
                name: "CopyObject",
                method: Method::PUT,
                uri: "http://localhost/test-bucket/test-key.txt",
                extra_headers: COPY_SOURCE,
            },
            BodylessCase {
                name: "UploadPartCopy",
                method: Method::PUT,
                uri: "http://localhost/test-bucket/test-key.txt?partNumber=1&uploadId=upload-id",
                extra_headers: COPY_SOURCE,
            },
        ];

        let test_s3 = Arc::new(TestS3::default());
        let s3: Arc<dyn crate::s3_trait::S3> = test_s3.clone();
        let config = test_config();
        let auth = SimpleAuth::from_single(ACCESS_KEY, SECRET_KEY);
        let ccx = test_context(&s3, &config, &auth);

        for version in [Version::HTTP_11, Version::HTTP_2] {
            for case in &cases {
                let before = case.calls(&test_s3);
                let mut req = signed_request(case.method.clone(), version, case.uri, case.extra_headers);
                assert!(req.headers.get(hyper::header::CONTENT_LENGTH).is_none());

                let response = super::call(&mut req, &ccx).await.unwrap();
                assert!(
                    response.status.is_success(),
                    "{} over {version:?} should succeed without Content-Length, got {:?}",
                    case.name,
                    response.status
                );
                assert_eq!(case.calls(&test_s3), before + 1, "{} handler should be invoked", case.name);
            }
        }

        assert_eq!(test_s3.total_bodyless_calls(), cases.len() * 2);
    }

    #[tokio::test]
    async fn signed_put_object_still_rejects_missing_content_length() {
        let test_s3 = Arc::new(TestS3::default());
        let s3: Arc<dyn crate::s3_trait::S3> = test_s3.clone();
        let config = test_config();
        let auth = SimpleAuth::from_single(ACCESS_KEY, SECRET_KEY);
        let ccx = test_context(&s3, &config, &auth);

        for version in [Version::HTTP_11, Version::HTTP_2] {
            let mut req = signed_request(Method::PUT, version, "http://localhost/test-bucket/test-key.txt", &[]);
            assert!(req.headers.get(hyper::header::CONTENT_LENGTH).is_none());

            let response = super::call(&mut req, &ccx).await.unwrap();
            assert_eq!(
                response.status,
                StatusCode::LENGTH_REQUIRED,
                "PutObject over {version:?} must still require Content-Length"
            );
        }

        assert_eq!(test_s3.put_object.load(Ordering::SeqCst), 0);
    }

    struct ContentLengthRecordingS3 {
        received: Mutex<Option<(Option<i64>, Option<u64>)>>, // (input.content_length, headers content-length)
    }

    #[async_trait::async_trait]
    impl crate::s3_trait::S3 for ContentLengthRecordingS3 {
        async fn put_object(
            &self,
            req: crate::S3Request<crate::dto::PutObjectInput>,
        ) -> crate::error::S3Result<S3Response<crate::dto::PutObjectOutput>> {
            let header_cl = req.headers.get(hyper::header::CONTENT_LENGTH).map(|v| {
                v.to_str()
                    .expect("test header is ASCII")
                    .parse::<u64>()
                    .expect("test header parses")
            });
            *self.received.lock().unwrap() = Some((req.input.content_length, header_cl));
            Ok(S3Response::new(crate::dto::PutObjectOutput::default()))
        }
    }

    fn empty_body_signed_put(version: Version) -> Request {
        let uri = "http://localhost/test-bucket/test-key.txt".parse::<Uri>().unwrap();
        let authorization = sign_request(&Method::PUT, &uri, EMPTY_SHA256);
        Request::from(
            hyper::Request::builder()
                .method(Method::PUT)
                .version(version)
                .uri(uri.clone())
                .header(crate::header::HOST, uri.authority().unwrap().as_str())
                .header(crate::header::X_AMZ_CONTENT_SHA256, EMPTY_SHA256)
                .header(crate::header::X_AMZ_DATE, AMZ_DATE)
                .header(crate::header::AUTHORIZATION, authorization)
                .body(Body::empty())
                .unwrap(),
        )
    }

    #[tokio::test]
    async fn backfills_known_content_length_for_zero_length_put() {
        // RFC 9112 §6.3: a PUT without `Content-Length` and without
        // `Transfer-Encoding` has an empty body. The length is known (exact
        // zero), so with the default `normalize_content_length` the `S3`
        // implementation must observe `Some(0)` and an inserted
        // `Content-Length: 0` header instead of an ambiguous `None`.
        let recording = Arc::new(ContentLengthRecordingS3 {
            received: Mutex::new(None),
        });
        let s3: Arc<dyn crate::s3_trait::S3> = recording.clone();
        let config = test_config();
        let auth = SimpleAuth::from_single(ACCESS_KEY, SECRET_KEY);
        let ccx = test_context(&s3, &config, &auth);

        for version in [Version::HTTP_11, Version::HTTP_2] {
            let mut req = empty_body_signed_put(version);
            assert!(req.headers.get(hyper::header::CONTENT_LENGTH).is_none());

            let response = super::call(&mut req, &ccx).await.unwrap();
            assert_eq!(response.status, StatusCode::OK, "empty-body PutObject over {version:?} must be accepted");

            let (input_len, header_len) = recording.received.lock().unwrap().take().expect("put_object was called");
            assert_eq!(input_len, Some(0), "dto content_length must be backfilled over {version:?}");
            assert_eq!(header_len, Some(0), "Content-Length header must be inserted over {version:?}");
        }
    }

    #[tokio::test]
    async fn decoded_content_length_takes_priority_over_exact_length() {
        // aws-chunked uploads express the object size via
        // `x-amz-decoded-content-length`; when present it must win over the
        // exact remaining length of the (empty) body.
        let recording = Arc::new(ContentLengthRecordingS3 {
            received: Mutex::new(None),
        });
        let s3: Arc<dyn crate::s3_trait::S3> = recording.clone();
        let config = test_config();
        let auth = SimpleAuth::from_single(ACCESS_KEY, SECRET_KEY);
        let ccx = test_context(&s3, &config, &auth);

        let mut req = empty_body_signed_put(Version::HTTP_11);
        req.headers
            .insert(crate::header::X_AMZ_DECODED_CONTENT_LENGTH, HeaderValue::from_static("5"));

        let response = super::call(&mut req, &ccx).await.unwrap();
        assert_eq!(response.status, StatusCode::OK);

        let (input_len, header_len) = recording.received.lock().unwrap().take().expect("put_object was called");
        assert_eq!(input_len, Some(5), "decoded content length must take priority");
        assert_eq!(header_len, Some(5));
    }

    #[tokio::test]
    async fn normalize_content_length_disabled_keeps_strict_header_semantics() {
        let recording = Arc::new(ContentLengthRecordingS3 {
            received: Mutex::new(None),
        });
        let s3: Arc<dyn crate::s3_trait::S3> = recording.clone();
        let config: Arc<dyn S3ConfigProvider> = Arc::new(StaticConfigProvider::new(Arc::new(S3Config {
            presigned_url_max_skew_time_secs: u32::MAX,
            expected_region: Some(REGION.parse().expect("valid test region")),
            normalize_content_length: false,
            ..Default::default()
        })));
        let auth = SimpleAuth::from_single(ACCESS_KEY, SECRET_KEY);
        let ccx = test_context(&s3, &config, &auth);

        let mut req = empty_body_signed_put(Version::HTTP_11);
        assert!(req.headers.get(hyper::header::CONTENT_LENGTH).is_none());

        let response = super::call(&mut req, &ccx).await.unwrap();
        assert_eq!(response.status, StatusCode::OK, "s3s still accepts the request");

        let (input_len, header_len) = recording.received.lock().unwrap().take().expect("put_object was called");
        assert_eq!(input_len, None, "dto content_length must reflect the wire headers when disabled");
        assert_eq!(header_len, None, "no Content-Length may be inserted when disabled");
    }

    #[tokio::test]
    async fn signed_bodyless_operations_reject_invalid_content_length() {
        let test_s3 = Arc::new(TestS3::default());
        let s3: Arc<dyn crate::s3_trait::S3> = test_s3.clone();
        let config = test_config();
        let auth = SimpleAuth::from_single(ACCESS_KEY, SECRET_KEY);
        let ccx = test_context(&s3, &config, &auth);

        let mut malformed = signed_request(Method::GET, Version::HTTP_11, "http://localhost/test-bucket/test-key.txt", &[]);
        malformed
            .headers
            .insert(hyper::header::CONTENT_LENGTH, hyper::header::HeaderValue::from_static("not-a-number"));
        let response = super::call(&mut malformed, &ccx).await.unwrap();
        assert_eq!(response.status, StatusCode::BAD_REQUEST);

        let mut overflowing = signed_request(Method::GET, Version::HTTP_11, "http://localhost/test-bucket/test-key.txt", &[]);
        overflowing.headers.insert(
            hyper::header::CONTENT_LENGTH,
            hyper::header::HeaderValue::from_static("184467440737095516160"),
        );
        let response = super::call(&mut overflowing, &ccx).await.unwrap();
        assert_eq!(response.status, StatusCode::BAD_REQUEST);

        let mut duplicate = signed_request(Method::GET, Version::HTTP_11, "http://localhost/test-bucket/test-key.txt", &[]);
        duplicate
            .headers
            .append(hyper::header::CONTENT_LENGTH, hyper::header::HeaderValue::from_static("0"));
        duplicate
            .headers
            .append(hyper::header::CONTENT_LENGTH, hyper::header::HeaderValue::from_static("0"));
        let response = super::call(&mut duplicate, &ccx).await.unwrap();
        assert_eq!(response.status, StatusCode::BAD_REQUEST);

        assert_eq!(test_s3.total_bodyless_calls(), 0);
    }

    #[tokio::test]
    async fn signed_bodyless_operations_reject_modified_signature() {
        let test_s3 = Arc::new(TestS3::default());
        let s3: Arc<dyn crate::s3_trait::S3> = test_s3.clone();
        let config = test_config();
        let auth = SimpleAuth::from_single(ACCESS_KEY, SECRET_KEY);
        let ccx = test_context(&s3, &config, &auth);

        let mut req = signed_request(Method::GET, Version::HTTP_2, "http://localhost/test-bucket/test-key.txt", &[]);
        let authorization = req
            .headers
            .get(crate::header::AUTHORIZATION)
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();
        let idx = authorization.find("Signature=").expect("test authorization has a signature") + "Signature=".len();
        let mut tampered = authorization.into_bytes();
        tampered[idx..idx + 8].copy_from_slice(b"00000000");
        req.headers
            .insert(crate::header::AUTHORIZATION, hyper::header::HeaderValue::from_bytes(&tampered).unwrap());

        let response = super::call(&mut req, &ccx).await.unwrap();
        assert_eq!(
            response.status,
            StatusCode::FORBIDDEN,
            "tampered signatures must still be rejected on the bodyless path"
        );
        assert_eq!(test_s3.get_object.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn signed_bodyless_operations_reject_non_empty_payload_hash() {
        const NON_EMPTY_SHA256: &str = "0000000000000000000000000000000000000000000000000000000000000000";

        let test_s3 = Arc::new(TestS3::default());
        let s3: Arc<dyn crate::s3_trait::S3> = test_s3.clone();
        let config = test_config();
        let auth = SimpleAuth::from_single(ACCESS_KEY, SECRET_KEY);
        let ccx = test_context(&s3, &config, &auth);

        let uri = "http://localhost/test-bucket/test-key.txt".parse::<Uri>().unwrap();
        let authorization = sign_request(&Method::GET, &uri, NON_EMPTY_SHA256);
        let mut req = Request::from(
            hyper::Request::builder()
                .method(Method::GET)
                .version(Version::HTTP_11)
                .uri(uri.clone())
                .header(crate::header::HOST, uri.authority().unwrap().as_str())
                .header(crate::header::X_AMZ_CONTENT_SHA256, NON_EMPTY_SHA256)
                .header(crate::header::X_AMZ_DATE, AMZ_DATE)
                .header(crate::header::AUTHORIZATION, authorization)
                .body(empty_unknown_length_body())
                .unwrap(),
        );

        let response = super::call(&mut req, &ccx).await.unwrap();
        assert_eq!(
            response.status,
            StatusCode::LENGTH_REQUIRED,
            "a non-empty payload hash claim must still require Content-Length"
        );
        assert_eq!(test_s3.get_object.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn multipart_post_without_content_length_keeps_actual_file_length() {
        use crate::auth::SecretKey;

        let s3: Arc<dyn crate::s3_trait::S3> = Arc::new(post_policy_test_helpers::TestS3NoOp);
        let config: Arc<dyn S3ConfigProvider> = Arc::new(StaticConfigProvider::new(Arc::new(S3Config {
            post_object_max_file_size: 1024,
            presigned_url_max_skew_time_secs: u32::MAX,
            expected_region: Some("us-east-1".parse().expect("valid test region")),
            ..Default::default()
        })));
        let auth = post_policy_test_helpers::create_test_auth();
        let ccx = post_policy_test_helpers::create_test_context(&s3, &config, &auth);
        let secret_key: SecretKey = "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY".into();
        let policy_json = &format!(
            r#"{{"expiration":"2030-01-01T00:00:00.000Z","conditions":[{}]}}"#,
            post_policy_test_helpers::BASE_CONDITIONS,
        );
        let mut req = post_policy_test_helpers::build_post_object_request(policy_json, "hello", &secret_key, false);
        req.headers.insert(
            crate::header::X_AMZ_CONTENT_SHA256,
            hyper::header::HeaderValue::from_static(s3s_sigv4::EMPTY_STRING_SHA256_HASH),
        );
        req.headers.remove(hyper::header::CONTENT_LENGTH);

        let result = super::prepare(&mut req, &ccx).await;

        match result.expect("multipart POST without Content-Length should prepare") {
            Prepare::S3(op) => assert_eq!(op.name(), "PostObject"),
            Prepare::CustomRoute => panic!("multipart POST should not dispatch to a custom route"),
        }
        let stream = req.s3ext.post_object_stream.as_ref().expect("post object stream");
        assert_eq!(
            stream.remaining_length().exact(),
            Some(5),
            "the file must be aggregated at its real length, not treated as zero-length"
        );
    }
}

mod extract_decoded_content_length_tests {
    use super::extract_decoded_content_length;
    use hyper::HeaderMap;
    use hyper::header::{HeaderName, HeaderValue};

    const HEADER_NAME: &str = "x-amz-decoded-content-length";

    fn headers_from_slice(slice: &[(&str, &str)]) -> HeaderMap {
        let mut headers = HeaderMap::new();
        for &(name, value) in slice {
            headers.append(
                HeaderName::from_bytes(name.as_bytes()).expect("valid test header name"),
                HeaderValue::from_bytes(value.as_bytes()).expect("valid test header value"),
            );
        }
        headers
    }

    #[test]
    fn missing_header_returns_none() {
        let hs = HeaderMap::new();
        let result = extract_decoded_content_length(&hs).unwrap();
        assert_eq!(result, None);
    }

    #[test]
    fn valid_integer_returns_some() {
        let hs = headers_from_slice(&[(HEADER_NAME, "66560")]);
        let result = extract_decoded_content_length(&hs).unwrap();
        assert_eq!(result, Some(66560));
    }

    #[test]
    fn zero_returns_some() {
        let hs = headers_from_slice(&[(HEADER_NAME, "0")]);
        let result = extract_decoded_content_length(&hs).unwrap();
        assert_eq!(result, Some(0));
    }

    #[test]
    fn non_numeric_value_returns_error() {
        let hs = headers_from_slice(&[(HEADER_NAME, "not-a-number")]);
        let result = extract_decoded_content_length(&hs);
        assert!(result.is_err());
    }

    #[test]
    fn empty_value_returns_error() {
        let hs = headers_from_slice(&[(HEADER_NAME, "")]);
        let result = extract_decoded_content_length(&hs);
        assert!(result.is_err());
    }

    #[test]
    fn negative_value_returns_error() {
        let hs = headers_from_slice(&[(HEADER_NAME, "-1")]);
        let result = extract_decoded_content_length(&hs);
        assert!(result.is_err());
    }

    #[test]
    fn large_value_within_usize() {
        let val = usize::MAX.to_string();
        let hs = headers_from_slice(&[(HEADER_NAME, &val)]);
        let result = extract_decoded_content_length(&hs).unwrap();
        assert_eq!(result, Some(usize::MAX));
    }

    #[test]
    fn u64_max_exceeds_usize_on_32bit() {
        let val = u64::MAX.to_string();
        let hs = headers_from_slice(&[(HEADER_NAME, &val)]);
        let result = extract_decoded_content_length(&hs);
        if usize::try_from(u64::MAX).is_err() {
            // On 32-bit platforms, u64::MAX won't fit in usize
            assert!(result.is_err());
        } else {
            assert!(result.is_ok());
        }
    }
}

mod virtual_hosted_style_hint_tests {
    use super::*;
    use crate::config::{S3ConfigProvider, StaticConfigProvider};
    use crate::dto::*;
    use crate::s3_trait::S3;
    use hyper::Method;
    use std::sync::Arc;

    // S3 impl that records which operation was called
    #[derive(Default)]
    struct RecordS3 {
        create_bucket: std::sync::atomic::AtomicBool,
        list_buckets: std::sync::atomic::AtomicBool,
    }

    #[async_trait::async_trait]
    impl S3 for RecordS3 {
        async fn create_bucket(
            &self,
            _req: crate::S3Request<CreateBucketInput>,
        ) -> crate::S3Result<crate::S3Response<CreateBucketOutput>> {
            self.create_bucket.store(true, std::sync::atomic::Ordering::SeqCst);
            Ok(crate::S3Response::new(CreateBucketOutput::default()))
        }

        async fn list_buckets(
            &self,
            _req: crate::S3Request<ListBucketsInput>,
        ) -> crate::S3Result<crate::S3Response<ListBucketsOutput>> {
            self.list_buckets.store(true, std::sync::atomic::Ordering::SeqCst);
            Ok(crate::S3Response::new(ListBucketsOutput::default()))
        }
    }

    /// Build a request with path "/" and the given method/host.
    fn make_request(method: Method, host: &str) -> crate::http::Request {
        crate::http::Request::from(
            hyper::Request::builder()
                .method(method)
                .uri(format!("http://{host}/"))
                .header(crate::header::HOST, host)
                .body(crate::http::Body::empty())
                .unwrap(),
        )
    }

    /// Read the response body as a string for assertion.
    fn body_str(resp: &super::Response) -> String {
        let bytes = resp.body.bytes().expect("response body should be available");
        String::from_utf8(bytes.to_vec()).unwrap()
    }

    #[tokio::test]
    async fn actionable_501_for_virtual_hosted_style_without_s3_host() {
        let record = Arc::new(RecordS3::default());
        let s3: Arc<dyn S3> = record.clone();
        let config: Arc<dyn S3ConfigProvider> = Arc::new(StaticConfigProvider::default());
        let ccx = super::CallContext {
            s3: &s3,
            config: &config,
            host: None,
            auth: None,
            access: None,
            route: None,
            validation: None,
        };

        // All methods that resolve_route rejects for S3Path::Root
        // should get the actionable hint.
        for method in [Method::PUT, Method::HEAD, Method::DELETE, Method::POST] {
            let mut req = make_request(method.clone(), "my-bucket.example.com");
            let result = super::call(&mut req, &ccx).await;

            assert!(result.is_ok(), "call failed for {method}");
            let resp = result.unwrap();
            assert_eq!(resp.status, http::StatusCode::NOT_IMPLEMENTED, "wrong status for {method}");
            let text = body_str(&resp);
            assert!(
                text.contains("virtual-hosted-style"),
                "body should mention virtual-hosted-style for {method}: {text}"
            );
            assert!(
                !text.contains("Unknown operation"),
                "body should not be the old generic error for {method}: {text}"
            );
        }
    }

    #[tokio::test]
    async fn get_virtual_hosted_style_without_s3_host_passes_through() {
        let record = Arc::new(RecordS3::default());
        let s3: Arc<dyn S3> = record.clone();
        let config: Arc<dyn S3ConfigProvider> = Arc::new(StaticConfigProvider::default());
        let ccx = super::CallContext {
            s3: &s3,
            config: &config,
            host: None,
            auth: None,
            access: None,
            route: None,
            validation: None,
        };
        let mut req = make_request(Method::GET, "my-bucket.example.com");

        let result = super::call(&mut req, &ccx).await;

        assert!(result.is_ok());
        // GET Root → ListBuckets should still work
        assert!(record.list_buckets.load(std::sync::atomic::Ordering::SeqCst));
    }

    #[tokio::test]
    async fn no_hint_for_non_virtual_hosted_style_hosts() {
        let record = Arc::new(RecordS3::default());
        let s3: Arc<dyn S3> = record.clone();
        let config: Arc<dyn S3ConfigProvider> = Arc::new(StaticConfigProvider::default());
        let ccx = super::CallContext {
            s3: &s3,
            config: &config,
            host: None,
            auth: None,
            access: None,
            route: None,
            validation: None,
        };

        // IP, localhost, two-label domains, and bracketed IPv6 should not trigger the VH hint.
        for host in [
            "127.0.0.1:9000",
            "localhost:9000",
            "example.com:9000",
            "[::ffff:127.0.0.1]:9000", // IPv4-mapped IPv6 with embedded dots
        ] {
            let mut req = make_request(Method::PUT, host);
            let result = super::call(&mut req, &ccx).await;

            assert!(result.is_ok(), "call failed for {host}");
            let resp = result.unwrap();
            assert_eq!(resp.status, http::StatusCode::NOT_IMPLEMENTED, "wrong status for {host}");
            let text = body_str(&resp);
            assert!(!text.contains("virtual-hosted-style"), "hint should not be triggered for {host}: {text}");
        }
    }

    #[tokio::test]
    async fn put_virtual_hosted_style_with_s3_host_passes() {
        use crate::host::SingleDomain;

        let record = Arc::new(RecordS3::default());
        let s3: Arc<dyn S3> = record.clone();
        let config: Arc<dyn S3ConfigProvider> = Arc::new(StaticConfigProvider::default());
        let host = SingleDomain::new("example.com").unwrap();
        let ccx = super::CallContext {
            s3: &s3,
            config: &config,
            host: Some(&host),
            auth: None,
            access: None,
            route: None,
            validation: None,
        };
        let mut req = make_request(Method::PUT, "my-bucket.example.com");

        let result = super::call(&mut req, &ccx).await;

        assert!(result.is_ok());
        // CreateBucket should have been called
        assert!(record.create_bucket.load(std::sync::atomic::Ordering::SeqCst));
    }

    #[tokio::test]
    async fn virtual_hosted_style_hint_does_not_expose_internal_api() {
        let record = Arc::new(RecordS3::default());
        let s3: Arc<dyn S3> = record.clone();
        let config: Arc<dyn S3ConfigProvider> = Arc::new(StaticConfigProvider::default());
        let ccx = super::CallContext {
            s3: &s3,
            config: &config,
            host: None,
            auth: None,
            access: None,
            route: None,
            validation: None,
        };
        let mut req = make_request(Method::PUT, "my-bucket.example.com");

        let result = super::call(&mut req, &ccx).await;

        assert!(result.is_ok());
        let resp = result.unwrap();
        // `crate::http::Body` stores bytes directly when built from `serialize_error`.
        let body_bytes = resp.body.bytes().expect("error response body should be available");
        let body_str = std::str::from_utf8(&body_bytes).unwrap();
        assert!(!body_str.contains("S3ServiceBuilder"));
        assert!(!body_str.contains("SingleDomain"));
        assert!(!body_str.contains("MultiDomain"));
        assert!(!body_str.contains("S3Host"));
    }
}

#[cfg(feature = "minio")]
mod listen_bucket_notification {
    use super::*;

    use crate::ops::generated_minio::resolve_route;
    use crate::path::S3Path;

    fn make_get_request(uri: &str) -> crate::http::Request {
        crate::http::Request::from(
            hyper::Request::builder()
                .method(Method::GET)
                .uri(uri)
                .body(crate::http::Body::empty())
                .unwrap(),
        )
    }

    fn resolve(uri: &str) -> &'static str {
        let req = make_get_request(uri);
        let path = S3Path::Bucket { bucket: "bucket".into() };
        let query = req.uri.query().unwrap_or_default();
        let pairs: Vec<(String, String)> = serde_urlencoded::from_str(query).unwrap();
        let qs = crate::http::OrderedQs::from_vec_unchecked(pairs);
        resolve_route(&req, &path, Some(&qs)).unwrap().name()
    }

    #[test]
    fn route_events_to_listen_bucket_notification() {
        // The `events` query parameter is the MinIO ListenBucketNotification extension.
        assert_eq!(resolve("http://localhost/bucket?events=s3:ObjectCreated:*"), "ListenBucketNotification");
        // Values are not matched, only presence.
        assert_eq!(resolve("http://localhost/bucket?events"), "ListenBucketNotification");
        // Extra query parameters do not change the outcome.
        assert_eq!(
            resolve("http://localhost/bucket?events=s3:ObjectCreated:*&prefix=zz&suffix=yy"),
            "ListenBucketNotification"
        );
    }

    #[test]
    fn deserialize_repeated_events_keys() {
        // Clients like `mc watch` send one `events` key per event name.
        let mut req = make_get_request(
            "http://localhost/bucket?events=s3:ObjectCreated:*&events=s3:ObjectRemoved:*&events=s3:ObjectAccessed:*",
        );
        let query = req.uri.query().unwrap_or_default();
        let pairs: Vec<(String, String)> = serde_urlencoded::from_str(query).unwrap();
        req.s3ext.qs = Some(crate::http::OrderedQs::from_vec_unchecked(pairs));
        req.s3ext.s3_path = Some(S3Path::Bucket { bucket: "bucket".into() });
        let input = crate::ops::generated_minio::ListenBucketNotification::deserialize_http(&mut req).unwrap();
        assert_eq!(input.events.as_deref(), Some("s3:ObjectCreated:*,s3:ObjectRemoved:*,s3:ObjectAccessed:*"));
        assert_eq!(input.prefix.as_deref(), None);
    }

    #[test]
    fn route_bucket_without_events_unchanged() {
        // No `events` parameter: falls through to the usual bucket listing ops.
        assert_eq!(resolve("http://localhost/bucket"), "ListObjects");
        assert_eq!(resolve("http://localhost/bucket?list-type=2"), "ListObjectsV2");
        assert_eq!(resolve("http://localhost/bucket?versions"), "ListObjectVersions");
    }
}
