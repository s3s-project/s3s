// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

//! The virtual-hosted-style hint returned when no `S3Host` is configured.

use super::*;

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
            if method == Method::HEAD {
                // RFC 9110 §9.3.2: HEAD responses must not carry a body.
                assert!(http_body::Body::is_end_stream(&resp.body), "HEAD response must be bodyless");
                continue;
            }
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
