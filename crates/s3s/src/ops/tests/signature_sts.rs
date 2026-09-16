// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

//! STS body hashing and its size limit, including end-to-end `SigV4` path tests.

use super::common::*;
use crate::ops::signature::*;

use crate::auth::SimpleAuth;
use crate::error::S3ErrorCode;
use crate::http::Body;
use crate::utils::crypto::hex_sha256;
use bytes::Bytes;
use hyper::header::HeaderMap;
use hyper::{Method, Uri, Version};

const STS_REGION: &str = "us-east-1";
const STS_SERVICE: &str = "sts";
const STS_HOST: &str = "sts.amazonaws.com";

/// Build a `SigV4` `Authorization` header signed for the `sts` service with the
/// given body hash as the payload checksum.  Does **not** include
/// `x-amz-content-sha256` so the STS-specific path in
/// `v4_check_header_auth` is exercised.
fn sts_authorization(method: &Method, body_hash: &str) -> String {
    let amz_date = s3s_sigv4::AmzDate::parse(AMZ_DATE).unwrap();
    let amz_date_str = amz_date.fmt_iso8601();
    let mut signed_headers: Vec<(&str, &str)> = vec![("host", STS_HOST), ("x-amz-date", amz_date_str.as_str())];
    signed_headers.sort_unstable_by(|a, b| a.0.cmp(b.0));
    let signed_header_names = signed_headers.iter().map(|(n, _)| *n).collect::<Vec<_>>().join(";");
    let query = &[] as &[(String, String)];
    let payload = s3s_sigv4::Payload::SingleChunk(body_hash);
    let canonical_request = s3s_sigv4::create_canonical_request(method.as_str(), "/", query, signed_headers, payload);
    let string_to_sign = s3s_sigv4::create_string_to_sign(&canonical_request, &amz_date, STS_REGION, STS_SERVICE);
    let signature = s3s_sigv4::calculate_signature(&string_to_sign, SECRET_KEY, &amz_date, STS_REGION, STS_SERVICE);
    format!(
        "AWS4-HMAC-SHA256 Credential={ACCESS_KEY}/{}/{STS_REGION}/{STS_SERVICE}/aws4_request, \
         SignedHeaders={signed_header_names}, Signature={}",
        amz_date.fmt_date(),
        signature.as_str()
    )
}

fn sts_headers(authorization: &str) -> HeaderMap {
    headers_from_slice(&[
        ("host", STS_HOST),
        ("x-amz-date", AMZ_DATE),
        ("authorization", authorization),
        // intentionally NO x-amz-content-sha256 — that is what triggers the STS body-hash path
    ])
}

#[test]
fn sts_body_hash_is_deterministic_hex() {
    let body_content = b"Action=AssumeRole&RoleArn=arn:aws:iam::123456789012:role/test-role&RoleSessionName=test-session";

    let hash = hex_sha256(body_content, str::to_owned);

    assert_eq!(hash.len(), 64);
    assert!(hash.chars().all(|c| c.is_ascii_hexdigit()));
    assert_eq!(hash, hex_sha256(body_content, str::to_owned));
}

#[test]
fn sts_max_body_size_is_in_reasonable_range() {
    // STS requests are typically small (under 2 KB for AssumeRole).
    // The limit must be large enough for real requests but small enough to
    // prevent trivial DoS via oversized bodies.
    const { assert!(MAX_STS_BODY_SIZE >= 2048, "limit must be large enough for typical STS requests") };
    const { assert!(MAX_STS_BODY_SIZE <= 65536, "limit must be small enough to bound memory use") };
}

/// A valid SigV4-signed STS request without `x-amz-content-sha256` must pass
/// `v4_check_header_auth` when the body is within `MAX_STS_BODY_SIZE`.
#[tokio::test]
async fn sts_header_auth_verifies_small_body() {
    let body_content = b"Action=AssumeRole&Version=2011-06-15";
    let body_hash = hex_sha256(body_content, str::to_owned);

    let method = Method::POST;
    let uri: Uri = format!("https://{STS_HOST}/").parse().unwrap();
    let authorization = sts_authorization(&method, &body_hash);
    let hs = sts_headers(&authorization);
    let mut body = Body::from(Bytes::from_static(body_content));

    let auth = SimpleAuth::from_single(ACCESS_KEY, SECRET_KEY);
    let config = test_config();

    let mut cx = SignatureContext {
        auth: Some(&auth),
        config: &config,
        req_version: Version::HTTP_11,
        req_method: &method,
        req_uri: &uri,
        req_body: &mut body,
        qs: None,
        hs: &hs,
        decoded_uri_path: "/",
        raw_uri_path: "/",
        vh_bucket: None,
        content_length: None,
        mime: None,
        decoded_content_length: None,
        transformed_body: None,
        multipart: None,
        trailing_headers: None,
    };

    let cred = cx.v4_check_header_auth().await.expect("STS small-body request must succeed");
    assert_eq!(cred.access_key, ACCESS_KEY);
}

/// A STS request whose body exceeds `MAX_STS_BODY_SIZE` must be rejected.
#[tokio::test]
async fn sts_header_auth_rejects_oversized_body() {
    // Use any body hash for the signature — the size check fires before signature
    // verification succeeds, so the exact hash value does not matter here.
    let placeholder_hash = "a".repeat(64);
    let method = Method::POST;
    let uri: Uri = format!("https://{STS_HOST}/").parse().unwrap();
    let authorization = sts_authorization(&method, &placeholder_hash);
    let hs = sts_headers(&authorization);
    let oversized = vec![b'x'; MAX_STS_BODY_SIZE + 1];
    let mut body = Body::from(Bytes::from(oversized));

    let auth = SimpleAuth::from_single(ACCESS_KEY, SECRET_KEY);
    let config = test_config();

    let mut cx = SignatureContext {
        auth: Some(&auth),
        config: &config,
        req_version: Version::HTTP_11,
        req_method: &method,
        req_uri: &uri,
        req_body: &mut body,
        qs: None,
        hs: &hs,
        decoded_uri_path: "/",
        raw_uri_path: "/",
        vh_bucket: None,
        content_length: None,
        mime: None,
        decoded_content_length: None,
        transformed_body: None,
        multipart: None,
        trailing_headers: None,
    };

    let err = cx
        .v4_check_header_auth()
        .await
        .expect_err("STS oversized body must be rejected");
    assert_eq!(err.code(), &S3ErrorCode::InvalidRequest, "oversized body should yield InvalidRequest");
    assert!(
        err.message().is_some_and(|m| m.contains("STS request body")),
        "error should mention STS request body: {:?}",
        err.message()
    );
}
