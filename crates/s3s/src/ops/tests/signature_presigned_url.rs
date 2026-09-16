// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

//! Presigned URLs: expiry, signed service/region, ports and HTTP/2 authority.

use super::common::*;
use crate::ops::signature::*;

use crate::config::{S3Config, S3ConfigProvider};
use crate::error::S3ErrorCode;
use crate::http::{Body, OrderedQs};
use crate::utils::crypto::hex_sha256;
use hyper::{HeaderMap, Method, Uri};
use s3s_sigv4::AmzDate;
use std::sync::Arc;

fn presigned_query_fields(amz_date: &AmzDate, service: &str) -> Vec<(String, String)> {
    vec![
        ("X-Amz-Algorithm".to_owned(), "AWS4-HMAC-SHA256".to_owned()),
        (
            "X-Amz-Credential".to_owned(),
            format!("AKIAIOSFODNN7EXAMPLE/{}/us-east-1/{service}/aws4_request", amz_date.fmt_date()),
        ),
        ("X-Amz-Date".to_owned(), amz_date.fmt_iso8601().to_string()),
        ("X-Amz-Expires".to_owned(), "604800".to_owned()),
        ("X-Amz-SignedHeaders".to_owned(), "host".to_owned()),
    ]
}

#[tokio::test]
async fn v4_presigned_url_rejects_invalid_expires_as_authorization_query_error() {
    use crate::config::StaticConfigProvider;

    let qs = OrderedQs::parse(concat!(
        "X-Amz-Algorithm=AWS4-HMAC-SHA256",
        "&X-Amz-Credential=AKIAIOSFODNN7EXAMPLE%2F20130524%2Fus-east-1%2Fs3%2Faws4_request",
        "&X-Amz-Date=20130524T000000Z",
        "&X-Amz-Expires=604801",
        "&X-Amz-SignedHeaders=host",
        "&X-Amz-Signature=aeeed9bbccd4d02ee5c0109b86d86835f995330da4c265957d157751f604d404"
    ))
    .expect("query should parse");
    let config: Arc<dyn S3ConfigProvider> = Arc::new(StaticConfigProvider::default());
    let method = Method::GET;
    let uri = Uri::from_static("https://s3.amazonaws.com/test.txt");
    let headers = headers_from_slice(&[("authorization", "AWS4-HMAC-SHA256 Credential=invalid")]);
    let mut body = Body::empty();
    let mut cx = SignatureContext {
        auth: None,
        config: &config,
        req_version: ::http::Version::HTTP_11,
        req_method: &method,
        req_uri: &uri,
        req_body: &mut body,
        qs: Some(&qs),
        hs: &headers,
        decoded_uri_path: "/test.txt",
        raw_uri_path: "/test.txt",
        vh_bucket: None,
        content_length: None,
        mime: None,
        decoded_content_length: None,
        transformed_body: None,
        multipart: None,
        trailing_headers: None,
    };

    let err = cx
        .v4_check()
        .await
        .expect("X-Amz-Signature must take precedence over header auth")
        .expect_err("expiration beyond seven days must be rejected before authentication");
    assert_eq!(err.code(), &S3ErrorCode::AuthorizationQueryParametersError);
    assert_eq!(err.message(), Some("The authorization query parameters that you provided are not valid."));
    assert!(err.source().is_some(), "parse error must remain available as the source");
}

#[tokio::test]
async fn v4_presigned_url_accepts_expires_beyond_aws_default_when_configured() {
    use crate::config::{S3Config, S3ConfigProvider, StaticConfigProvider};

    let amz_date = AmzDate::parse(&fmt_current_amz_date(time::OffsetDateTime::now_utc()))
        .expect("current time should produce a valid x-amz-date");
    let mut query_strings = presigned_query_fields(&amz_date, "s3");
    query_strings[3] = ("X-Amz-Expires".to_owned(), "604801".to_owned());
    query_strings.push((
        "X-Amz-Signature".to_owned(),
        "aeeed9bbccd4d02ee5c0109b86d86835f995330da4c265957d157751f604d404".to_owned(),
    ));
    let qs = OrderedQs::from_vec_unchecked(query_strings);

    let s3_config = S3Config {
        presigned_url_max_skew_time_secs: u32::MAX,
        presigned_url_max_expires_secs: 700_000,
        ..Default::default()
    };
    let config: Arc<dyn S3ConfigProvider> = Arc::new(StaticConfigProvider::new(Arc::new(s3_config)));
    let method = Method::GET;
    let uri = Uri::from_static("https://s3.amazonaws.com/test.txt");
    let headers = headers_from_slice(&[("host", "s3.amazonaws.com")]);
    let mut body = Body::empty();
    let mut cx = SignatureContext {
        auth: None,
        config: &config,
        req_version: ::http::Version::HTTP_11,
        req_method: &method,
        req_uri: &uri,
        req_body: &mut body,
        qs: Some(&qs),
        hs: &headers,
        decoded_uri_path: "/test.txt",
        raw_uri_path: "/test.txt",
        vh_bucket: None,
        content_length: None,
        mime: None,
        decoded_content_length: None,
        transformed_body: None,
        multipart: None,
        trailing_headers: None,
    };

    let err = cx
        .v4_check()
        .await
        .expect("X-Amz-Signature must select presigned auth")
        .expect_err("missing auth provider should fail after presigned parsing succeeds");
    assert_eq!(err.code(), &S3ErrorCode::NotImplemented);
}

#[tokio::test]
async fn x_amz_expires_limit_applies_only_to_presigned_query_auth() {
    use crate::config::StaticConfigProvider;

    let qs = OrderedQs::parse("X-Amz-Expires=604801").expect("query should parse");
    let config: Arc<dyn S3ConfigProvider> = Arc::new(StaticConfigProvider::default());
    let method = Method::GET;
    let uri = Uri::from_static("https://s3.amazonaws.com/test.txt");
    let anonymous_headers = HeaderMap::new();
    let mut body = Body::empty();
    let mut anonymous = SignatureContext {
        auth: None,
        config: &config,
        req_version: ::http::Version::HTTP_11,
        req_method: &method,
        req_uri: &uri,
        req_body: &mut body,
        qs: Some(&qs),
        hs: &anonymous_headers,
        decoded_uri_path: "/test.txt",
        raw_uri_path: "/test.txt",
        vh_bucket: None,
        content_length: None,
        mime: None,
        decoded_content_length: None,
        transformed_body: None,
        multipart: None,
        trailing_headers: None,
    };
    assert!(
        anonymous.v4_check().await.is_none(),
        "an unsigned query must not be treated as presigned auth"
    );

    let header_auth_headers = headers_from_slice(&[("authorization", "AWS4-HMAC-SHA256 Credential=invalid")]);
    let mut body = Body::empty();
    let mut header_auth = SignatureContext {
        auth: None,
        config: &config,
        req_version: ::http::Version::HTTP_11,
        req_method: &method,
        req_uri: &uri,
        req_body: &mut body,
        qs: Some(&qs),
        hs: &header_auth_headers,
        decoded_uri_path: "/test.txt",
        raw_uri_path: "/test.txt",
        vh_bucket: None,
        content_length: None,
        mime: None,
        decoded_content_length: None,
        transformed_body: None,
        multipart: None,
        trailing_headers: None,
    };
    let err = header_auth
        .v4_check()
        .await
        .expect("authorization header must select header auth")
        .expect_err("malformed header auth should fail");
    assert_ne!(err.code(), &S3ErrorCode::AuthorizationQueryParametersError);
}

#[tokio::test]
async fn sig_v2_presigned_url_rejected_when_disabled() {
    let config = sig_v2_test_config(false);
    let method = Method::GET;
    let uri = Uri::from_static("https://s3.amazonaws.com/test.txt");
    let qs = OrderedQs::parse("AWSAccessKeyId=AKIAIOSFODNN7EXAMPLE&Signature=abc&Expires=1175139620").unwrap();
    let headers = HeaderMap::new();
    let mut body = Body::empty();
    let mut cx = sig_v2_test_context(&config, None, &method, &uri, &mut body, Some(&qs), &headers, None);

    let err = cx
        .v2_check()
        .await
        .expect("v2 presigned url must be detected")
        .expect_err("SigV2 must be rejected when disabled");
    assert_eq!(err.code(), &S3ErrorCode::AccessDenied);
}

#[tokio::test]
async fn v4_presigned_url_rejects_unknown_service() {
    use crate::S3ErrorCode;
    use crate::auth::SecretKey;
    use crate::config::{S3ConfigProvider, StaticConfigProvider};
    use std::sync::Arc;

    let amz_date = AmzDate::parse(&fmt_current_amz_date(time::OffsetDateTime::now_utc()))
        .expect("current time should produce a valid x-amz-date");
    let mut query_strings = presigned_query_fields(&amz_date, "custom-svc");
    query_strings.push((
        "X-Amz-Signature".to_owned(),
        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_owned(),
    ));
    let qs = OrderedQs::from_vec_unchecked(query_strings);

    let access_key = "AKIAIOSFODNN7EXAMPLE";
    let secret_key: SecretKey = "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY".into();
    let auth = crate::auth::SimpleAuth::from_single(access_key, secret_key);
    let config: Arc<dyn S3ConfigProvider> = Arc::new(StaticConfigProvider::default());

    let method = Method::GET;
    let uri = Uri::from_static("https://s3.amazonaws.com/test.txt");
    let headers = HeaderMap::new();
    let mut body = Body::empty();

    let mut cx = SignatureContext {
        auth: Some(&auth),
        config: &config,
        req_version: ::http::Version::HTTP_11,
        req_method: &method,
        req_uri: &uri,
        req_body: &mut body,
        qs: Some(&qs),
        hs: &headers,
        decoded_uri_path: "/test.txt",
        raw_uri_path: "/test.txt",
        vh_bucket: None,
        content_length: None,
        mime: None,
        decoded_content_length: None,
        transformed_body: None,
        multipart: None,
        trailing_headers: None,
    };

    let err = cx
        .v4_check_presigned_url()
        .await
        .expect_err("unknown service must be rejected");
    assert_eq!(err.code(), &S3ErrorCode::NotImplemented);
}

#[tokio::test]
async fn v4_presigned_url_rejects_wrong_region() {
    use crate::config::{S3ConfigProvider, StaticConfigProvider};
    use std::sync::Arc;

    let qs = OrderedQs::parse(concat!(
        "X-Amz-Algorithm=AWS4-HMAC-SHA256",
        "&X-Amz-Credential=AKIAIOSFODNN7EXAMPLE%2F20130524%2Fus-east-1%2Fs3%2Faws4_request",
        "&X-Amz-Date=20130524T000000Z",
        "&X-Amz-Expires=3600",
        "&X-Amz-SignedHeaders=host",
        "&X-Amz-Signature=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
    ))
    .unwrap();

    let auth = crate::ops::tests::NeverGetSecretKeyAuth;
    let s3_config = S3Config {
        expected_region: Some("us-west-2".parse().expect("valid test region")),
        ..Default::default()
    };
    let config: Arc<dyn S3ConfigProvider> = Arc::new(StaticConfigProvider::new(Arc::new(s3_config)));

    let method = Method::GET;
    let uri = Uri::from_static("https://s3.amazonaws.com/test.txt");
    let headers = headers_from_slice(&[("host", "s3.amazonaws.com")]);
    let mut body = Body::empty();
    let mut cx = SignatureContext {
        auth: Some(&auth),
        config: &config,
        req_version: ::http::Version::HTTP_11,
        req_method: &method,
        req_uri: &uri,
        req_body: &mut body,
        qs: Some(&qs),
        hs: &headers,
        decoded_uri_path: "/test.txt",
        raw_uri_path: "/test.txt",
        vh_bucket: None,
        content_length: None,
        mime: None,
        decoded_content_length: None,
        transformed_body: None,
        multipart: None,
        trailing_headers: None,
    };

    let err = cx
        .v4_check_presigned_url()
        .await
        .expect_err("presigned URL for another region should be rejected");
    assert_eq!(err.code(), &S3ErrorCode::AuthorizationHeaderMalformed);
}

#[tokio::test]
async fn v4_presigned_url_accepts_standard_and_raw_uri_path_signatures() {
    use crate::auth::SecretKey;
    use crate::auth::SimpleAuth;
    use crate::config::{S3ConfigProvider, StaticConfigProvider};
    use std::sync::Arc;

    let access_key = "AKIAIOSFODNN7EXAMPLE";
    let secret_key: SecretKey = "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY".into();
    let auth = SimpleAuth::from_single(access_key, secret_key.clone());
    let s3_config = S3Config {
        expected_region: Some("us-east-1".parse().expect("valid test region")),
        ..Default::default()
    };
    let config: Arc<dyn S3ConfigProvider> = Arc::new(StaticConfigProvider::new(Arc::new(s3_config)));

    let method = Method::GET;
    let uri = Uri::from_static("https://s3.amazonaws.com/test-bucket/path/sitemap.xmlage=");
    let decoded_uri_path = "/test-bucket/path/sitemap.xmlage=";
    let raw_uri_path = "/test-bucket/path/sitemap.xmlage=";
    let amz_date = AmzDate::parse(&fmt_current_amz_date(time::OffsetDateTime::now_utc()))
        .expect("current time should produce a valid x-amz-date");
    let headers_for_signing = [("host", "s3.amazonaws.com")];
    let query_strings_for_signing = presigned_query_fields(&amz_date, "s3");

    let canonical_requests = [
        s3s_sigv4::create_presigned_canonical_request(
            method.as_str(),
            decoded_uri_path,
            &query_strings_for_signing,
            headers_for_signing,
        ),
        s3s_sigv4::create_presigned_canonical_request_with_raw_uri_path(
            method.as_str(),
            raw_uri_path,
            &query_strings_for_signing,
            headers_for_signing,
        ),
    ];
    assert_ne!(canonical_requests[0], canonical_requests[1]);

    for canonical_request in canonical_requests {
        let string_to_sign = s3s_sigv4::create_string_to_sign(&canonical_request, &amz_date, "us-east-1", "s3");
        let signature = s3s_sigv4::calculate_signature(&string_to_sign, secret_key.expose(), &amz_date, "us-east-1", "s3");
        let mut signed_query_strings = query_strings_for_signing.clone();
        signed_query_strings.push(("X-Amz-Signature".to_owned(), signature.as_str().to_owned()));
        let qs = OrderedQs::from_vec_unchecked(signed_query_strings);
        let headers = headers_from_slice(&[("host", "s3.amazonaws.com")]);

        let mut body = Body::empty();
        let mut cx = SignatureContext {
            auth: Some(&auth),
            config: &config,
            req_version: ::http::Version::HTTP_11,
            req_method: &method,
            req_uri: &uri,
            req_body: &mut body,
            qs: Some(&qs),
            hs: &headers,
            decoded_uri_path,
            raw_uri_path,
            vh_bucket: None,
            content_length: None,
            mime: None,
            decoded_content_length: None,
            transformed_body: None,
            multipart: None,
            trailing_headers: None,
        };

        let cred = cx
            .v4_check_presigned_url()
            .await
            .expect("valid presigned URL with a raw '=' URI path should succeed");
        assert_eq!(cred.access_key, access_key);
    }
}

#[tokio::test]
async fn v4_presigned_url_uses_http2_authority_for_signed_host() {
    use crate::auth::SecretKey;
    use crate::auth::SimpleAuth;
    use crate::config::{S3ConfigProvider, StaticConfigProvider};
    use std::sync::Arc;

    let access_key = "AKIAIOSFODNN7EXAMPLE";
    let secret_key: SecretKey = "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY".into();
    let auth = SimpleAuth::from_single(access_key, secret_key.clone());
    let config: Arc<dyn S3ConfigProvider> = Arc::new(StaticConfigProvider::default());

    let method = Method::GET;
    let uri = Uri::from_static("https://s3.amazonaws.com/test-bucket/path/sitemap.xmlage=");
    let decoded_uri_path = "/test-bucket/path/sitemap.xmlage=";
    let raw_uri_path = "/test-bucket/path/sitemap.xmlage=";
    let amz_date = AmzDate::parse(&fmt_current_amz_date(time::OffsetDateTime::now_utc()))
        .expect("current time should produce a valid x-amz-date");
    let headers_for_signing = [("host", "s3.amazonaws.com")];
    let query_strings_for_signing = presigned_query_fields(&amz_date, "s3");
    let canonical_request = s3s_sigv4::create_presigned_canonical_request(
        method.as_str(),
        decoded_uri_path,
        &query_strings_for_signing,
        headers_for_signing,
    );
    let string_to_sign = s3s_sigv4::create_string_to_sign(&canonical_request, &amz_date, "us-east-1", "s3");
    let signature = s3s_sigv4::calculate_signature(&string_to_sign, secret_key.expose(), &amz_date, "us-east-1", "s3");
    let mut signed_query_strings = query_strings_for_signing;
    signed_query_strings.push(("X-Amz-Signature".to_owned(), signature.as_str().to_owned()));
    let qs = OrderedQs::from_vec_unchecked(signed_query_strings);

    let headers = HeaderMap::new();
    let mut body = Body::empty();
    let mut cx = SignatureContext {
        auth: Some(&auth),
        config: &config,
        req_version: ::http::Version::HTTP_2,
        req_method: &method,
        req_uri: &uri,
        req_body: &mut body,
        qs: Some(&qs),
        hs: &headers,
        decoded_uri_path,
        raw_uri_path,
        vh_bucket: None,
        content_length: None,
        mime: None,
        decoded_content_length: None,
        transformed_body: None,
        multipart: None,
        trailing_headers: None,
    };

    let cred = cx
        .v4_check_presigned_url()
        .await
        .expect("HTTP/2 authority should be used for a signed host header");
    assert_eq!(cred.access_key, access_key);
}

#[tokio::test]
async fn v4_presigned_url_with_port_in_signed_host() {
    // Signature-invariant nail for the port-agnostic routing fix (#438):
    // signature verification must keep using the raw Host value including
    // any port, even though routing now matches without it.
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
    let host = "user.fs.example.com:19000";
    let headers_for_signing = [("host", host)];
    let query_strings_for_signing = presigned_query_fields(&amz_date, "s3");
    let canonical_request = s3s_sigv4::create_presigned_canonical_request(
        method.as_str(),
        decoded_uri_path,
        &query_strings_for_signing,
        headers_for_signing,
    );
    let string_to_sign = s3s_sigv4::create_string_to_sign(&canonical_request, &amz_date, "us-east-1", "s3");
    let signature = s3s_sigv4::calculate_signature(&string_to_sign, secret_key.expose(), &amz_date, "us-east-1", "s3");
    let mut signed_query_strings = query_strings_for_signing;
    signed_query_strings.push(("X-Amz-Signature".to_owned(), signature.as_str().to_owned()));
    let qs = OrderedQs::from_vec_unchecked(signed_query_strings);

    let headers = headers_from_slice(&[("host", host)]);
    let mut body = Body::empty();
    let mut cx = SignatureContext {
        auth: Some(&auth),
        config: &config,
        req_version: ::http::Version::HTTP_11,
        req_method: &method,
        req_uri: &uri,
        req_body: &mut body,
        qs: Some(&qs),
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
        .v4_check_presigned_url()
        .await
        .expect("the raw Host value with its port must be used for verification");
    assert_eq!(cred.access_key, access_key);
}

#[tokio::test]
async fn sig_v2_vhost_presigned_url_with_port_uses_wire_host() {
    // SigV2 counterpart of the signature-invariant nail: the canonicalized
    // resource must contain the routed bucket prefix (/user) derived from
    // the port-carrying vhost host, while verification keeps the raw Host.
    let host = "user.fs.example.com:19000";
    let secret_key: crate::auth::SecretKey = "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY".into();

    let method = Method::GET;
    let uri = Uri::from_static("https://user.fs.example.com:19000/test.txt");
    let headers = headers_from_slice(&[("host", host)]);
    let qs_pairs = vec![
        ("AWSAccessKeyId".to_owned(), "AKIAIOSFODNN7EXAMPLE".to_owned()),
        ("Expires".to_owned(), "4294967295".to_owned()),
    ];
    let string_to_sign = s3s_sigv2::create_string_to_sign(
        s3s_sigv2::Mode::PresignedUrl,
        method.as_str(),
        "/test.txt",
        Some(&qs_pairs),
        &[("host", host)],
        Some("user"),
    );
    assert!(
        string_to_sign.contains("/user/test.txt"),
        "the routed bucket prefix must enter the canonicalized resource, got: {string_to_sign:?}"
    );
    let signature = s3s_sigv2::calculate_signature(secret_key.expose(), &string_to_sign);
    let mut qs_pairs = qs_pairs;
    qs_pairs.push(("Signature".to_owned(), signature.as_str().to_owned()));
    let qs = OrderedQs::from_vec_unchecked(qs_pairs);

    let mut body = Body::empty();
    let enabled_config = sig_v2_test_config(true);
    let auth = crate::auth::SimpleAuth::from_single("AKIAIOSFODNN7EXAMPLE", secret_key.clone());
    let mut cx = SignatureContext {
        auth: Some(&auth),
        config: &enabled_config,
        req_version: ::http::Version::HTTP_11,
        req_method: &method,
        req_uri: &uri,
        req_body: &mut body,
        qs: Some(&qs),
        hs: &headers,
        decoded_uri_path: "/test.txt",
        raw_uri_path: "/test.txt",
        vh_bucket: Some("user"),
        content_length: None,
        mime: None,
        decoded_content_length: None,
        transformed_body: None,
        multipart: None,
        trailing_headers: None,
    };
    let cred = cx
        .v2_check()
        .await
        .expect("v2 presigned url must be detected")
        .expect("vhost presigned SigV2 with a port-carrying host must verify");
    assert_eq!(cred.access_key, "AKIAIOSFODNN7EXAMPLE");

    // negative control: the SigV2 gate takes precedence over the fix
    let mut body = Body::empty();
    let disabled_config = sig_v2_test_config(false);
    let mut cx = SignatureContext {
        auth: Some(&auth),
        config: &disabled_config,
        req_version: ::http::Version::HTTP_11,
        req_method: &method,
        req_uri: &uri,
        req_body: &mut body,
        qs: Some(&qs),
        hs: &headers,
        decoded_uri_path: "/test.txt",
        raw_uri_path: "/test.txt",
        vh_bucket: Some("user"),
        content_length: None,
        mime: None,
        decoded_content_length: None,
        transformed_body: None,
        multipart: None,
        trailing_headers: None,
    };
    let err = cx
        .v2_check()
        .await
        .expect("v2 presigned url must be detected")
        .expect_err("SigV2 must be rejected when disabled");
    assert_eq!(err.code(), &S3ErrorCode::AccessDenied);
}

#[tokio::test]
async fn v4_presigned_url_put_with_valid_content_sha256() {
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

    let body_data = b"hello world";
    let content_sha256 = hex_sha256(body_data, str::to_owned);
    let method = Method::PUT;
    let uri = Uri::from_static("https://s3.amazonaws.com/test-bucket/test-key");
    let amz_date = AmzDate::parse(&fmt_current_amz_date(time::OffsetDateTime::now_utc()))
        .expect("current time should produce a valid x-amz-date");
    let headers_for_signing = [("host", "s3.amazonaws.com")];
    let query_strings_for_signing = presigned_query_fields(&amz_date, "s3");

    let canonical_request = s3s_sigv4::create_presigned_canonical_request(
        method.as_str(),
        "/test-bucket/test-key",
        &query_strings_for_signing,
        headers_for_signing,
    );
    let string_to_sign = s3s_sigv4::create_string_to_sign(&canonical_request, &amz_date, "us-east-1", "s3");
    let signature = s3s_sigv4::calculate_signature(&string_to_sign, secret_key.expose(), &amz_date, "us-east-1", "s3");

    let mut signed_query_strings = query_strings_for_signing;
    signed_query_strings.push(("X-Amz-Signature".to_owned(), signature.as_str().to_owned()));
    let qs = OrderedQs::from_vec_unchecked(signed_query_strings);

    let headers = headers_from_slice(&[
        ("host", "s3.amazonaws.com"),
        ("x-amz-content-sha256", content_sha256.as_str()),
    ]);

    let mut body = Body::from(Bytes::from_static(body_data));
    let mut cx = SignatureContext {
        auth: Some(&auth),
        config: &config,
        req_version: ::http::Version::HTTP_11,
        req_method: &method,
        req_uri: &uri,
        req_body: &mut body,
        qs: Some(&qs),
        hs: &headers,
        decoded_uri_path: "/test-bucket/test-key",
        raw_uri_path: "/test-bucket/test-key",
        vh_bucket: None,
        content_length: Some(body_data.len() as u64),
        mime: None,
        decoded_content_length: None,
        transformed_body: None,
        multipart: None,
        trailing_headers: None,
    };

    let cred = cx
        .v4_check_presigned_url()
        .await
        .expect("PUT presigned URL with valid content-sha256 should succeed");
    assert_eq!(cred.access_key, access_key);

    // Verify body was replaced with UploadStream: reading it back gives original data
    let stored = cx
        .req_body
        .store_all_limited(100)
        .await
        .expect("body should be readable through UploadStream");
    assert_eq!(stored, &body_data[..]);
}

#[tokio::test]
async fn v4_presigned_url_put_rejects_streaming_content_sha256() {
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

    let body_data = b"hello";
    let method = Method::PUT;
    let uri = Uri::from_static("https://s3.amazonaws.com/test-bucket/test-key");
    let amz_date = AmzDate::parse(&fmt_current_amz_date(time::OffsetDateTime::now_utc()))
        .expect("current time should produce a valid x-amz-date");
    let headers_for_signing = [("host", "s3.amazonaws.com")];
    let query_strings_for_signing = presigned_query_fields(&amz_date, "s3");

    let canonical_request = s3s_sigv4::create_presigned_canonical_request(
        method.as_str(),
        "/test-bucket/test-key",
        &query_strings_for_signing,
        headers_for_signing,
    );
    let string_to_sign = s3s_sigv4::create_string_to_sign(&canonical_request, &amz_date, "us-east-1", "s3");
    let signature = s3s_sigv4::calculate_signature(&string_to_sign, secret_key.expose(), &amz_date, "us-east-1", "s3");

    let mut signed_query_strings = query_strings_for_signing;
    signed_query_strings.push(("X-Amz-Signature".to_owned(), signature.as_str().to_owned()));
    let qs = OrderedQs::from_vec_unchecked(signed_query_strings);

    let headers = headers_from_slice(&[
        ("host", "s3.amazonaws.com"),
        ("x-amz-content-sha256", "STREAMING-AWS4-HMAC-SHA256-PAYLOAD"),
    ]);

    let mut body = Body::from(Bytes::from_static(body_data));
    let mut cx = SignatureContext {
        auth: Some(&auth),
        config: &config,
        req_version: ::http::Version::HTTP_11,
        req_method: &method,
        req_uri: &uri,
        req_body: &mut body,
        qs: Some(&qs),
        hs: &headers,
        decoded_uri_path: "/test-bucket/test-key",
        raw_uri_path: "/test-bucket/test-key",
        vh_bucket: None,
        content_length: Some(body_data.len() as u64),
        mime: None,
        decoded_content_length: None,
        transformed_body: None,
        multipart: None,
        trailing_headers: None,
    };

    let err = cx
        .v4_check_presigned_url()
        .await
        .expect_err("streaming content-sha256 should be rejected");
    assert_eq!(err.code(), &S3ErrorCode::NotImplemented);
}
