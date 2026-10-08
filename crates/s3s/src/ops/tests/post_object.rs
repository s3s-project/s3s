// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

//! `POST Object` (multipart form upload) and `PostPolicy` sizing, streaming and trailer
//! validation.

use super::common::*;
use super::*;

/// Drains the prepared POST object stream, returning its bytes or the `S3` error
/// code its failure carries.
async fn drain_post_object(req: &mut crate::http::Request) -> Result<Vec<u8>, crate::error::S3ErrorCode> {
    use futures::StreamExt;

    let mut stream = req
        .s3ext
        .post_object_stream
        .take()
        .expect("post object stream should be present");
    let mut data = Vec::new();
    while let Some(chunk) = stream.next().await {
        match chunk {
            Ok(bytes) => data.extend_from_slice(&bytes),
            Err(err) => {
                let err = err
                    .downcast_ref::<crate::http::FileStreamError>()
                    .expect("POST object streams fail with the file stream error");
                return Err(err.to_s3_error_code());
            }
        }
    }
    Ok(data)
}

/// Returns the exact length the prepared POST object stream reports, if it has one.
fn post_object_exact_length(req: &crate::http::Request) -> Option<usize> {
    req.s3ext
        .post_object_stream
        .as_ref()
        .expect("post object stream should be present")
        .remaining_length()
        .exact()
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
    assert_eq!(err.code(), &crate::error::S3ErrorCode::InvalidArgument);
    assert_eq!(err.message(), Some("the region 'us-east-1' is wrong; expecting 'us-west-2'"));
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

    // The policy maximum is enforced on the file stream: the file is rejected
    // as soon as it crosses 100 bytes, not after the whole body is read.
    let result = super::prepare(&mut req, &ccx).await;
    assert!(result.is_ok(), "prepare does not read the file");

    let code = drain_post_object(&mut req).await.unwrap_err();
    assert_eq!(
        code,
        crate::error::S3ErrorCode::EntityTooLarge,
        "a file over the policy maximum is EntityTooLarge"
    );
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
    assert!(result.is_ok(), "the range is checked while the file is read");

    let code = drain_post_object(&mut req).await.unwrap_err();
    assert_eq!(
        code,
        crate::error::S3ErrorCode::EntityTooLarge,
        "a file over the policy range is EntityTooLarge"
    );
}

/// A policy minimum above the effective maximum describes a range no body can
/// satisfy. It is a client error, answered before a stream is built: building
/// one would invert the wrapper's bounds and panic on the request path.
#[tokio::test]
async fn post_object_policy_min_above_config_max_is_a_client_error() {
    use crate::auth::SecretKey;
    use std::sync::Arc;

    let s3: Arc<dyn crate::s3_trait::S3> = Arc::new(post_policy_test_helpers::TestS3NoOp);

    // The effective maximum is the tighter of the policy's and the configured one.
    let config = post_policy_test_helpers::create_test_config(1024);

    let auth = post_policy_test_helpers::create_test_auth();
    let ccx = post_policy_test_helpers::create_test_context(&s3, &config, &auth);

    let secret_key: SecretKey = "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY".into();

    // Minimum 2048 above the configured maximum 1024: unsatisfiable.
    let policy_json = &format!(
        r#"{{"expiration":"2030-01-01T00:00:00.000Z","conditions":[["content-length-range",2048,4096],{}]}}"#,
        post_policy_test_helpers::BASE_CONDITIONS,
    );
    let file_content = "any contents at all";

    let mut req = post_policy_test_helpers::build_post_object_request(policy_json, file_content, &secret_key, false);

    let Err(err) = super::prepare(&mut req, &ccx).await else {
        panic!("an unsatisfiable content-length-range is a client error");
    };
    assert_eq!(
        err.code(),
        &crate::error::S3ErrorCode::EntityTooSmall,
        "the buffered path answered EntityTooSmall for an unsatisfiable range"
    );
}

/// The file part is forwarded as a stream whose length is not derived from the
/// request: how many trailer bytes the body carries is only known once it has
/// been read, so the stream reports a range rather than an exact length.
#[tokio::test]
async fn post_object_with_content_length_streams_an_unknown_length_file() {
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
    let file_content = "file content for streaming";
    let mut req = post_policy_test_helpers::build_post_object_request(policy_json, file_content, &secret_key, false);

    let result = super::prepare(&mut req, &ccx).await;
    assert!(result.is_ok(), "expected prepare to succeed");

    assert_eq!(
        post_object_exact_length(&req),
        None,
        "the file length is not known before the body is read"
    );

    let data = drain_post_object(&mut req)
        .await
        .expect("the file is inside the configured range");
    assert_eq!(data, file_content.as_bytes());
}

#[tokio::test]
async fn post_object_empty_file_streams_zero_length() {
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
    let mut req = post_policy_test_helpers::build_post_object_request(policy_json, "", &secret_key, false);

    let result = super::prepare(&mut req, &ccx).await;
    assert!(result.is_ok(), "expected prepare to succeed");

    let data = drain_post_object(&mut req).await.expect("an empty file is inside the range");
    assert!(data.is_empty(), "zero-length file must yield no content");
}

#[tokio::test]
async fn post_object_content_with_near_miss_boundary_streams_exactly() {
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
    // The helper's boundary is "------------------------test12345678"; the
    // content embeds CRLFs and a truncated copy of it (missing the final 8).
    let file_content = "alpha\r\nbeta\r\n\r\n--------------------------test1234567X tail\r\nmore content\r\n";
    let mut req = post_policy_test_helpers::build_post_object_request(policy_json, file_content, &secret_key, false);

    let result = super::prepare(&mut req, &ccx).await;
    assert!(result.is_ok(), "expected prepare to succeed");

    let data = drain_post_object(&mut req).await.expect("the content is inside the range");
    assert_eq!(data, file_content.as_bytes());
}

/// A chunked POST (no request `Content-Length`) is streamed as well: the
/// expected range is enforced while the bytes arrive.
#[tokio::test]
async fn post_object_chunked_form_streams_the_file() {
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
    let file_content = "file content for chunked upload";
    let mut req = post_policy_test_helpers::build_post_object_request_chunked(policy_json, file_content, &secret_key, 1024);

    let result = super::prepare(&mut req, &ccx).await;
    if let Err(err) = &result {
        panic!("expected prepare to succeed, got {err:?}");
    }

    let data = drain_post_object(&mut req).await.expect("the file is inside the range");
    assert_eq!(data, file_content.as_bytes());
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

    // Nothing reads the body before dispatch: the limit is enforced on the
    // stream, as soon as the bytes cross it.
    let result = super::prepare(&mut req, &ccx).await;
    assert!(result.is_ok(), "prepare does not read the file");

    let code = drain_post_object(&mut req).await.unwrap_err();
    assert_eq!(code, crate::error::S3ErrorCode::EntityTooLarge, "an oversized file is EntityTooLarge");
}

/// A body stream that fails is reported while the file stream is read, not by
/// `prepare`: nothing reads the body before dispatch.
#[tokio::test]
async fn post_object_chunked_reports_a_broken_body_stream() {
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
    // an error right after the file part headers: reading the file stream hits
    // the transport error.
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
    // The fields helper already terminates the last field with CRLF, so the file
    // part starts with its own delimiter.
    let file_field_header = format!(
        "--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"test.txt\"\r\nContent-Type: text/plain\r\n\r\n"
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
    let Ok(_prepare) = result else {
        let err = result.as_ref().err().expect("checked above");
        panic!("prepare does not read the body, got {err:?}");
    };

    // A failing body stream is an incomplete body; the *malformed* bodies
    // (a truncated closing delimiter, another part after the file) are the
    // client errors, which `FileStreamError::to_s3_error_code` maps to 400.
    let code = drain_post_object(&mut req).await.unwrap_err();
    assert_eq!(code, crate::error::S3ErrorCode::IncompleteBody, "a failing body stream is incomplete");
}

/// The closing delimiter may be followed directly by the end of the body: how
/// many trailer bytes the request carries is only known once the body has been
/// read, so the file part is streamed and its length is the length of the data
/// the stream delivers.
#[tokio::test]
async fn post_object_with_content_length_accepts_a_missing_final_crlf() {
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
    let mut req = post_policy_test_helpers::build_post_object_request(policy_json, "content", &secret_key, false);

    // Strip the final CRLF from the multipart body so the closing delimiter is
    // followed directly by the end of the body.
    let mut full = req.body.bytes().expect("body should be buffered").to_vec();
    assert!(full.ends_with(b"--\r\n"));
    full.pop();
    full.pop();
    req.body = crate::http::Body::from(bytes::Bytes::from(full.clone()));
    req.headers
        .insert(hyper::header::CONTENT_LENGTH, hyper::header::HeaderValue::from(full.len()));

    let result = super::prepare(&mut req, &ccx).await;
    assert!(result.is_ok(), "dispatch is not affected");

    let data = drain_post_object(&mut req)
        .await
        .expect("the body is inside the configured range");
    assert_eq!(data, b"content");
}

/// The size limit is enforced while the file part is streamed, so a file over
/// the configured maximum is still rejected with the code clients expect for an
/// oversized upload — as soon as it crosses the limit, without buffering it.
#[tokio::test]
async fn post_object_with_content_length_rejects_an_oversized_file() {
    use crate::auth::SecretKey;
    use std::sync::Arc;

    let s3: Arc<dyn crate::s3_trait::S3> = Arc::new(post_policy_test_helpers::TestS3NoOp);

    // The config maximum applies: the policy carries no content-length-range.
    let config = post_policy_test_helpers::create_test_config(10);
    let auth = post_policy_test_helpers::create_test_auth();
    let ccx = post_policy_test_helpers::create_test_context(&s3, &config, &auth);

    let secret_key: SecretKey = "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY".into();
    let policy_json = &format!(
        r#"{{"expiration":"2030-01-01T00:00:00.000Z","conditions":[{}]}}"#,
        post_policy_test_helpers::BASE_CONDITIONS,
    );
    let file_content = "x".repeat(64);
    let mut req = post_policy_test_helpers::build_post_object_request(policy_json, &file_content, &secret_key, false);

    let result = super::prepare(&mut req, &ccx).await;

    assert!(result.is_ok(), "prepare does not read the file");

    let code = drain_post_object(&mut req).await.unwrap_err();
    assert_eq!(
        code,
        crate::error::S3ErrorCode::EntityTooLarge,
        "a file over the configured maximum is EntityTooLarge"
    );
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
    // Use 1 KiB chunks so the body stream yields ~30 chunks for the file part:
    // the range counts bytes, not the number of chunks.
    let file_content = "a".repeat(30_000);
    let chunk_size = 1024;

    let mut req =
        post_policy_test_helpers::build_post_object_request_chunked(policy_json, &file_content, &secret_key, chunk_size);

    let result = super::prepare(&mut req, &ccx).await;
    assert!(result.is_ok(), "the range is checked while the file is read");

    // This must succeed: the file is 30 000 bytes, within [100, 50000].
    let data = drain_post_object(&mut req)
        .await
        .expect("30 000 bytes are within [100, 50000]");
    assert_eq!(data.len(), 30_000, "every byte counts once, whatever the chunking");
    assert_eq!(data, file_content.as_bytes());

    // A file below the minimum fails when the stream ends.
    let small_file_content = "a".repeat(50); // 50 bytes, less than minimum of 100
    let mut req_small =
        post_policy_test_helpers::build_post_object_request_chunked(policy_json, &small_file_content, &secret_key, chunk_size);

    let result_small = super::prepare(&mut req_small, &ccx).await;
    assert!(result_small.is_ok(), "the minimum is checked when the stream ends");

    let code = drain_post_object(&mut req_small).await.unwrap_err();
    assert_eq!(
        code,
        crate::error::S3ErrorCode::EntityTooSmall,
        "a file below the policy minimum is EntityTooSmall"
    );
}

/// A signed `POST Object` form that repeats the `Authorization` header is answered with the code
/// and the message Amazon S3 uses for a header it does not implement, instead of being served
/// through the policy path.
#[tokio::test]
async fn post_object_with_duplicate_authorization_is_rejected() {
    use crate::auth::SecretKey;
    use std::sync::Arc;
    use std::sync::atomic::AtomicUsize;

    let s3: Arc<dyn crate::s3_trait::S3> = Arc::new(post_policy_test_helpers::TestS3WithPostTracking {
        post_calls: AtomicUsize::new(0),
    });
    let config = post_policy_test_helpers::create_test_config(1024 * 1024);
    let auth = post_policy_test_helpers::create_test_auth();
    let ccx = post_policy_test_helpers::create_test_context(&s3, &config, &auth);

    let secret_key: SecretKey = "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY".into();
    let policy_json = &format!(
        r#"{{"expiration":"2030-01-01T00:00:00.000Z","conditions":[["content-length-range",0,100],["eq","$Content-Type","text/plain"],{}]}}"#,
        post_policy_test_helpers::BASE_CONDITIONS,
    );
    let file_content = "a".repeat(50);
    let mut req = post_policy_test_helpers::build_post_object_request(policy_json, &file_content, &secret_key, true);

    req.headers.append(
        crate::header::AUTHORIZATION,
        hyper::header::HeaderValue::from_static("AWS4-HMAC-SHA256 first"),
    );
    req.headers.append(
        crate::header::AUTHORIZATION,
        hyper::header::HeaderValue::from_static("AWS4-HMAC-SHA256 second"),
    );

    let result = super::prepare(&mut req, &ccx).await;
    let Err(err) = result else {
        panic!("a repeated Authorization header must be rejected");
    };
    assert_eq!(*err.code(), S3ErrorCode::NotImplemented);
    assert_eq!(err.message(), Some("A header you provided implies functionality that is not implemented"));
}

/// One `Authorization` header on a signed `POST Object` form does not change the outcome: the
/// request still passes policy validation and routes to the `PostObject` operation.
#[tokio::test]
async fn post_object_with_single_authorization_is_served() {
    use crate::auth::SecretKey;
    use std::sync::Arc;
    use std::sync::atomic::AtomicUsize;

    let s3: Arc<dyn crate::s3_trait::S3> = Arc::new(post_policy_test_helpers::TestS3WithPostTracking {
        post_calls: AtomicUsize::new(0),
    });
    let config = post_policy_test_helpers::create_test_config(1024 * 1024);
    let auth = post_policy_test_helpers::create_test_auth();
    let ccx = post_policy_test_helpers::create_test_context(&s3, &config, &auth);

    let secret_key: SecretKey = "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY".into();
    let policy_json = &format!(
        r#"{{"expiration":"2030-01-01T00:00:00.000Z","conditions":[["content-length-range",0,100],["eq","$Content-Type","text/plain"],{}]}}"#,
        post_policy_test_helpers::BASE_CONDITIONS,
    );
    let file_content = "a".repeat(50);
    let mut req = post_policy_test_helpers::build_post_object_request(policy_json, &file_content, &secret_key, true);

    req.headers.append(
        crate::header::AUTHORIZATION,
        hyper::header::HeaderValue::from_static("AWS4-HMAC-SHA256 not-a-real-credential"),
    );

    let prepare = super::prepare(&mut req, &ccx)
        .await
        .expect("one Authorization header must not change the POST form path");
    let super::Prepare::S3(op) = prepare else {
        panic!("a signed POST object form must route to the S3 PostObject operation");
    };
    assert_eq!(op.name(), "PostObject");
}

// The `x-s3s-payload-length` extension on a form upload: the declaration is a form field, and
// the file stream enforces it together with the policy range.

/// Builds a form upload whose policy covers a declaration field.
fn build_declared_post_request(range: (u64, u64), declared: &str, file_len: usize) -> crate::http::Request {
    let secret_key: crate::auth::SecretKey = "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY".into();
    let policy_json = &format!(
        r#"{{"expiration":"2030-01-01T00:00:00.000Z","conditions":[["content-length-range",{},{}],["eq","$Content-Type","text/plain"],["eq","$x-s3s-payload-length","{}"],{}]}}"#,
        range.0,
        range.1,
        declared,
        post_policy_test_helpers::BASE_CONDITIONS,
    );
    let file_content = "a".repeat(file_len);
    post_policy_test_helpers::build_post_object_request_with(
        policy_json,
        &file_content,
        &secret_key,
        true,
        &[("x-s3s-payload-length", declared)],
        &[],
    )
}

#[tokio::test]
async fn post_object_declared_length_matching_the_file_part_is_accepted() {
    use std::sync::Arc;

    let s3: Arc<dyn crate::s3_trait::S3> = Arc::new(post_policy_test_helpers::TestS3NoOp);
    let config = post_policy_test_helpers::create_test_config(10 * 1024);
    let auth = post_policy_test_helpers::create_test_auth();
    let ccx = post_policy_test_helpers::create_test_context(&s3, &config, &auth);
    let mut req = build_declared_post_request((0, 10_240), "150", 150);

    super::prepare(&mut req, &ccx)
        .await
        .expect("a matching declaration is accepted");
    let data = drain_post_object(&mut req).await.expect("the file is delivered");
    assert_eq!(data.len(), 150);
}

#[tokio::test]
async fn post_object_delivering_fewer_bytes_than_declared_is_too_small() {
    use std::sync::Arc;

    let s3: Arc<dyn crate::s3_trait::S3> = Arc::new(post_policy_test_helpers::TestS3NoOp);
    let config = post_policy_test_helpers::create_test_config(10 * 1024);
    let auth = post_policy_test_helpers::create_test_auth();
    let ccx = post_policy_test_helpers::create_test_context(&s3, &config, &auth);
    let mut req = build_declared_post_request((0, 10_240), "300", 150);

    super::prepare(&mut req, &ccx).await.expect("prepare does not read the file");
    let code = drain_post_object(&mut req).await.unwrap_err();
    assert_eq!(code, crate::error::S3ErrorCode::EntityTooSmall, "fewer bytes than declared");
}

#[tokio::test]
async fn post_object_delivering_more_bytes_than_declared_is_too_large() {
    use std::sync::Arc;

    let s3: Arc<dyn crate::s3_trait::S3> = Arc::new(post_policy_test_helpers::TestS3NoOp);
    let config = post_policy_test_helpers::create_test_config(10 * 1024);
    let auth = post_policy_test_helpers::create_test_auth();
    let ccx = post_policy_test_helpers::create_test_context(&s3, &config, &auth);
    let mut req = build_declared_post_request((0, 10_240), "100", 150);

    super::prepare(&mut req, &ccx).await.expect("prepare does not read the file");
    let code = drain_post_object(&mut req).await.unwrap_err();
    assert_eq!(code, crate::error::S3ErrorCode::EntityTooLarge, "more bytes than declared");
}

#[tokio::test]
async fn post_object_declaration_outside_the_policy_range_is_rejected_before_the_stream() {
    use std::sync::Arc;

    let s3: Arc<dyn crate::s3_trait::S3> = Arc::new(post_policy_test_helpers::TestS3NoOp);
    let config = post_policy_test_helpers::create_test_config(10 * 1024);
    let auth = post_policy_test_helpers::create_test_auth();
    let ccx = post_policy_test_helpers::create_test_context(&s3, &config, &auth);
    // The policy asks for at least 200 bytes; the declaration says 150: no body satisfies both.
    let mut req = build_declared_post_request((200, 10_240), "150", 150);

    let err = super::prepare(&mut req, &ccx).await.err().expect("the intersection is empty");
    assert_eq!(err.code(), &crate::error::S3ErrorCode::EntityTooSmall);
}

#[tokio::test]
async fn post_object_declaration_as_a_header_is_treated_as_unsigned() {
    use std::sync::Arc;

    let s3: Arc<dyn crate::s3_trait::S3> = Arc::new(post_policy_test_helpers::TestS3NoOp);
    let config = post_policy_test_helpers::create_test_config(10 * 1024);
    let auth = post_policy_test_helpers::create_test_auth();
    let ccx = post_policy_test_helpers::create_test_context(&s3, &config, &auth);
    let secret_key: crate::auth::SecretKey = "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY".into();
    let policy_json = &format!(
        r#"{{"expiration":"2030-01-01T00:00:00.000Z","conditions":[["content-length-range",0,10240],["eq","$Content-Type","text/plain"],{}]}}"#,
        post_policy_test_helpers::BASE_CONDITIONS,
    );
    let file_content = "a".repeat(150);
    let mut req = post_policy_test_helpers::build_post_object_request_with(
        policy_json,
        &file_content,
        &secret_key,
        true,
        &[],
        &[("x-s3s-payload-length", "150")],
    );

    let err = super::prepare(&mut req, &ccx)
        .await
        .err()
        .expect("a header is not a signed carrier on a form upload");
    assert_eq!(err.code(), &crate::error::S3ErrorCode::AccessDenied);
}
