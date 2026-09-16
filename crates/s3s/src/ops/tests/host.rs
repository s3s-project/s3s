// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

//! Host handling: path-style vs virtual-hosted-style, ports, the `Host` header and
//! HTTP/2 authority rules.

use super::*;

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

#[tokio::test]
async fn vhost_with_port_routes_bucket() {
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
    let host = MultiDomain::new(["fs.example.com"]).unwrap();
    let ccx = CallContext {
        s3: &s3,
        config: &config,
        host: Some(&host),
        auth: None,
        access: None,
        route: None,
        validation: None,
    };

    // HTTP/1.1: the Host header carries the port.
    let mut req = Request::from(
        hyper::Request::builder()
            .method(Method::GET)
            .uri("http://user.fs.example.com:19000/avatar.png")
            .header(crate::header::HOST, "user.fs.example.com:19000")
            .body(Body::empty())
            .unwrap(),
    );
    let _ = super::prepare(&mut req, &ccx).await;
    assert!(
        matches!(req.s3ext.s3_path, Some(S3Path::Object { ref bucket, .. }) if bucket.as_ref() == "user"),
        "port-carrying vhost host must route to its bucket"
    );

    // HTTP/2: no Host header; the :authority is injected as the host.
    let mut req = Request::from(
        hyper::Request::builder()
            .method(Method::GET)
            .version(::http::Version::HTTP_2)
            .uri("http://user.fs.example.com:19000/avatar.png")
            .body(Body::empty())
            .unwrap(),
    );
    assert!(req.headers.get(crate::header::HOST).is_none());
    let _ = super::prepare(&mut req, &ccx).await;
    assert!(
        matches!(req.s3ext.s3_path, Some(S3Path::Object { ref bucket, .. }) if bucket.as_ref() == "user"),
        "HTTP/2 :authority with a port must route to its bucket"
    );
}

#[tokio::test]
async fn vhost_with_port_unconfigured_domain_falls_back_to_path_style() {
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
    let host = MultiDomain::new(["other.example.com"]).unwrap();
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
            .uri("http://user.fs.example.com:19000/avatar.png")
            .header(crate::header::HOST, "user.fs.example.com:19000")
            .body(Body::empty())
            .unwrap(),
    );
    let _ = super::prepare(&mut req, &ccx).await;
    assert!(
        !matches!(req.s3ext.s3_path, Some(S3Path::Object { ref bucket, .. }) if bucket.as_ref() == "user"),
        "an unconfigured domain with a port must not route to a vhost bucket"
    );
}

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
async fn presigned_url_with_port_and_vhost_routes_bucket() {
    use crate::auth::{SecretKey, SimpleAuth};
    use crate::config::{S3Config, S3ConfigProvider, StaticConfigProvider};
    use crate::host::MultiDomain;
    use crate::http::{Body, Request};
    use std::sync::Arc;

    struct RecordingS3 {
        received: std::sync::Mutex<Option<(String, String)>>, // (bucket, key)
    }
    #[async_trait::async_trait]
    impl crate::s3_trait::S3 for RecordingS3 {
        async fn get_object(
            &self,
            req: crate::S3Request<crate::dto::GetObjectInput>,
        ) -> crate::error::S3Result<crate::protocol::S3Response<crate::dto::GetObjectOutput>> {
            *self.received.lock().unwrap() = Some((req.input.bucket.clone(), req.input.key.clone()));
            Ok(crate::protocol::S3Response::new(crate::dto::GetObjectOutput::default()))
        }
    }

    let access_key = "AKIAIOSFODNN7EXAMPLE";
    let secret_key: SecretKey = "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY".into();
    let recording = Arc::new(RecordingS3 {
        received: std::sync::Mutex::new(None),
    });
    let s3: Arc<dyn crate::s3_trait::S3> = recording.clone();
    let config: Arc<dyn S3ConfigProvider> = Arc::new(StaticConfigProvider::new(Arc::new(S3Config {
        presigned_url_max_skew_time_secs: u32::MAX,
        ..Default::default()
    })));
    let auth = SimpleAuth::from_single(access_key, secret_key.clone());
    let host = MultiDomain::new(["fs.example.com"]).unwrap();
    let ccx = CallContext {
        s3: &s3,
        config: &config,
        host: Some(&host),
        auth: Some(&auth),
        access: None,
        route: None,
        validation: None,
    };

    // Sign a presigned URL for the port-carrying vhost host.
    let host_hdr = "user.fs.example.com:19000";
    let amz_date = s3s_sigv4::AmzDate::parse(&format_current_amz_date()).expect("current time is a valid x-amz-date");
    let query_fields = [
        ("X-Amz-Algorithm".to_owned(), "AWS4-HMAC-SHA256".to_owned()),
        (
            "X-Amz-Credential".to_owned(),
            format!("{access_key}/{}/us-east-1/s3/aws4_request", amz_date.fmt_date()),
        ),
        ("X-Amz-Date".to_owned(), amz_date.fmt_iso8601().to_string()),
        ("X-Amz-Expires".to_owned(), "604800".to_owned()),
        ("X-Amz-SignedHeaders".to_owned(), "host".to_owned()),
    ];
    let canonical_request =
        s3s_sigv4::create_presigned_canonical_request("GET", "/avatar.png", &query_fields, [("host", host_hdr)]);
    let string_to_sign = s3s_sigv4::create_string_to_sign(&canonical_request, &amz_date, "us-east-1", "s3");
    let signature = s3s_sigv4::calculate_signature(&string_to_sign, secret_key.expose(), &amz_date, "us-east-1", "s3");
    let mut query_fields: Vec<(String, String)> = query_fields.into();
    query_fields.push(("X-Amz-Signature".to_owned(), signature));

    let mut uri = format!("https://{host_hdr}/avatar.png?").to_owned();
    uri.push_str(
        &query_fields
            .iter()
            .map(|(k, v)| format!("{k}={}", urlencoding::encode(v)))
            .collect::<Vec<_>>()
            .join("&"),
    );

    let mut req = Request::from(
        hyper::Request::builder()
            .method(Method::GET)
            .uri(uri)
            .header(crate::header::HOST, host_hdr)
            .body(Body::empty())
            .unwrap(),
    );

    let response = super::call(&mut req, &ccx).await.unwrap();
    assert_eq!(
        response.status,
        hyper::StatusCode::OK,
        "presigned URL with a port-carrying host must succeed"
    );

    let (bucket, key) = recording.received.lock().unwrap().take().expect("get_object was called");
    assert_eq!(bucket.as_str(), "user");
    assert_eq!(key.as_str(), "avatar.png");
}

fn format_current_amz_date() -> String {
    let dt = time::OffsetDateTime::now_utc();
    format!(
        "{:04}{:02}{:02}T{:02}{:02}{:02}Z",
        dt.year(),
        u8::from(dt.month()),
        dt.day(),
        dt.hour(),
        dt.minute(),
        dt.second()
    )
}
