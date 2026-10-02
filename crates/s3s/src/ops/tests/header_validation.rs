// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

//! Request-header validation: `x-amz-checksum-<algorithm>` headers that name an
//! unknown algorithm, and an empty `x-amz-expected-bucket-owner`.

use super::common::*;

use crate::auth::SimpleAuth;
use crate::dto::ChecksumAlgorithm;
use crate::http::{Body, Request};

use hyper::header::HeaderName;
use hyper::{Method, StatusCode, Uri, Version};
use std::sync::Arc;
use std::sync::atomic::Ordering;

/// Sends a signed request with an empty body plus `extra_headers`.
///
/// The request declares `Content-Length: 0`, which is what the service requires
/// of a payload-bearing operation such as `PutObject`; without it the request
/// is refused before the headers under test are read.
fn signed_empty_request(method: Method, extra_headers: &[(&'static str, &'static str)]) -> Request {
    signed_request_with_header_lists(method, extra_headers, extra_headers)
}

/// Sends a signed request whose headers may repeat a name.
///
/// `sign_headers` is what the signature covers: a repeated header is signed as its
/// comma-joined value list, which is how the service canonicalizes it.
/// `send_headers` is what actually goes on the wire.
fn signed_request_with_header_lists(
    method: Method,
    sign_headers: &[(&'static str, &'static str)],
    send_headers: &[(&'static str, &'static str)],
) -> Request {
    let uri = "http://localhost/test-bucket/test-key.txt".parse::<Uri>().unwrap();
    let authorization = sign_request(&method, &uri, EMPTY_SHA256, sign_headers);

    let mut builder = hyper::Request::builder()
        .method(method)
        .version(Version::HTTP_11)
        .uri(uri.clone())
        .header(crate::header::HOST, uri.authority().unwrap().as_str())
        .header(crate::header::X_AMZ_CONTENT_SHA256, EMPTY_SHA256)
        .header(crate::header::X_AMZ_DATE, AMZ_DATE)
        .header(crate::header::AUTHORIZATION, authorization)
        .header(hyper::header::CONTENT_LENGTH, "0");

    for &(name, value) in send_headers {
        builder = builder.header(name, value);
    }

    Request::from(builder.body(Body::empty()).unwrap())
}

fn xml_body(response: &crate::http::Response) -> String {
    let bytes = response.body.bytes().expect("error responses carry an in-memory body");
    String::from_utf8(bytes.to_vec()).unwrap()
}

#[tokio::test]
async fn unknown_checksum_algorithm_header_is_rejected() {
    let test_s3 = Arc::new(TestS3::default());
    let s3: Arc<dyn crate::s3_trait::S3> = test_s3.clone();
    let config = test_config();
    let auth = SimpleAuth::from_single(ACCESS_KEY, SECRET_KEY);
    let ccx = test_context(&s3, &config, &auth);

    let mut req = signed_empty_request(Method::PUT, &[("x-amz-checksum-foo", "abc")]);
    let response = super::call(&mut req, &ccx).await.unwrap();

    assert_eq!(response.status, StatusCode::BAD_REQUEST);
    assert_eq!(
        xml_body(&response),
        concat!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>",
            "<Error><Code>InvalidRequest</Code>",
            "<Message>The algorithm type you specified in x-amz-checksum- header is invalid.</Message></Error>"
        )
    );
    assert_eq!(test_s3.put_object.load(Ordering::SeqCst), 0, "the handler must not be invoked");
}

#[tokio::test]
async fn known_checksum_algorithm_headers_are_accepted() {
    let test_s3 = Arc::new(TestS3::default());
    let s3: Arc<dyn crate::s3_trait::S3> = test_s3.clone();
    let config = test_config();
    let auth = SimpleAuth::from_single(ACCESS_KEY, SECRET_KEY);
    let ccx = test_context(&s3, &config, &auth);

    for header in ["x-amz-checksum-crc32", "x-amz-checksum-sha256", "x-amz-checksum-xxhash128"] {
        let mut req = signed_empty_request(Method::PUT, &[(header, "AAAAAA==")]);
        let response = super::call(&mut req, &ccx).await.unwrap();
        assert_eq!(response.status, StatusCode::OK, "{header} must be accepted");
    }

    assert_eq!(test_s3.put_object.load(Ordering::SeqCst), 3);
}

#[tokio::test]
async fn checksum_auxiliary_headers_are_not_algorithm_headers() {
    let test_s3 = Arc::new(TestS3::default());
    let s3: Arc<dyn crate::s3_trait::S3> = test_s3.clone();
    let config = test_config();
    let auth = SimpleAuth::from_single(ACCESS_KEY, SECRET_KEY);
    let ccx = test_context(&s3, &config, &auth);

    let cases: &[(&'static str, &'static str)] = &[
        ("x-amz-checksum-mode", "ENABLED"),
        ("x-amz-checksum-type", "COMPOSITE"),
        ("x-amz-checksum-algorithm", "SHA256"),
    ];

    for &(header, value) in cases {
        let mut req = signed_empty_request(Method::PUT, &[(header, value)]);
        let response = super::call(&mut req, &ccx).await.unwrap();
        assert_eq!(response.status, StatusCode::OK, "{header} is not an algorithm header");
    }
}

#[tokio::test]
async fn empty_expected_bucket_owner_is_rejected() {
    let test_s3 = Arc::new(TestS3::default());
    let s3: Arc<dyn crate::s3_trait::S3> = test_s3.clone();
    let config = test_config();
    let auth = SimpleAuth::from_single(ACCESS_KEY, SECRET_KEY);
    let ccx = test_context(&s3, &config, &auth);

    let mut req = signed_empty_request(Method::GET, &[("x-amz-expected-bucket-owner", "")]);
    let response = super::call(&mut req, &ccx).await.unwrap();

    assert_eq!(response.status, StatusCode::BAD_REQUEST);
    let body = xml_body(&response);
    assert!(body.contains("<Code>InvalidBucketOwnerAWSAccountID</Code>"), "{body}");
    assert!(
        body.contains("<Message>The value of the expected bucket owner parameter must be an AWS Account ID... []</Message>"),
        "{body}"
    );
    assert_eq!(test_s3.get_object.load(Ordering::SeqCst), 0, "the handler must not be invoked");
}

#[tokio::test]
async fn non_empty_expected_bucket_owner_is_accepted() {
    let test_s3 = Arc::new(TestS3::default());
    let s3: Arc<dyn crate::s3_trait::S3> = test_s3.clone();
    let config = test_config();
    let auth = SimpleAuth::from_single(ACCESS_KEY, SECRET_KEY);
    let ccx = test_context(&s3, &config, &auth);

    let mut req = signed_empty_request(Method::PUT, &[("x-amz-expected-bucket-owner", "123456789012")]);
    let response = super::call(&mut req, &ccx).await.unwrap();

    assert_eq!(response.status, StatusCode::OK);
    assert_eq!(test_s3.put_object.load(Ordering::SeqCst), 1);
}

#[test]
fn every_model_checksum_algorithm_has_a_known_header() {
    for algorithm in [
        ChecksumAlgorithm::CRC32,
        ChecksumAlgorithm::CRC32C,
        ChecksumAlgorithm::CRC64NVME,
        ChecksumAlgorithm::MD5,
        ChecksumAlgorithm::SHA1,
        ChecksumAlgorithm::SHA256,
        ChecksumAlgorithm::SHA512,
        ChecksumAlgorithm::XXHASH128,
        ChecksumAlgorithm::XXHASH3,
        ChecksumAlgorithm::XXHASH64,
    ] {
        let name = HeaderName::from_bytes(format!("x-amz-checksum-{}", algorithm.to_ascii_lowercase()).as_bytes())
            .expect("algorithm header name");

        let req = Request::from(hyper::Request::builder().header(name, "abc").body(Body::empty()).unwrap());

        crate::http::validate_checksum_headers(&req).expect(algorithm);
    }
}

#[test]
fn unknown_checksum_algorithm_header_fails_the_validator() {
    let req = Request::from(
        hyper::Request::builder()
            .header("x-amz-checksum-foo", "abc")
            .body(Body::empty())
            .unwrap(),
    );

    assert!(crate::http::validate_checksum_headers(&req).is_err());
}

#[tokio::test]
async fn a_repeated_checksum_header_is_rejected() {
    let test_s3 = Arc::new(TestS3::default());
    let s3: Arc<dyn crate::s3_trait::S3> = test_s3.clone();
    let config = test_config();
    let auth = SimpleAuth::from_single(ACCESS_KEY, SECRET_KEY);
    let ccx = test_context(&s3, &config, &auth);

    let mut req = signed_request_with_header_lists(
        Method::PUT,
        &[("x-amz-checksum-crc32", "AAAAAA==,AAAAAA==")],
        &[("x-amz-checksum-crc32", "AAAAAA=="), ("x-amz-checksum-crc32", "AAAAAA==")],
    );
    let response = super::call(&mut req, &ccx).await.unwrap();

    assert_eq!(response.status, StatusCode::BAD_REQUEST);
    let body = xml_body(&response);
    assert!(body.contains("<Code>InvalidArgument</Code>"), "{body}");
    assert!(body.contains("<Message>Only one value may be specified.</Message>"), "{body}");
    assert_eq!(test_s3.put_object.load(Ordering::SeqCst), 0, "the handler must not be invoked");
}

#[tokio::test]
async fn a_repeated_auxiliary_checksum_header_is_rejected() {
    let test_s3 = Arc::new(TestS3::default());
    let s3: Arc<dyn crate::s3_trait::S3> = test_s3.clone();
    let config = test_config();
    let auth = SimpleAuth::from_single(ACCESS_KEY, SECRET_KEY);
    let ccx = test_context(&s3, &config, &auth);

    let mut req = signed_request_with_header_lists(
        Method::PUT,
        &[("x-amz-checksum-mode", "ENABLED,ENABLED")],
        &[("x-amz-checksum-mode", "ENABLED"), ("x-amz-checksum-mode", "ENABLED")],
    );
    let response = super::call(&mut req, &ccx).await.unwrap();

    assert_eq!(response.status, StatusCode::BAD_REQUEST);
    let body = xml_body(&response);
    assert!(body.contains("<Code>InvalidArgument</Code>"), "{body}");
    assert!(body.contains("<Message>Only one value may be specified.</Message>"), "{body}");
    assert_eq!(test_s3.put_object.load(Ordering::SeqCst), 0, "the handler must not be invoked");
}

#[tokio::test]
async fn two_checksum_headers_are_rejected() {
    let test_s3 = Arc::new(TestS3::default());
    let s3: Arc<dyn crate::s3_trait::S3> = test_s3.clone();
    let config = test_config();
    let auth = SimpleAuth::from_single(ACCESS_KEY, SECRET_KEY);
    let ccx = test_context(&s3, &config, &auth);

    let mut req = signed_empty_request(
        Method::PUT,
        &[("x-amz-checksum-crc32", "AAAAAA=="), ("x-amz-checksum-sha256", "AAAAAA==")],
    );
    let response = super::call(&mut req, &ccx).await.unwrap();

    assert_eq!(response.status, StatusCode::BAD_REQUEST);
    let body = xml_body(&response);
    assert!(body.contains("<Code>InvalidRequest</Code>"), "{body}");
    assert!(
        body.contains("<Message>Expecting a single x-amz-checksum- header. Multiple checksum Types are not allowed.</Message>"),
        "{body}"
    );
    assert_eq!(test_s3.put_object.load(Ordering::SeqCst), 0, "the handler must not be invoked");
}

#[tokio::test]
async fn an_auxiliary_header_is_not_a_second_checksum_header() {
    let test_s3 = Arc::new(TestS3::default());
    let s3: Arc<dyn crate::s3_trait::S3> = test_s3.clone();
    let config = test_config();
    let auth = SimpleAuth::from_single(ACCESS_KEY, SECRET_KEY);
    let ccx = test_context(&s3, &config, &auth);

    let mut req = signed_empty_request(
        Method::PUT,
        &[("x-amz-checksum-algorithm", "CRC32"), ("x-amz-checksum-crc32", "AAAAAA==")],
    );
    let response = super::call(&mut req, &ccx).await.unwrap();

    assert_eq!(response.status, StatusCode::OK);
    assert_eq!(test_s3.put_object.load(Ordering::SeqCst), 1);
}
