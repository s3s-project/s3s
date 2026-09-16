// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

//! Access control: `S3Access` and `S3Route::check_access` decisions for anonymous and
//! unsigned requests.

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
