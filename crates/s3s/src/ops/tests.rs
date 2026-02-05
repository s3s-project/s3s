use super::*;

// use crate::service::S3Service;

// use stdx::mem::output_size;

// #[test]
// #[ignore]
// fn track_future_size() {
//     macro_rules! future_size {
//         ($f:path, $v:expr) => {
//             (stringify!($f), output_size(&$f), $v)
//         };
//     }

//     #[rustfmt::skip]
//     let sizes = [
//         future_size!(S3Service::call,                           2704),
//         future_size!(call,                                      1512),
//         future_size!(prepare,                                   1440),
//         future_size!(SignatureContext::check,                   776),
//         future_size!(SignatureContext::v2_check,                296),
//         future_size!(SignatureContext::v2_check_presigned_url,  168),
//         future_size!(SignatureContext::v2_check_header_auth,    184),
//         future_size!(SignatureContext::v4_check,                752),
//         future_size!(SignatureContext::v4_check_post_signature, 368),
//         future_size!(SignatureContext::v4_check_presigned_url,  456),
//         future_size!(SignatureContext::v4_check_header_auth,    640),
//     ];

//     println!("{sizes:#?}");
//     for (name, size, expected) in sizes {
//         assert_eq!(size, expected, "{name:?} size changed: prev {expected}, now {size}");
//     }
// }

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

#[tokio::test]
async fn presigned_url_expires_0_should_be_expired() {
    use crate::S3ErrorCode;
    use crate::config::{S3ConfigProvider, StaticConfigProvider};
    use crate::http::{Body, OrderedHeaders, OrderedQs};
    use crate::ops::signature::SignatureContext;
    use hyper::{Method, Uri};
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
    let mut body = Body::empty();

    let mut cx = SignatureContext {
        auth: None,
        config: &config,
        req_version: ::http::Version::HTTP_11,
        req_method: &method,
        req_uri: &uri,
        req_body: &mut body,
        qs: Some(&qs),
        hs: OrderedHeaders::from_slice_unchecked(&[]),
        decoded_uri_path: "/test.txt".to_owned(),
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
    use crate::config::{S3ConfigProvider, StaticConfigProvider};
    use crate::http::{Body, Request};
    use crate::ops::CallContext;
    use crate::sig_v4;
    use bytes::Bytes;
    use hyper::Method;
    use hyper::header::HeaderValue;
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
    let config: Arc<dyn S3ConfigProvider> = Arc::new(StaticConfigProvider::default());

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
    let credential = "AKIAIOSFODNN7EXAMPLE/20200926/us-east-1/s3/aws4_request";
    let amz_date = sig_v4::AmzDate::parse("20200926T132547Z").unwrap();
    let region = "us-east-1";
    let service = "s3";
    let signature = sig_v4::calculate_signature(policy_b64, &secret_key, &amz_date, region, service);

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
        amz_date = amz_date.fmt_iso8601(),
        b = boundary,
        signature = signature,
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
                HeaderValue::from_str(&format!("multipart/form-data; boundary={boundary}")).unwrap(),
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

// Helper functions for POST policy resource exhaustion tests

/// Helper to create a test S3 service that tracks POST calls
mod post_policy_test_helpers {
    use crate::S3Request;
    use crate::auth::{SecretKey, SimpleAuth};
    use crate::config::{S3Config, S3ConfigProvider, StaticConfigProvider};
    use crate::http::{Body, Request};
    use crate::ops::CallContext;
    use crate::sig_v4;
    use bytes::Bytes;
    use hyper::Method;
    use hyper::header::HeaderValue;
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
            post_object_max_file_size,
            ..Default::default()
        };
        Arc::new(StaticConfigProvider::new(Arc::new(config)))
    }

    /// Create auth and `CallContext` for testing
    pub fn create_test_context<'a>(
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

    /// Create a `SimpleAuth` for testing
    pub fn create_test_auth() -> SimpleAuth {
        let access_key = "AKIAIOSFODNN7EXAMPLE";
        let secret_key: SecretKey = "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY".into();
        SimpleAuth::from_single(access_key, secret_key)
    }

    /// Build a POST object request with a policy
    pub fn build_post_object_request(policy_json: &str, file_content: &str, secret_key: &SecretKey) -> Request {
        let policy_b64 = base64_simd::STANDARD.encode_to_string(policy_json);

        let boundary = "------------------------test12345678";
        let bucket = "test-bucket";
        let key = "test-key";
        let amz_date = sig_v4::AmzDate::parse("20250101T000000Z").unwrap();
        let region = "us-east-1";
        let service = "s3";
        let algorithm = "AWS4-HMAC-SHA256";
        let credential = "AKIAIOSFODNN7EXAMPLE/20250101/us-east-1/s3/aws4_request";
        let signature = sig_v4::calculate_signature(&policy_b64, secret_key, &amz_date, region, service);

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
                "Content-Disposition: form-data; name=\"file\"; filename=\"test.txt\"\r\n",
                "Content-Type: text/plain\r\n\r\n",
                "{file_content}\r\n",
                "--{b}--\r\n"
            ),
            amz_date = amz_date.fmt_iso8601(),
            b = boundary,
            signature = signature,
            bucket = bucket,
            policy_b64 = policy_b64,
            algorithm = algorithm,
            credential = credential,
            key = key,
            file_content = file_content,
        );

        Request::from(
            hyper::Request::builder()
                .method(Method::POST)
                .uri(format!("http://localhost/{bucket}"))
                .header(crate::header::HOST, "localhost")
                .header(
                    crate::header::CONTENT_TYPE,
                    HeaderValue::from_str(&format!("multipart/form-data; boundary={boundary}")).unwrap(),
                )
                .body(Body::from(Bytes::from(body)))
                .unwrap(),
        )
    }
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
    let policy_json = r#"{"expiration":"2030-01-01T00:00:00.000Z","conditions":[["content-length-range",0,100]]}"#;
    let file_content = "a".repeat(50); // 50 bytes (within policy limit of 100 bytes)

    let mut req = post_policy_test_helpers::build_post_object_request(policy_json, &file_content, &secret_key);

    // This should succeed because file size (50 bytes) is within policy limit (100 bytes)
    // The important part is that the aggregation limit used is 100 bytes (policy max), not 1MB (config max)
    let result = super::prepare(&mut req, &ccx).await;
    assert!(result.is_ok(), "expected success for file within policy limit");
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
    let policy_json = r#"{"expiration":"2030-01-01T00:00:00.000Z","conditions":[["content-length-range",0,100]]}"#;
    // Create a file with 150 bytes (exceeds policy max of 100 bytes, but under config max of 10KB)
    // This is the critical security test: file should be rejected before consuming memory
    let file_content = "a".repeat(150);

    let mut req = post_policy_test_helpers::build_post_object_request(policy_json, &file_content, &secret_key);

    // This should fail because file size (150 bytes) exceeds policy limit (100 bytes)
    // The key security improvement: file is rejected during aggregation (at 100 bytes limit),
    // not after reading the full 150 bytes (or potentially larger files)
    let result = super::prepare(&mut req, &ccx).await;
    assert!(result.is_err(), "expected error for file exceeding policy limit");

    // MultipartError::FileTooLarge is wrapped into InvalidRequest by the invalid_request! macro
    match result {
        Err(err) => {
            let code = err.code();
            assert!(
                matches!(code, crate::error::S3ErrorCode::InvalidRequest),
                "expected InvalidRequest error, got {code:?}",
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
    let policy_json = r#"{"expiration":"2030-01-01T00:00:00.000Z","conditions":[["content-length-range",0,10240]]}"#;
    // Create a file with 150 bytes (within config max of 200 bytes, within policy max of 10KB)
    let file_content = "a".repeat(150);

    let mut req = post_policy_test_helpers::build_post_object_request(policy_json, &file_content, &secret_key);

    // This should succeed because file size (150 bytes) is within config max (200 bytes)
    // The aggregation limit used is min(policy_max=10KB, config_max=200) = 200 bytes
    let result = super::prepare(&mut req, &ccx).await;
    assert!(result.is_ok(), "expected success for file within config limit");
}

// ========================================
// Access Control Tests
// ========================================

/// Test S3 route denies anonymous access when auth is configured
#[tokio::test]
async fn test_s3_route_anonymous_access_denied() {
    use crate::S3Request;
    use crate::auth::{SecretKey, SimpleAuth};
    use crate::config::{S3ConfigProvider, StaticConfigProvider};
    use crate::http::{Body, Request};
    use crate::ops::CallContext;
    use hyper::Method;
    use std::sync::Arc;
    use std::sync::atomic::AtomicUsize;

    struct TestS3 {
        get_object_calls: AtomicUsize,
    }

    #[async_trait::async_trait]
    impl crate::s3_trait::S3 for TestS3 {
        async fn get_object(
            &self,
            _req: S3Request<crate::dto::GetObjectInput>,
        ) -> crate::error::S3Result<crate::protocol::S3Response<crate::dto::GetObjectOutput>> {
            self.get_object_calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(crate::protocol::S3Response::new(crate::dto::GetObjectOutput::default()))
        }
    }

    let test_s3 = Arc::new(TestS3 {
        get_object_calls: AtomicUsize::new(0),
    });
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

    // This should fail with AccessDenied because the request has no authentication
    let result = super::prepare(&mut req, &ccx).await;

    match result {
        Err(err) => {
            assert_eq!(
                *err.code(),
                crate::error::S3ErrorCode::AccessDenied,
                "Anonymous request should be denied"
            );
        }
        Ok(_) => panic!("Anonymous request should have been denied"),
    }

    // Verify that the S3 service was never called
    assert_eq!(test_s3.get_object_calls.load(std::sync::atomic::Ordering::SeqCst), 0);
}

/// Test S3 route with custom S3Access that allows anonymous access
#[tokio::test]
async fn test_s3_route_custom_access_allows_anonymous() {
    use crate::S3Request;
    use crate::access::{S3Access, S3AccessContext};
    use crate::auth::{SecretKey, SimpleAuth};
    use crate::config::{S3ConfigProvider, StaticConfigProvider};
    use crate::http::{Body, Request};
    use crate::ops::CallContext;
    use hyper::Method;
    use std::sync::Arc;
    use std::sync::atomic::AtomicUsize;

    struct TestS3 {
        get_object_calls: AtomicUsize,
    }

    #[async_trait::async_trait]
    impl crate::s3_trait::S3 for TestS3 {
        async fn get_object(
            &self,
            _req: S3Request<crate::dto::GetObjectInput>,
        ) -> crate::error::S3Result<crate::protocol::S3Response<crate::dto::GetObjectOutput>> {
            self.get_object_calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(crate::protocol::S3Response::new(crate::dto::GetObjectOutput::default()))
        }
    }

    /// Custom S3Access that allows anonymous access
    struct AnonymousAccess;

    #[async_trait::async_trait]
    impl S3Access for AnonymousAccess {
        async fn check(&self, _cx: &mut S3AccessContext<'_>) -> crate::error::S3Result<()> {
            // Allow all access, including anonymous
            Ok(())
        }
    }

    let test_s3 = Arc::new(TestS3 {
        get_object_calls: AtomicUsize::new(0),
    });
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

    // This should pass access control because custom access allows anonymous
    let result = super::prepare(&mut req, &ccx).await;

    // Should succeed (or fail with an error other than AccessDenied)
    match result {
        Err(err) => {
            assert_ne!(
                *err.code(),
                crate::error::S3ErrorCode::AccessDenied,
                "Should not be denied when custom access control allows anonymous"
            );
        }
        Ok(_) => {
            // Success is expected
        }
    }
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
    use hyper::{HeaderMap, Method, Uri};
    use hyper::header::HeaderValue;
    use hyper::http::Extensions;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct TestS3;

    #[async_trait::async_trait]
    impl crate::s3_trait::S3 for TestS3 {}

    /// Custom route that uses default check_access (requires authentication)
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
                    .map(|v| v.as_bytes() == b"application/x-custom")
                    .unwrap_or(false)
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
            .header(
                hyper::header::CONTENT_TYPE,
                HeaderValue::from_static("application/x-custom"),
            )
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
            panic!("Unexpected error that wasn't serialized: {:?}", err);
        }
    }

    // Verify that the custom route was never actually called (because access was denied)
    assert_eq!(custom_route.get_call_count(), 0);
}

/// Test custom route that overrides check_access to allow anonymous access
#[tokio::test]
async fn test_custom_route_anonymous_access_allowed_when_overridden() {
    use crate::S3Request;
    use crate::auth::{SecretKey, SimpleAuth};
    use crate::config::{S3ConfigProvider, StaticConfigProvider};
    use crate::http::{Body, Request};
    use crate::ops::CallContext;
    use crate::protocol::S3Response;
    use crate::route::S3Route;
    use hyper::{HeaderMap, Method, Uri};
    use hyper::header::HeaderValue;
    use hyper::http::Extensions;
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
                    .map(|v| v.as_bytes() == b"application/x-public")
                    .unwrap_or(false)
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
            .header(
                hyper::header::CONTENT_TYPE,
                HeaderValue::from_static("application/x-public"),
            )
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
            panic!("Anonymous request should succeed on public route, got error: {:?}", err);
        }
    }

    // Verify that the custom route was actually called
    assert_eq!(anonymous_route.get_call_count(), 1);
}
