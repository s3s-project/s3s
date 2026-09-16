// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

//! `POST Object` (multipart form upload) and `PostPolicy` sizing, streaming and trailer
//! validation.

use super::common::*;
use super::*;

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
