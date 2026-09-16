// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

//! Focused micro-benchmarks for the GET response path.
//!
//! Ignored by default; see [`super`] for the shared invocation:
//!
//! ```bash
//! cargo test -p s3s --release --lib -- ops::benches::get_object --ignored --nocapture
//! ```

use crate::ops::*;

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
        let prep = crate::ops::prepare(&mut req, ccx).await.unwrap();
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
        let resp = crate::ops::call(&mut req, ccx).await.unwrap();
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
