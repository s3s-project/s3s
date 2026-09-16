// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

//! Header authentication (`Authorization`), v2 and v4, including the raw-path fallback
//! and the signed-host rules.

use super::common::*;
use crate::ops::signature::*;

use crate::auth::SecretKey;
use crate::auth::signature::Signature;
use crate::error::S3ErrorCode;
use crate::http::Body;
use hyper::{Method, Uri};
use s3s_sigv2::AuthorizationV2;
use s3s_sigv4::AmzDate;

#[test]
fn raw_path_fallback_rejects_missing_or_mismatched_signatures() {
    let secret_key: SecretKey = "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY".into();
    let amz_date = AmzDate::parse("20130524T000000Z").unwrap();
    let method = Method::GET;
    let headers = [
        ("host", "s3.amazonaws.com"),
        ("x-amz-content-sha256", "UNSIGNED-PAYLOAD"),
        ("x-amz-date", "20130524T000000Z"),
    ];

    let canonical_request = s3s_sigv4::create_canonical_request(
        method.as_str(),
        "/test-bucket/path",
        &[] as &[(&str, &str)],
        headers,
        s3s_sigv4::Payload::Unsigned,
    );
    let verifier = SignatureVerificationContext::new(
        Signature::from_hex("0000000000000000000000000000000000000000000000000000000000000000").unwrap(),
        "/test-bucket/path",
        &secret_key,
        &amz_date,
        "us-east-1",
        "s3",
    );
    let err = verifier
        .verify_with_raw_path_fallback(&canonical_request, || panic!("raw fallback should not be attempted"))
        .expect_err("signature mismatch without raw reserved characters should be rejected");
    assert_eq!(err.code(), &S3ErrorCode::SignatureDoesNotMatch);

    let canonical_request = s3s_sigv4::create_canonical_request(
        method.as_str(),
        "/test-bucket/path=",
        &[] as &[(&str, &str)],
        headers,
        s3s_sigv4::Payload::Unsigned,
    );
    let verifier = SignatureVerificationContext::new(
        Signature::from_hex("0000000000000000000000000000000000000000000000000000000000000000").unwrap(),
        "/test-bucket/path=",
        &secret_key,
        &amz_date,
        "us-east-1",
        "s3",
    );
    let err = verifier
        .verify_with_raw_path_fallback(&canonical_request, || {
            s3s_sigv4::create_canonical_request_with_raw_uri_path(
                method.as_str(),
                "/test-bucket/path=",
                &[] as &[(&str, &str)],
                headers,
                s3s_sigv4::Payload::Unsigned,
            )
        })
        .expect_err("raw fallback signature mismatch should be rejected");
    assert_eq!(err.code(), &S3ErrorCode::SignatureDoesNotMatch);
}

#[tokio::test]
async fn sig_v2_header_auth_rejected_when_disabled() {
    let config = sig_v2_test_config(false);
    let method = Method::GET;
    let uri = Uri::from_static("https://s3.amazonaws.com/test.txt");
    let headers = headers_from_slice(&[("authorization", "AWS AKIAIOSFODNN7EXAMPLE:qgk2+6Sv9/oM7G3qLEjTH1a1l1g=")]);
    let mut body = Body::empty();
    let mut cx = sig_v2_test_context(&config, None, &method, &uri, &mut body, None, &headers, None);

    let err = cx
        .v2_check()
        .await
        .expect("v2 header auth must be detected")
        .expect_err("SigV2 must be rejected when disabled");
    assert_eq!(err.code(), &S3ErrorCode::AccessDenied);
}

#[tokio::test]
async fn sig_v2_passes_gate_when_enabled() {
    let config = sig_v2_test_config(true);
    let method = Method::GET;
    let uri = Uri::from_static("https://s3.amazonaws.com/test.txt");

    // When SigV2 is explicitly enabled, the gate passes and the request proceeds to
    // signature verification; without an auth provider it fails at the auth
    // lookup with NotImplemented, not AccessDenied. The date must be fresh
    // to pass the freshness check first.
    let date = fmt_rfc1123(time::OffsetDateTime::now_utc());
    let headers = headers_from_slice(&[
        ("authorization", "AWS AKIAIOSFODNN7EXAMPLE:qgk2+6Sv9/oM7G3qLEjTH1a1l1g="),
        ("date", &date),
    ]);
    let mut body = Body::empty();
    let mut cx = sig_v2_test_context(&config, None, &method, &uri, &mut body, None, &headers, None);

    let err = cx
        .v2_check()
        .await
        .expect("v2 header auth must be detected")
        .expect_err("header auth without provider should fail at auth lookup");
    assert_eq!(err.code(), &S3ErrorCode::NotImplemented);
}

#[tokio::test]
async fn sig_v2_header_auth_accepts_fresh_date() {
    use crate::auth::SimpleAuth;

    let access_key = "AKIAIOSFODNN7EXAMPLE";
    let secret_key: crate::auth::SecretKey = "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY".into();
    let auth = SimpleAuth::from_single(access_key, secret_key.clone());

    let date = fmt_rfc1123(time::OffsetDateTime::now_utc());
    let signature = sig_v2_header_auth_signature(&secret_key, &date);
    let headers = headers_from_slice(&[("authorization", &format!("AWS {access_key}:{signature}")), ("date", &date)]);

    let config = sig_v2_test_config(true);
    let method = Method::GET;
    let uri = Uri::from_static("https://s3.amazonaws.com/test.txt");
    let mut body = Body::empty();
    let mut cx = sig_v2_test_context(&config, Some(&auth), &method, &uri, &mut body, None, &headers, None);

    let cred = cx
        .v2_check()
        .await
        .expect("v2 header auth must be detected")
        .expect("a fresh date with a valid signature must pass");
    assert_eq!(cred.access_key, access_key);
}

#[tokio::test]
async fn sig_v2_header_auth_rejects_stale_date() {
    use crate::auth::SimpleAuth;

    let access_key = "AKIAIOSFODNN7EXAMPLE";
    let secret_key: crate::auth::SecretKey = "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY".into();
    let auth = SimpleAuth::from_single(access_key, secret_key.clone());

    let stale = time::OffsetDateTime::now_utc() - time::Duration::hours(2);
    let date = fmt_rfc1123(stale);
    let signature = sig_v2_header_auth_signature(&secret_key, &date);
    let headers = headers_from_slice(&[("authorization", &format!("AWS {access_key}:{signature}")), ("date", &date)]);

    let config = sig_v2_test_config(true);
    let method = Method::GET;
    let uri = Uri::from_static("https://s3.amazonaws.com/test.txt");
    let mut body = Body::empty();
    let mut cx = sig_v2_test_context(&config, Some(&auth), &method, &uri, &mut body, None, &headers, None);

    let err = cx
        .v2_check()
        .await
        .expect("v2 header auth must be detected")
        .expect_err("a stale date must be rejected");
    assert_eq!(err.code(), &S3ErrorCode::RequestTimeTooSkewed);
}

#[tokio::test]
async fn sig_v2_rejects_an_appended_unsigned_amz_header() {
    use crate::auth::SimpleAuth;

    let access_key = "AKIAIOSFODNN7EXAMPLE";
    let secret_key: crate::auth::SecretKey = "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY".into();
    let auth = SimpleAuth::from_single(access_key, secret_key.clone());

    let date = fmt_rfc1123(time::OffsetDateTime::now_utc());
    let signature = sig_v2_header_auth_signature(&secret_key, &date);
    let authorization = format!("AWS {access_key}:{signature}");
    let config = sig_v2_test_config(true);

    // Control: the signed request passes, so the fixtures are valid.
    let headers = headers_from_slice(&[("authorization", &authorization), ("date", &date)]);
    let method = Method::GET;
    let uri = Uri::from_static("https://s3.amazonaws.com/test.txt");
    let mut body = Body::empty();
    let mut cx = sig_v2_test_context(&config, Some(&auth), &method, &uri, &mut body, None, &headers, None);
    cx.v2_check()
        .await
        .expect("v2 header auth must be detected")
        .expect("the signed request must pass");

    // The same signed request with one appended `x-amz-*` header must fail verification.
    let headers = headers_from_slice(&[
        ("authorization", &authorization),
        ("date", &date),
        ("x-amz-copy-source", "/source-bucket/source-key"),
    ]);
    let mut body = Body::empty();
    let mut cx = sig_v2_test_context(&config, Some(&auth), &method, &uri, &mut body, None, &headers, None);
    let err = cx
        .v2_check()
        .await
        .expect("v2 header auth must be detected")
        .expect_err("an appended x-amz-* header must break the SigV2 signature");
    assert_eq!(err.code(), &S3ErrorCode::SignatureDoesNotMatch);
}

#[tokio::test]
async fn sig_v2_header_auth_rejects_future_date() {
    use crate::auth::SimpleAuth;

    let access_key = "AKIAIOSFODNN7EXAMPLE";
    let secret_key: crate::auth::SecretKey = "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY".into();
    let auth = SimpleAuth::from_single(access_key, secret_key.clone());

    let future = time::OffsetDateTime::now_utc() + time::Duration::hours(2);
    let date = fmt_rfc1123(future);
    let signature = sig_v2_header_auth_signature(&secret_key, &date);
    let headers = headers_from_slice(&[("authorization", &format!("AWS {access_key}:{signature}")), ("date", &date)]);

    let config = sig_v2_test_config(true);
    let method = Method::GET;
    let uri = Uri::from_static("https://s3.amazonaws.com/test.txt");
    let mut body = Body::empty();
    let mut cx = sig_v2_test_context(&config, Some(&auth), &method, &uri, &mut body, None, &headers, None);

    let err = cx
        .v2_check()
        .await
        .expect("v2 header auth must be detected")
        .expect_err("a future date must be rejected");
    assert_eq!(err.code(), &S3ErrorCode::RequestTimeTooSkewed);
}

#[tokio::test]
async fn sig_v2_header_auth_rejects_invalid_date_format() {
    use crate::auth::SimpleAuth;

    let access_key = "AKIAIOSFODNN7EXAMPLE";
    let secret_key: crate::auth::SecretKey = "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY".into();
    let auth = SimpleAuth::from_single(access_key, secret_key.clone());

    let date = "not-a-date";
    let headers = headers_from_slice(&[("authorization", &format!("AWS {access_key}:whatever")), ("date", date)]);

    let config = sig_v2_test_config(true);
    let method = Method::GET;
    let uri = Uri::from_static("https://s3.amazonaws.com/test.txt");
    let mut body = Body::empty();
    let mut cx = sig_v2_test_context(&config, Some(&auth), &method, &uri, &mut body, None, &headers, None);

    let err = cx
        .v2_check()
        .await
        .expect("v2 header auth must be detected")
        .expect_err("an unparseable date must be rejected");
    assert_eq!(err.code(), &S3ErrorCode::InvalidRequest);
}

#[tokio::test]
async fn sig_v2_header_auth_prefers_x_amz_date() {
    use crate::auth::SimpleAuth;

    let access_key = "AKIAIOSFODNN7EXAMPLE";
    let secret_key: crate::auth::SecretKey = "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY".into();
    let auth = SimpleAuth::from_single(access_key, secret_key.clone());

    // x-amz-date is authoritative: a stale `Date` header must be ignored
    // when x-amz-date is present and fresh.
    let x_amz_date = fmt_current_amz_date(time::OffsetDateTime::now_utc());
    let stale_date = fmt_rfc1123(time::OffsetDateTime::now_utc() - time::Duration::hours(2));
    let string_to_sign = s3s_sigv2::create_string_to_sign(
        s3s_sigv2::Mode::HeaderAuth,
        "GET",
        "/test.txt",
        None,
        &[("x-amz-date", &x_amz_date)],
        None,
    );
    let signature = s3s_sigv2::calculate_signature(secret_key.expose(), &string_to_sign);
    let headers = headers_from_slice(&[
        ("authorization", &format!("AWS {access_key}:{signature}")),
        ("date", &stale_date),
        ("x-amz-date", &x_amz_date),
    ]);

    let config = sig_v2_test_config(true);
    let method = Method::GET;
    let uri = Uri::from_static("https://s3.amazonaws.com/test.txt");
    let mut body = Body::empty();
    let mut cx = sig_v2_test_context(&config, Some(&auth), &method, &uri, &mut body, None, &headers, None);

    let cred = cx
        .v2_check()
        .await
        .expect("v2 header auth must be detected")
        .expect("a fresh x-amz-date must win over a stale Date header");
    assert_eq!(cred.access_key, access_key);
}

fn fmt_rfc1123(odt: time::OffsetDateTime) -> String {
    use time::format_description::FormatItem;
    use time::macros::format_description;
    const RFC1123: &[FormatItem<'_>] =
        format_description!("[weekday repr:short], [day] [month repr:short] [year] [hour]:[minute]:[second] GMT");
    odt.format(RFC1123).expect("valid RFC 1123 date")
}

fn sig_v2_header_auth_signature(secret_key: &crate::auth::SecretKey, date: &str) -> String {
    let string_to_sign =
        s3s_sigv2::create_string_to_sign(s3s_sigv2::Mode::HeaderAuth, "GET", "/test.txt", None, &[("date", date)], None);
    s3s_sigv2::calculate_signature(secret_key.expose(), &string_to_sign)
}

#[tokio::test]
async fn v4_header_auth_with_port_in_signed_host() {
    // Header-auth counterpart of the signature-invariant nail above.
    use crate::auth::SecretKey;
    use crate::auth::SimpleAuth;
    use crate::config::{S3ConfigProvider, StaticConfigProvider};
    use std::sync::Arc;

    let access_key = "AKIAIOSFODNN7EXAMPLE";
    let secret_key: SecretKey = "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY".into();
    let auth = SimpleAuth::from_single(access_key, secret_key.clone());
    let config: Arc<dyn S3ConfigProvider> = Arc::new(StaticConfigProvider::default());

    let method = Method::GET;
    let uri = Uri::from_static("https://user.fs.example.com:19000/test.txt");
    let decoded_uri_path = "/test.txt";
    let raw_uri_path = "/test.txt";
    let amz_date = AmzDate::parse(&fmt_current_amz_date(time::OffsetDateTime::now_utc()))
        .expect("current time should produce a valid x-amz-date");
    let amz_date_str = amz_date.fmt_iso8601();
    let host = "user.fs.example.com:19000";
    let payload_hash = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
    let signed_headers = [
        ("host", host),
        ("x-amz-content-sha256", payload_hash),
        ("x-amz-date", amz_date_str.as_str()),
    ];
    let canonical_request = s3s_sigv4::create_canonical_request(
        method.as_str(),
        decoded_uri_path,
        &[] as &[(&str, &str)],
        signed_headers,
        s3s_sigv4::Payload::SingleChunk(payload_hash),
    );
    let string_to_sign = s3s_sigv4::create_string_to_sign(&canonical_request, &amz_date, "us-east-1", "s3");
    let signature = s3s_sigv4::calculate_signature(&string_to_sign, secret_key.expose(), &amz_date, "us-east-1", "s3");
    let authorization = format!(
        "AWS4-HMAC-SHA256 Credential={access_key}/{}/us-east-1/s3/aws4_request, \
         SignedHeaders=host;x-amz-content-sha256;x-amz-date, Signature={}",
        amz_date.fmt_date(),
        signature.as_str(),
    );

    let headers = headers_from_slice(&[
        ("host", host),
        ("x-amz-content-sha256", payload_hash),
        ("x-amz-date", amz_date_str.as_str()),
        ("authorization", authorization.as_str()),
    ]);
    let mut body = Body::empty();
    let mut cx = SignatureContext {
        auth: Some(&auth),
        config: &config,
        req_version: ::http::Version::HTTP_11,
        req_method: &method,
        req_uri: &uri,
        req_body: &mut body,
        qs: None,
        hs: &headers,
        decoded_uri_path,
        raw_uri_path,
        vh_bucket: Some("user"),
        content_length: None,
        mime: None,
        decoded_content_length: None,
        transformed_body: None,
        multipart: None,
        trailing_headers: None,
    };

    let cred = cx
        .v4_check()
        .await
        .expect("header auth must be detected")
        .expect("the raw Host value with its port must be used for verification");
    assert_eq!(cred.access_key, access_key);
}

#[tokio::test]
async fn v4_header_auth_rejects_wrong_region() {
    use crate::config::{S3Config, S3ConfigProvider, StaticConfigProvider};
    use std::sync::Arc;

    let access_key = "AKIAIOSFODNN7EXAMPLE";
    let auth = crate::ops::tests::NeverGetSecretKeyAuth;
    let s3_config = S3Config {
        presigned_url_max_skew_time_secs: u32::MAX,
        expected_region: Some("us-west-2".parse().expect("valid test region")),
        ..Default::default()
    };
    let config: Arc<dyn S3ConfigProvider> = Arc::new(StaticConfigProvider::new(Arc::new(s3_config)));

    let authorization = format!(
        "AWS4-HMAC-SHA256 Credential={access_key}/20130524/us-east-1/s3/aws4_request, SignedHeaders=host;x-amz-content-sha256;x-amz-date, Signature=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
    );
    let headers = headers_from_slice(&[
        ("authorization", authorization.as_str()),
        ("host", "s3.amazonaws.com"),
        ("x-amz-content-sha256", "UNSIGNED-PAYLOAD"),
        ("x-amz-date", "20130524T000000Z"),
    ]);
    let method = Method::GET;
    let uri = Uri::from_static("https://s3.amazonaws.com/test.txt");
    let mut body = Body::empty();
    let mut cx = SignatureContext {
        auth: Some(&auth),
        config: &config,
        req_version: ::http::Version::HTTP_11,
        req_method: &method,
        req_uri: &uri,
        req_body: &mut body,
        qs: None,
        hs: &headers,
        decoded_uri_path: "/test.txt",
        raw_uri_path: "/test.txt",
        vh_bucket: None,
        content_length: Some(0),
        mime: None,
        decoded_content_length: None,
        transformed_body: None,
        multipart: None,
        trailing_headers: None,
    };

    let err = cx
        .v4_check_header_auth()
        .await
        .expect_err("header signature for another region should be rejected");
    assert_eq!(err.code(), &S3ErrorCode::AuthorizationHeaderMalformed);
}

#[tokio::test]
async fn v4_header_auth_rejects_unsupported_algorithm() {
    use crate::config::{S3Config, S3ConfigProvider, StaticConfigProvider};
    use std::sync::Arc;

    let access_key = "AKIAIOSFODNN7EXAMPLE";
    let auth = crate::ops::tests::NeverGetSecretKeyAuth;
    let config: Arc<dyn S3ConfigProvider> = Arc::new(StaticConfigProvider::new(Arc::new(S3Config::default())));

    for algorithm in ["OTHER", "aws4-hmac-sha256", "AWS4-ECDSA-P256-SHA256"] {
        let authorization = format!(
            "{algorithm} Credential={access_key}/20130524/us-east-1/s3/aws4_request, SignedHeaders=host;x-amz-content-sha256;x-amz-date, Signature=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
        );
        let headers = headers_from_slice(&[
            ("authorization", authorization.as_str()),
            ("host", "s3.amazonaws.com"),
            ("x-amz-content-sha256", "UNSIGNED-PAYLOAD"),
            ("x-amz-date", "20130524T000000Z"),
        ]);
        let method = Method::GET;
        let uri = Uri::from_static("https://s3.amazonaws.com/test.txt");
        let mut body = Body::empty();
        let mut cx = SignatureContext {
            auth: Some(&auth),
            config: &config,
            req_version: ::http::Version::HTTP_11,
            req_method: &method,
            req_uri: &uri,
            req_body: &mut body,
            qs: None,
            hs: &headers,
            decoded_uri_path: "/test.txt",
            raw_uri_path: "/test.txt",
            vh_bucket: None,
            content_length: Some(0),
            mime: None,
            decoded_content_length: None,
            transformed_body: None,
            multipart: None,
            trailing_headers: None,
        };

        let err = cx
            .v4_check_header_auth()
            .await
            .expect_err("header auth with a non-SigV4 algorithm token should be rejected");
        assert_eq!(err.code(), &S3ErrorCode::NotImplemented, "algorithm {algorithm}");
    }
}

#[tokio::test]
async fn v4_header_auth_accepts_configured_service() {
    use crate::auth::SecretKey;
    use crate::auth::SimpleAuth;
    use crate::config::{S3Config, S3ConfigProvider, StaticConfigProvider};
    use std::sync::Arc;

    let access_key = "AKIAIOSFODNN7EXAMPLE";
    let secret_key: SecretKey = "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY".into();
    let auth = SimpleAuth::from_single(access_key, secret_key.clone());
    let s3_config = S3Config {
        presigned_url_max_skew_time_secs: u32::MAX,
        expected_region: Some("us-east-1".parse().expect("valid test region")),
        sig_v4_allowed_services: vec!["s3".to_owned(), "sts".to_owned(), "s3tables".to_owned()],
        ..Default::default()
    };
    let config: Arc<dyn S3ConfigProvider> = Arc::new(StaticConfigProvider::new(Arc::new(s3_config)));

    let method = Method::GET;
    let uri = Uri::from_static("https://s3.amazonaws.com/test.txt");
    let decoded_uri_path = "/test.txt";
    let raw_uri_path = "/test.txt";
    let amz_date = AmzDate::parse("20130524T000000Z").unwrap();
    let headers_for_signing = [
        ("host", "s3.amazonaws.com"),
        ("x-amz-content-sha256", "UNSIGNED-PAYLOAD"),
        ("x-amz-date", "20130524T000000Z"),
    ];
    let canonical_request = s3s_sigv4::create_canonical_request(
        method.as_str(),
        decoded_uri_path,
        &[] as &[(&str, &str)],
        headers_for_signing,
        s3s_sigv4::Payload::Unsigned,
    );
    let string_to_sign = s3s_sigv4::create_string_to_sign(&canonical_request, &amz_date, "us-east-1", "s3tables");
    let signature = s3s_sigv4::calculate_signature(&string_to_sign, secret_key.expose(), &amz_date, "us-east-1", "s3tables");
    let authorization = format!(
        "AWS4-HMAC-SHA256 Credential={access_key}/20130524/us-east-1/s3tables/aws4_request, SignedHeaders=host;x-amz-content-sha256;x-amz-date, Signature={}",
        signature.as_str(),
    );
    let headers = headers_from_slice(&[
        ("authorization", authorization.as_str()),
        ("host", "s3.amazonaws.com"),
        ("x-amz-content-sha256", "UNSIGNED-PAYLOAD"),
        ("x-amz-date", "20130524T000000Z"),
    ]);

    let mut body = Body::empty();
    let mut cx = SignatureContext {
        auth: Some(&auth),
        config: &config,
        req_version: ::http::Version::HTTP_11,
        req_method: &method,
        req_uri: &uri,
        req_body: &mut body,
        qs: None,
        hs: &headers,
        decoded_uri_path,
        raw_uri_path,
        vh_bucket: None,
        content_length: Some(0),
        mime: None,
        decoded_content_length: None,
        transformed_body: None,
        multipart: None,
        trailing_headers: None,
    };

    let cred = cx
        .v4_check_header_auth()
        .await
        .expect("configured SigV4 service should be accepted");
    assert_eq!(cred.access_key, access_key);
    assert_eq!(cred.service.as_deref(), Some("s3tables"));
}

#[tokio::test]
async fn v4_header_auth_accepts_standard_and_raw_uri_path_signatures() {
    use crate::auth::SecretKey;
    use crate::auth::SimpleAuth;
    use crate::config::{S3Config, S3ConfigProvider, StaticConfigProvider};
    use std::sync::Arc;

    let access_key = "AKIAIOSFODNN7EXAMPLE";
    let secret_key: SecretKey = "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY".into();
    let auth = SimpleAuth::from_single(access_key, secret_key.clone());
    let s3_config = S3Config {
        presigned_url_max_skew_time_secs: u32::MAX,
        expected_region: Some("us-east-1".parse().expect("valid test region")),
        ..Default::default()
    };
    let config: Arc<dyn S3ConfigProvider> = Arc::new(StaticConfigProvider::new(Arc::new(s3_config)));

    let method = Method::GET;
    let uri = Uri::from_static("https://s3.amazonaws.com/test-bucket/path/sitemap.xmlage=");
    let decoded_uri_path = "/test-bucket/path/sitemap.xmlage=";
    let raw_uri_path = "/test-bucket/path/sitemap.xmlage=";
    let amz_date = AmzDate::parse("20130524T000000Z").unwrap();
    let headers_for_signing = [
        ("host", "s3.amazonaws.com"),
        ("x-amz-content-sha256", "UNSIGNED-PAYLOAD"),
        ("x-amz-date", "20130524T000000Z"),
    ];

    let canonical_requests = [
        s3s_sigv4::create_canonical_request(
            method.as_str(),
            decoded_uri_path,
            &[] as &[(&str, &str)],
            headers_for_signing,
            s3s_sigv4::Payload::Unsigned,
        ),
        s3s_sigv4::create_canonical_request_with_raw_uri_path(
            method.as_str(),
            raw_uri_path,
            &[] as &[(&str, &str)],
            headers_for_signing,
            s3s_sigv4::Payload::Unsigned,
        ),
    ];

    for canonical_request in canonical_requests {
        let string_to_sign = s3s_sigv4::create_string_to_sign(&canonical_request, &amz_date, "us-east-1", "s3");
        let signature = s3s_sigv4::calculate_signature(&string_to_sign, secret_key.expose(), &amz_date, "us-east-1", "s3");
        let authorization = format!(
            "AWS4-HMAC-SHA256 Credential={access_key}/20130524/us-east-1/s3/aws4_request, SignedHeaders=host;x-amz-content-sha256;x-amz-date, Signature={}",
            signature.as_str(),
        );
        let headers = headers_from_slice(&[
            ("authorization", authorization.as_str()),
            ("host", "s3.amazonaws.com"),
            ("x-amz-content-sha256", "UNSIGNED-PAYLOAD"),
            ("x-amz-date", "20130524T000000Z"),
        ]);

        let mut body = Body::empty();
        let mut cx = SignatureContext {
            auth: Some(&auth),
            config: &config,
            req_version: ::http::Version::HTTP_11,
            req_method: &method,
            req_uri: &uri,
            req_body: &mut body,
            qs: None,
            hs: &headers,
            decoded_uri_path,
            raw_uri_path,
            vh_bucket: None,
            content_length: Some(0),
            mime: None,
            decoded_content_length: None,
            transformed_body: None,
            multipart: None,
            trailing_headers: None,
        };

        let cred = cx
            .v4_check_header_auth()
            .await
            .expect("valid SigV4 auth with a raw '=' URI path should succeed");
        assert_eq!(cred.access_key, access_key);
    }
}

#[tokio::test]
async fn v4_header_auth_accepts_rest_base64_content_sha256() {
    use crate::auth::SecretKey;
    use crate::auth::SimpleAuth;
    use crate::config::{S3Config, S3ConfigProvider, StaticConfigProvider};
    use bytes::Bytes;
    use std::sync::Arc;

    let access_key = "AKIAIOSFODNN7EXAMPLE";
    let secret_key: SecretKey = "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY".into();
    let auth = SimpleAuth::from_single(access_key, secret_key.clone());
    let s3_config = S3Config {
        presigned_url_max_skew_time_secs: u32::MAX,
        ..Default::default()
    };
    let config: Arc<dyn S3ConfigProvider> = Arc::new(StaticConfigProvider::new(Arc::new(s3_config)));

    let method = Method::POST;
    let uri = Uri::from_static("https://s3.amazonaws.com/iceberg/v1/catalog/commit");
    let path = "/iceberg/v1/catalog/commit";
    let body_data = b"hello world";
    let payload_hash = "b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9";
    let content_sha256 = "uU0nuZNNPgilLlLX2n2r+sSE7+N6U4DukIj3rOLvzek=";
    let amz_date = AmzDate::parse("20130524T000000Z").unwrap();
    let headers_for_signing = [
        ("host", "s3.amazonaws.com"),
        ("x-amz-content-sha256", content_sha256),
        ("x-amz-date", "20130524T000000Z"),
    ];
    let canonical_request = s3s_sigv4::create_canonical_request(
        method.as_str(),
        path,
        &[] as &[(&str, &str)],
        headers_for_signing,
        s3s_sigv4::Payload::SingleChunk(payload_hash),
    );
    let string_to_sign = s3s_sigv4::create_string_to_sign(&canonical_request, &amz_date, "us-east-1", "s3");
    let signature = s3s_sigv4::calculate_signature(&string_to_sign, secret_key.expose(), &amz_date, "us-east-1", "s3");
    let authorization = format!(
        "AWS4-HMAC-SHA256 Credential={access_key}/20130524/us-east-1/s3/aws4_request, SignedHeaders=host;x-amz-content-sha256;x-amz-date, Signature={}",
        signature.as_str(),
    );

    let headers = headers_from_slice(&[
        ("authorization", authorization.as_str()),
        ("host", "s3.amazonaws.com"),
        ("x-amz-content-sha256", content_sha256),
        ("x-amz-date", "20130524T000000Z"),
    ]);
    let mut body = Body::from(Bytes::from_static(body_data));
    let mut cx = SignatureContext {
        auth: Some(&auth),
        config: &config,
        req_version: ::http::Version::HTTP_11,
        req_method: &method,
        req_uri: &uri,
        req_body: &mut body,
        qs: None,
        hs: &headers,
        decoded_uri_path: path,
        raw_uri_path: path,
        vh_bucket: None,
        content_length: Some(body_data.len() as u64),
        mime: None,
        decoded_content_length: None,
        transformed_body: None,
        multipart: None,
        trailing_headers: None,
    };
    let cred = cx
        .v4_check_header_auth()
        .await
        .expect("valid REST SigV4 base64 content checksum should succeed");
    assert_eq!(cred.access_key, access_key);
    let stored = cx
        .req_body
        .store_all_limited(100)
        .await
        .expect("valid REST payload checksum should remain readable");
    assert_eq!(stored, &body_data[..]);
}

#[tokio::test]
async fn v4_header_auth_uses_http2_authority_for_signed_host() {
    use crate::auth::SecretKey;
    use crate::auth::SimpleAuth;
    use crate::config::{S3Config, S3ConfigProvider, StaticConfigProvider};
    use std::sync::Arc;

    let access_key = "AKIAIOSFODNN7EXAMPLE";
    let secret_key: SecretKey = "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY".into();
    let auth = SimpleAuth::from_single(access_key, secret_key.clone());
    let s3_config = S3Config {
        presigned_url_max_skew_time_secs: u32::MAX,
        ..Default::default()
    };
    let config: Arc<dyn S3ConfigProvider> = Arc::new(StaticConfigProvider::new(Arc::new(s3_config)));

    let method = Method::GET;
    let uri = Uri::from_static("https://s3.amazonaws.com/test-bucket/path/sitemap.xmlage=");
    let decoded_uri_path = "/test-bucket/path/sitemap.xmlage=";
    let raw_uri_path = "/test-bucket/path/sitemap.xmlage=";
    let amz_date = AmzDate::parse("20130524T000000Z").unwrap();
    let headers_for_signing = [
        ("host", "s3.amazonaws.com"),
        ("x-amz-content-sha256", "UNSIGNED-PAYLOAD"),
        ("x-amz-date", "20130524T000000Z"),
    ];
    let canonical_request = s3s_sigv4::create_canonical_request(
        method.as_str(),
        decoded_uri_path,
        &[] as &[(&str, &str)],
        headers_for_signing,
        s3s_sigv4::Payload::Unsigned,
    );
    let string_to_sign = s3s_sigv4::create_string_to_sign(&canonical_request, &amz_date, "us-east-1", "s3");
    let signature = s3s_sigv4::calculate_signature(&string_to_sign, secret_key.expose(), &amz_date, "us-east-1", "s3");
    let authorization = format!(
        "AWS4-HMAC-SHA256 Credential={access_key}/20130524/us-east-1/s3/aws4_request, SignedHeaders=host;x-amz-content-sha256;x-amz-date, Signature={}",
        signature.as_str(),
    );
    let headers = headers_from_slice(&[
        ("authorization", authorization.as_str()),
        ("x-amz-content-sha256", "UNSIGNED-PAYLOAD"),
        ("x-amz-date", "20130524T000000Z"),
    ]);

    let mut body = Body::empty();
    let mut cx = SignatureContext {
        auth: Some(&auth),
        config: &config,
        req_version: ::http::Version::HTTP_2,
        req_method: &method,
        req_uri: &uri,
        req_body: &mut body,
        qs: None,
        hs: &headers,
        decoded_uri_path,
        raw_uri_path,
        vh_bucket: None,
        content_length: Some(0),
        mime: None,
        decoded_content_length: None,
        transformed_body: None,
        multipart: None,
        trailing_headers: None,
    };

    let cred = cx
        .v4_check_header_auth()
        .await
        .expect("HTTP/2 authority should be used for a signed host header");
    assert_eq!(cred.access_key, access_key);
}

#[allow(clippy::too_many_lines)]
#[tokio::test]
async fn v4_header_auth_raw_uri_path_signature_seeds_streaming_body() {
    use crate::auth::SecretKey;
    use crate::auth::SimpleAuth;
    use crate::config::{S3Config, S3ConfigProvider, StaticConfigProvider};
    use bytes::Bytes;
    use std::sync::Arc;

    let access_key = "AKIAIOSFODNN7EXAMPLE";
    let secret_key: SecretKey = "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY".into();
    let auth = SimpleAuth::from_single(access_key, secret_key.clone());
    let s3_config = S3Config {
        presigned_url_max_skew_time_secs: u32::MAX,
        ..Default::default()
    };
    let config: Arc<dyn S3ConfigProvider> = Arc::new(StaticConfigProvider::new(Arc::new(s3_config)));

    let method = Method::PUT;
    let uri = Uri::from_static("https://s3.amazonaws.com/test-bucket/path/sitemap.xmlage=");
    let decoded_uri_path = "/test-bucket/path/sitemap.xmlage=";
    let raw_uri_path = "/test-bucket/path/sitemap.xmlage=";
    let amz_date = AmzDate::parse("20130524T000000Z").unwrap();
    let chunk_data = Bytes::from_static(b"hello");
    let decoded_content_length = chunk_data.len();
    let headers_for_signing = [
        ("host", "s3.amazonaws.com"),
        ("x-amz-content-sha256", "STREAMING-AWS4-HMAC-SHA256-PAYLOAD"),
        ("x-amz-date", "20130524T000000Z"),
        ("x-amz-decoded-content-length", "5"),
    ];

    let standard_canonical_request = s3s_sigv4::create_canonical_request(
        method.as_str(),
        decoded_uri_path,
        &[] as &[(&str, &str)],
        headers_for_signing,
        s3s_sigv4::Payload::MultipleChunks,
    );
    let raw_canonical_request = s3s_sigv4::create_canonical_request_with_raw_uri_path(
        method.as_str(),
        raw_uri_path,
        &[] as &[(&str, &str)],
        headers_for_signing,
        s3s_sigv4::Payload::MultipleChunks,
    );
    assert_ne!(standard_canonical_request, raw_canonical_request);

    let seed_string_to_sign = s3s_sigv4::create_string_to_sign(&raw_canonical_request, &amz_date, "us-east-1", "s3");
    let seed_signature = s3s_sigv4::calculate_signature(&seed_string_to_sign, secret_key.expose(), &amz_date, "us-east-1", "s3");

    let chunk_string_to_sign = s3s_sigv4::create_chunk_string_to_sign(
        &amz_date,
        "us-east-1",
        "s3",
        seed_signature.as_str(),
        std::slice::from_ref(&chunk_data),
    );
    let chunk_signature =
        s3s_sigv4::calculate_signature(&chunk_string_to_sign, secret_key.expose(), &amz_date, "us-east-1", "s3");
    let final_string_to_sign =
        s3s_sigv4::create_chunk_string_to_sign(&amz_date, "us-east-1", "s3", chunk_signature.as_str(), &[] as &[Vec<u8>]);
    let final_signature =
        s3s_sigv4::calculate_signature(&final_string_to_sign, secret_key.expose(), &amz_date, "us-east-1", "s3");

    let mut streaming_body = Vec::new();
    streaming_body
        .extend_from_slice(format!("{:x};chunk-signature={}\r\n", chunk_data.len(), chunk_signature.as_str()).as_bytes());
    streaming_body.extend_from_slice(&chunk_data);
    streaming_body.extend_from_slice(b"\r\n");
    streaming_body.extend_from_slice(format!("0;chunk-signature={}\r\n\r\n", final_signature.as_str()).as_bytes());
    let content_length = u64::try_from(streaming_body.len()).unwrap();

    let authorization = format!(
        "AWS4-HMAC-SHA256 Credential={access_key}/20130524/us-east-1/s3/aws4_request, SignedHeaders=host;x-amz-content-sha256;x-amz-date;x-amz-decoded-content-length, Signature={}",
        seed_signature.as_str()
    );
    let headers = headers_from_slice(&[
        ("authorization", authorization.as_str()),
        ("host", "s3.amazonaws.com"),
        ("x-amz-content-sha256", "STREAMING-AWS4-HMAC-SHA256-PAYLOAD"),
        ("x-amz-date", "20130524T000000Z"),
        ("x-amz-decoded-content-length", "5"),
    ]);

    let mut body = Body::from(Bytes::from(streaming_body));
    let mut cx = SignatureContext {
        auth: Some(&auth),
        config: &config,
        req_version: ::http::Version::HTTP_11,
        req_method: &method,
        req_uri: &uri,
        req_body: &mut body,
        qs: None,
        hs: &headers,
        decoded_uri_path,
        raw_uri_path,
        vh_bucket: None,
        content_length: Some(content_length),
        mime: None,
        decoded_content_length: Some(decoded_content_length),
        transformed_body: None,
        multipart: None,
        trailing_headers: None,
    };

    let cred = cx
        .v4_check_header_auth()
        .await
        .expect("valid streaming SigV4 auth with a raw '=' URI path should succeed");
    assert_eq!(cred.access_key, access_key);

    let mut transformed_body = cx.transformed_body.take().expect("streaming body should be transformed");
    let decoded_body = transformed_body
        .store_all_limited(decoded_content_length)
        .await
        .expect("raw-path seed signature should validate aws-chunked body");
    assert_eq!(decoded_body, chunk_data);
}

#[tokio::test]
async fn v2_header_auth_returns_no_region() {
    use crate::auth::SecretKey;
    use crate::config::{S3Config, S3ConfigProvider, StaticConfigProvider};
    use std::sync::Arc;

    let access_key = "AKIAIOSFODNN7EXAMPLE";
    let secret_key: SecretKey = "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY".into();
    let auth = crate::auth::SimpleAuth::from_single(access_key, secret_key.clone());
    let config = S3Config {
        enable_sig_v2: true,
        ..Default::default()
    };
    let config: Arc<dyn S3ConfigProvider> = Arc::new(StaticConfigProvider::new(Arc::new(config)));

    let date = fmt_rfc1123(time::OffsetDateTime::now_utc());
    let hs = headers_from_slice(&[("date", &date), ("host", "s3.amazonaws.com")]);

    let method = Method::GET;
    let uri = Uri::from_static("https://s3.amazonaws.com/test-bucket/test-key");
    let mut body = Body::empty();

    // Compute the expected signature using the same logic as the verification path.
    let headers = sig_v2_headers(&hs).expect("test headers are valid");
    let string_to_sign = s3s_sigv2::create_string_to_sign(
        s3s_sigv2::Mode::HeaderAuth,
        method.as_str(),
        "/test-bucket/test-key",
        None,
        &headers,
        None,
    );
    let signature = Signature::from_computed(s3s_sigv2::calculate_signature(secret_key.expose(), &string_to_sign));

    let auth_v2 = AuthorizationV2 {
        access_key,
        signature: signature.as_str(),
    };

    let mut cx = SignatureContext {
        auth: Some(&auth),
        config: &config,
        req_version: ::http::Version::HTTP_11,
        req_method: &method,
        req_uri: &uri,
        req_body: &mut body,
        qs: None,
        hs: &hs,
        decoded_uri_path: "/test-bucket/test-key",
        raw_uri_path: "/test-bucket/test-key",
        vh_bucket: None,
        content_length: None,
        mime: None,
        decoded_content_length: None,
        transformed_body: None,
        multipart: None,
        trailing_headers: None,
    };

    let cred = cx
        .v2_check_header_auth(auth_v2)
        .await
        .expect("valid SigV2 auth should succeed");
    assert_eq!(cred.region, None, "SigV2 carries no region");
    assert_eq!(cred.service.as_deref(), Some("s3"), "SigV2 service is always 's3'");
}

#[tokio::test]
async fn v4_header_auth_rejects_stale_request_time() {
    use crate::S3ErrorCode;
    use crate::auth::SecretKey;
    use crate::auth::SimpleAuth;
    use crate::config::{S3ConfigProvider, StaticConfigProvider};
    use std::sync::Arc;

    let access_key = "AKIAIOSFODNN7EXAMPLE";
    let secret_key: SecretKey = "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY".into();
    let auth = SimpleAuth::from_single(access_key, secret_key.clone());
    let config: Arc<dyn S3ConfigProvider> = Arc::new(StaticConfigProvider::default());

    let skew = time::Duration::seconds(i64::from(config.snapshot().presigned_url_max_skew_time_secs));
    let request_time = time::OffsetDateTime::now_utc() - skew - time::Duration::minutes(1);
    let amz_date_str = fmt_current_amz_date(request_time);
    let amz_date = AmzDate::parse(&amz_date_str).unwrap();

    let method = Method::GET;
    let uri = Uri::from_static("https://s3.amazonaws.com/test.txt");
    let headers_for_signing = [
        ("host", "s3.amazonaws.com"),
        ("x-amz-content-sha256", "UNSIGNED-PAYLOAD"),
        ("x-amz-date", amz_date_str.as_str()),
    ];
    let canonical_request = s3s_sigv4::create_canonical_request(
        method.as_str(),
        "/test.txt",
        &[] as &[(&str, &str)],
        headers_for_signing,
        s3s_sigv4::Payload::Unsigned,
    );
    let string_to_sign = s3s_sigv4::create_string_to_sign(&canonical_request, &amz_date, "us-east-1", "s3");
    let signature = s3s_sigv4::calculate_signature(&string_to_sign, secret_key.expose(), &amz_date, "us-east-1", "s3");
    let authorization = format!(
        "AWS4-HMAC-SHA256 Credential={access_key}/{}/us-east-1/s3/aws4_request, SignedHeaders=host;x-amz-content-sha256;x-amz-date, Signature={}",
        amz_date.fmt_date(),
        signature.as_str(),
    );

    let headers = headers_from_slice(&[
        ("authorization", authorization.as_str()),
        ("host", "s3.amazonaws.com"),
        ("x-amz-content-sha256", "UNSIGNED-PAYLOAD"),
        ("x-amz-date", amz_date_str.as_str()),
    ]);

    let mut body = Body::empty();
    let mut cx = SignatureContext {
        auth: Some(&auth),
        config: &config,
        req_version: ::http::Version::HTTP_11,
        req_method: &method,
        req_uri: &uri,
        req_body: &mut body,
        qs: None,
        hs: &headers,
        decoded_uri_path: "/test.txt",
        raw_uri_path: "/test.txt",
        vh_bucket: None,
        content_length: Some(0),
        mime: None,
        decoded_content_length: None,
        transformed_body: None,
        multipart: None,
        trailing_headers: None,
    };

    let err = cx
        .v4_check_header_auth()
        .await
        .expect_err("stale signed header request should be rejected");
    assert_eq!(err.code(), &S3ErrorCode::RequestTimeTooSkewed);
}
