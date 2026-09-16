// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

//! Request `Content-Length` handling: bodyless operations without a length, the
//! known-length backfill, and the opt-out switch.

use super::common::*;

use crate::auth::SimpleAuth;
use crate::config::{S3Config, S3ConfigProvider, StaticConfigProvider};
use crate::http::{Body, Request};
use crate::ops::Prepare;

use hyper::header::HeaderValue;
use hyper::{Method, StatusCode, Uri, Version};
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::Ordering;

impl TestS3 {
    fn total_bodyless_calls(&self) -> usize {
        self.get_object.load(Ordering::SeqCst)
            + self.head_object.load(Ordering::SeqCst)
            + self.delete_object.load(Ordering::SeqCst)
            + self.copy_object.load(Ordering::SeqCst)
            + self.upload_part_copy.load(Ordering::SeqCst)
    }
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

    for payload_sha256 in [EMPTY_SHA256, UNSIGNED_PAYLOAD] {
        for version in [Version::HTTP_11, Version::HTTP_2] {
            for case in &cases {
                let before = case.calls(&test_s3);
                let mut req = signed_request(case.method.clone(), version, case.uri, payload_sha256, case.extra_headers);
                assert!(req.headers.get(hyper::header::CONTENT_LENGTH).is_none());

                let response = super::call(&mut req, &ccx).await.unwrap();
                assert!(
                    response.status.is_success(),
                    "{} with {payload_sha256} over {version:?} should succeed without Content-Length, got {:?}",
                    case.name,
                    response.status
                );
                assert_eq!(case.calls(&test_s3), before + 1, "{} handler should be invoked", case.name);
            }
        }
    }

    assert_eq!(test_s3.total_bodyless_calls(), cases.len() * 4);
}

#[tokio::test]
async fn signed_upload_operations_still_reject_missing_content_length() {
    let test_s3 = Arc::new(TestS3::default());
    let s3: Arc<dyn crate::s3_trait::S3> = test_s3.clone();
    let config = test_config();
    let auth = SimpleAuth::from_single(ACCESS_KEY, SECRET_KEY);
    let ccx = test_context(&s3, &config, &auth);

    let cases = [
        ("PutObject", "http://localhost/test-bucket/test-key.txt"),
        ("UploadPart", "http://localhost/test-bucket/test-key.txt?partNumber=1&uploadId=upload-id"),
    ];

    for payload_sha256 in [EMPTY_SHA256, UNSIGNED_PAYLOAD] {
        for version in [Version::HTTP_11, Version::HTTP_2] {
            for (name, uri) in cases {
                let mut req = signed_request(Method::PUT, version, uri, payload_sha256, &[]);
                assert!(req.headers.get(hyper::header::CONTENT_LENGTH).is_none());

                let response = super::call(&mut req, &ccx).await.unwrap();
                assert_eq!(
                    response.status,
                    StatusCode::LENGTH_REQUIRED,
                    "{name} with {payload_sha256} over {version:?} must still require Content-Length"
                );
            }
        }
    }

    assert_eq!(test_s3.put_object.load(Ordering::SeqCst), 0);
    assert_eq!(test_s3.upload_part.load(Ordering::SeqCst), 0);
}

fn empty_body_signed_put(version: Version) -> Request {
    let uri = "http://localhost/test-bucket/test-key.txt".parse::<Uri>().unwrap();
    let authorization = sign_request(&Method::PUT, &uri, EMPTY_SHA256, &[]);
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
async fn ordinary_signed_put_ignores_unrelated_decoded_content_length() {
    // This request signs an ordinary empty payload, not an aws-chunked
    // stream. An unrelated header must not change its known payload length.
    // The unrelated header is deliberately not an `x-amz-*` one: an unsigned
    // `x-amz-*` header is rejected outright before routing, which is covered by
    // `unsigned_copy_source_is_rejected_on_header_signed_requests`.
    let recording = Arc::new(ContentLengthRecordingS3 {
        received: Mutex::new(None),
    });
    let s3: Arc<dyn crate::s3_trait::S3> = recording.clone();
    let config = test_config();
    let auth = SimpleAuth::from_single(ACCESS_KEY, SECRET_KEY);
    let ccx = test_context(&s3, &config, &auth);

    let mut req = empty_body_signed_put(Version::HTTP_11);
    req.headers
        .insert(hyper::header::CONTENT_ENCODING, HeaderValue::from_static("identity"));

    let response = super::call(&mut req, &ccx).await.unwrap();
    assert_eq!(response.status, StatusCode::OK);

    let (input_len, header_len) = recording.received.lock().unwrap().take().expect("put_object was called");
    assert_eq!(input_len, Some(0), "ordinary payload length must remain authoritative");
    assert_eq!(header_len, Some(0));
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

    let mut malformed = signed_request(
        Method::GET,
        Version::HTTP_11,
        "http://localhost/test-bucket/test-key.txt",
        EMPTY_SHA256,
        &[],
    );
    malformed
        .headers
        .insert(hyper::header::CONTENT_LENGTH, hyper::header::HeaderValue::from_static("not-a-number"));
    let response = super::call(&mut malformed, &ccx).await.unwrap();
    assert_eq!(response.status, StatusCode::BAD_REQUEST);

    let mut overflowing = signed_request(
        Method::GET,
        Version::HTTP_11,
        "http://localhost/test-bucket/test-key.txt",
        EMPTY_SHA256,
        &[],
    );
    overflowing.headers.insert(
        hyper::header::CONTENT_LENGTH,
        hyper::header::HeaderValue::from_static("184467440737095516160"),
    );
    let response = super::call(&mut overflowing, &ccx).await.unwrap();
    assert_eq!(response.status, StatusCode::BAD_REQUEST);

    let mut duplicate = signed_request(
        Method::GET,
        Version::HTTP_11,
        "http://localhost/test-bucket/test-key.txt",
        EMPTY_SHA256,
        &[],
    );
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

    let mut req = signed_request(
        Method::GET,
        Version::HTTP_2,
        "http://localhost/test-bucket/test-key.txt",
        EMPTY_SHA256,
        &[],
    );
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

    let mut req = signed_request(
        Method::GET,
        Version::HTTP_11,
        "http://localhost/test-bucket/test-key.txt",
        NON_EMPTY_SHA256,
        &[],
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
