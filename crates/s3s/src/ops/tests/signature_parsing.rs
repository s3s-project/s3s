// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

//! Signature header parsing: authorization headers, `x-amz-content-sha256` and the
//! signed-header list.

use super::common::*;
use crate::ops::signature::*;

use crate::error::S3ErrorCode;
use hyper::HeaderMap;
use hyper::header::{HeaderName, HeaderValue};
use s3s_sigv4::AmzContentSha256;

#[test]
fn sig_v2_headers_rejects_non_utf8_amz_header() {
    let mut headers = HeaderMap::new();
    headers.insert(
        HeaderName::from_static("x-amz-meta-name"),
        HeaderValue::from_bytes(b"JULI\xC1N").expect("valid opaque header value"),
    );

    let err = sig_v2_headers(&headers).expect_err("non-UTF-8 x-amz headers must fail signature verification");

    assert_eq!(err.code(), &S3ErrorCode::SignatureDoesNotMatch);
    assert_eq!(err.message(), Some("invalid header: x-amz-meta-name"));
}

#[test]
fn extract_authorization_v4_rejects_invalid_signature_with_signature_error() {
    let mut headers = HeaderMap::new();
    headers.insert(
        HeaderName::from_static("authorization"),
        HeaderValue::from_static(
            "AWS4-HMAC-SHA256 Credential=AKIAIOSFODNN7EXAMPLE/20130524/us-east-1/s3/aws4_request, SignedHeaders=host, Signature=not-a-real-signature",
        ),
    );

    let err = extract_authorization_v4(&headers).expect_err("non-canonical signature must be rejected");

    // The error must stay a signature error, not a malformed-header error.
    assert_eq!(err.code(), &S3ErrorCode::SignatureDoesNotMatch);
}

#[test]
fn extract_authorization_v4_rejects_malformed_header_with_invalid_request() {
    let mut headers = HeaderMap::new();
    headers.insert(
        HeaderName::from_static("authorization"),
        HeaderValue::from_static("AWS4-HMAC-SHA256 garbage"),
    );

    let err = extract_authorization_v4(&headers).expect_err("malformed authorization header must be rejected");
    assert_eq!(err.code(), &S3ErrorCode::InvalidRequest);
}

#[test]
fn sig_v2_headers_skips_non_utf8_non_amz_header() {
    let mut headers = HeaderMap::new();
    headers.insert(
        HeaderName::from_static("content-type"),
        HeaderValue::from_bytes(b"\xC1").expect("valid opaque header value"),
    );
    headers.insert(
        HeaderName::from_static("date"),
        HeaderValue::from_static("Fri, 24 Jan 2030 12:00:00 +0000"),
    );

    let pairs = sig_v2_headers(&headers).expect("non-x-amz non-UTF-8 headers are skipped");
    assert_eq!(pairs.len(), 1);
    assert_eq!(pairs[0], ("date", "Fri, 24 Jan 2030 12:00:00 +0000"));
}

#[test]
fn test_extract_amz_content_sha256_missing() {
    // Test that extract_amz_content_sha256 returns None when header is missing
    let headers = headers_from_slice(&[("host", "example.s3.amazonaws.com"), ("x-amz-date", "20130524T000000Z")]);
    let result = extract_amz_content_sha256(&headers).unwrap();
    assert!(result.is_none());
}

#[test]
fn test_extract_amz_content_sha256_present() {
    // Test that extract_amz_content_sha256 returns Some when header is present
    let headers = headers_from_slice(&[
        ("host", "example.s3.amazonaws.com"),
        ("x-amz-content-sha256", "UNSIGNED-PAYLOAD"),
        ("x-amz-date", "20130524T000000Z"),
    ]);
    let result = extract_amz_content_sha256(&headers).unwrap();
    assert!(result.is_some());
    assert_eq!(result.unwrap(), AmzContentSha256::UnsignedPayload);
}

#[test]
fn test_extract_amz_content_sha256_invalid() {
    // Test that extract_amz_content_sha256 returns error for invalid header value
    let headers = headers_from_slice(&[
        ("host", "example.s3.amazonaws.com"),
        ("x-amz-content-sha256", "INVALID-VALUE"),
        ("x-amz-date", "20130524T000000Z"),
    ]);
    let result = extract_amz_content_sha256(&headers);
    assert!(result.is_err());
    let err = result.unwrap_err();
    assert!(err.message().unwrap().contains("x-amz-content-sha256"));
}

#[test]
fn collect_signed_headers_preserves_duplicate_arrival_order() {
    let headers = headers_from_slice(&[
        ("host", "example.s3.amazonaws.com"),
        ("x-amz-meta-reviewer", "joe@example.com"),
        ("x-amz-meta-reviewer", "jane@example.com"),
    ]);
    let signed_headers = ["host", "x-amz-meta-reviewer"];

    let collected = collect_signed_headers(&headers, &signed_headers, |_| None).unwrap();

    assert_eq!(
        collected.as_slice(),
        [
            ("host", "example.s3.amazonaws.com"),
            ("x-amz-meta-reviewer", "joe@example.com"),
            ("x-amz-meta-reviewer", "jane@example.com"),
        ]
    );
}

#[test]
fn collect_signed_headers_rejects_missing_header() {
    let headers = headers_from_slice(&[("host", "example.s3.amazonaws.com")]);
    let signed_headers = ["host", "x-amz-meta-missing"];

    let err = collect_signed_headers(&headers, &signed_headers, |_| None).expect_err("missing signed header must fail");

    assert_eq!(err.code(), &S3ErrorCode::SignatureDoesNotMatch);
    assert_eq!(err.message(), Some("missing signed header: x-amz-meta-missing"));
}

#[test]
fn collect_signed_headers_rejects_non_utf8_values() {
    let mut headers = HeaderMap::new();
    headers.insert(
        HeaderName::from_static("x-amz-meta-name"),
        HeaderValue::from_bytes(b"JULI\xC1N").expect("valid opaque header value"),
    );
    let signed_headers = ["x-amz-meta-name"];

    let err = collect_signed_headers(&headers, &signed_headers, |_| Some("example.com"))
        .expect_err("non-UTF-8 value must not be signed");

    assert_eq!(err.code(), &S3ErrorCode::SignatureDoesNotMatch);
    assert_eq!(err.message(), Some("invalid signed header: x-amz-meta-name"));
}

#[test]
fn collect_signed_headers_uses_missing_header_fallback() {
    let headers = HeaderMap::new();
    let signed_headers = ["host"];

    let collected = collect_signed_headers(&headers, &signed_headers, |name| (name == "host").then_some("example.com")).unwrap();

    assert_eq!(collected.as_slice(), [("host", "example.com")]);
}
