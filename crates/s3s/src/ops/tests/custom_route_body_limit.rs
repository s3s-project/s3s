// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

//! Custom-route request body limit (`S3Config::custom_route_max_body_size`).

use super::common::*;
use super::*;

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
