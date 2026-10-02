// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

//! The requirement that the `SigV4` signed-header list covers `host`.

use super::common::*;

use crate::auth::SimpleAuth;
use crate::config::{S3Config, S3ConfigProvider, StaticConfigProvider};
use crate::error::S3ErrorCode;
use crate::http::Request;
use crate::ops::signature::reject_unsigned_host;

use hyper::{Method, StatusCode, Uri, Version};
use std::fmt::Write as _;
use std::sync::Arc;
use std::sync::atomic::Ordering;

const URI: &str = "http://localhost/test-bucket/test-key.txt";

const UNSIGNED_HEADERS_MESSAGE: &str = "There were headers present in the request which were not signed";

fn config_with_require_signed_host(require_signed_host: bool) -> Arc<dyn S3ConfigProvider> {
    Arc::new(StaticConfigProvider::new(Arc::new(S3Config {
        presigned_url_max_skew_time_secs: u32::MAX,
        expected_region: Some(REGION.parse().expect("valid test region")),
        require_signed_host,
        ..Default::default()
    })))
}

/// Builds a header-authenticated `PUT` whose `Authorization` header signs exactly `signed_names`.
fn signed_request_with_names(method: Method, version: Version, uri: &str, signed_names: &[&'static str]) -> Request {
    let uri = uri.parse::<Uri>().unwrap();
    let amz_date = s3s_sigv4::AmzDate::parse(AMZ_DATE).expect("valid test date");
    let amz_date_str = amz_date.fmt_iso8601();
    let host = uri.authority().expect("test URI has authority").as_str();

    let mut signed_headers = signed_names
        .iter()
        .map(|name| {
            let value = match *name {
                "host" => host,
                "x-amz-content-sha256" => EMPTY_SHA256,
                "x-amz-date" => amz_date_str.as_str(),
                other => panic!("unsupported test signed header: {other}"),
            };
            (*name, value)
        })
        .collect::<Vec<_>>();
    signed_headers.sort_unstable_by(|lhs, rhs| lhs.0.cmp(rhs.0));
    let signed_header_names = signed_headers.iter().map(|(name, _)| *name).collect::<Vec<_>>().join(";");

    let canonical_request = s3s_sigv4::create_canonical_request(
        method.as_str(),
        uri.path(),
        &[] as &[(String, String)],
        &signed_headers,
        s3s_sigv4::Payload::SingleChunk(EMPTY_SHA256),
    );
    let string_to_sign = s3s_sigv4::create_string_to_sign(&canonical_request, &amz_date, REGION, SERVICE);
    let signature = s3s_sigv4::calculate_signature(&string_to_sign, SECRET_KEY, &amz_date, REGION, SERVICE);
    let authorization = format!(
        "AWS4-HMAC-SHA256 Credential={ACCESS_KEY}/{}/{REGION}/{SERVICE}/aws4_request, SignedHeaders={signed_header_names}, Signature={}",
        amz_date.fmt_date(),
        signature.as_str()
    );

    let mut builder = hyper::Request::builder()
        .method(method)
        .version(version)
        .uri(uri.clone())
        .header(crate::header::X_AMZ_CONTENT_SHA256, EMPTY_SHA256)
        .header(crate::header::X_AMZ_DATE, AMZ_DATE)
        .header(crate::header::AUTHORIZATION, authorization)
        .header(hyper::header::CONTENT_LENGTH, 0);
    if version == Version::HTTP_11 {
        builder = builder.header(crate::header::HOST, host);
    }
    Request::from(builder.body(empty_unknown_length_body()).unwrap())
}

/// Builds a presigned `PUT` URL whose `X-Amz-SignedHeaders` is exactly `signed_headers`.
fn presigned_put_uri(signed_headers: &[(&'static str, &'static str)]) -> Uri {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock after epoch")
        .as_secs();
    let datetime = time::OffsetDateTime::from_unix_timestamp(i64::try_from(now).expect("fits i64")).expect("valid timestamp");
    let amz_date_str = format!(
        "{:04}{:02}{:02}T{:02}{:02}{:02}Z",
        datetime.year(),
        u8::from(datetime.month()),
        datetime.day(),
        datetime.hour(),
        datetime.minute(),
        datetime.second()
    );
    let date_stamp = &amz_date_str[..8];

    let mut signed_headers = signed_headers.to_vec();
    signed_headers.sort_unstable_by_key(|(name, _)| *name);
    let signed_header_names = signed_headers.iter().map(|(name, _)| *name).collect::<Vec<_>>().join(";");

    let pairs: Vec<(&str, String)> = vec![
        ("X-Amz-Algorithm", "AWS4-HMAC-SHA256".to_owned()),
        ("X-Amz-Credential", format!("{ACCESS_KEY}/{date_stamp}/{REGION}/{SERVICE}/aws4_request")),
        ("X-Amz-Date", amz_date_str.clone()),
        ("X-Amz-Expires", "3600".to_owned()),
        ("X-Amz-SignedHeaders", signed_header_names),
    ];
    let pairs_ref: Vec<(&str, &str)> = pairs.iter().map(|(name, value)| (*name, value.as_str())).collect();
    let amz_date = s3s_sigv4::AmzDate::parse(&amz_date_str).expect("valid amz date");
    let canonical_request =
        s3s_sigv4::create_presigned_canonical_request("PUT", "/test-bucket/test-key.txt", &pairs_ref, signed_headers);
    let string_to_sign = s3s_sigv4::create_string_to_sign(&canonical_request, &amz_date, REGION, SERVICE);
    let signature = s3s_sigv4::calculate_signature(&string_to_sign, SECRET_KEY, &amz_date, REGION, SERVICE);

    let mut query = pairs
        .iter()
        .map(|(name, value)| format!("{name}={}", query_encode(value)))
        .collect::<Vec<_>>()
        .join("&");
    query.push_str("&X-Amz-Signature=");
    query.push_str(signature.as_str());
    format!("http://localhost/test-bucket/test-key.txt?{query}").parse().unwrap()
}

fn query_encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(byte as char),
            _ => {
                let _ = write!(out, "%{byte:02X}");
            }
        }
    }
    out
}

fn presigned_request(version: Version, uri: Uri, extra: &[(&'static str, &'static str)]) -> Request {
    let mut builder = hyper::Request::builder()
        .method(Method::PUT)
        .version(version)
        .uri(uri)
        .header(crate::header::HOST, "localhost")
        .header(hyper::header::CONTENT_LENGTH, 0);
    for (name, value) in extra {
        builder = builder.header(*name, *value);
    }
    Request::from(builder.body(empty_unknown_length_body()).unwrap())
}

fn assert_access_denied_body(version: Version, body: &str) {
    assert!(
        body.contains("<Code>AccessDenied</Code>"),
        "{version:?}: unexpected error code in body: {body:?}"
    );
    assert!(
        body.contains(UNSIGNED_HEADERS_MESSAGE),
        "{version:?}: unexpected error message in body: {body:?}"
    );
}

#[test]
fn unsigned_host_is_rejected_by_default_and_the_check_can_be_disabled() {
    let mut config = S3Config::default();

    let err = reject_unsigned_host(&config, &["x-amz-date"]).expect_err("host must be required by default");
    assert_eq!(err.code(), &S3ErrorCode::AccessDenied);
    assert_eq!(err.message(), Some(UNSIGNED_HEADERS_MESSAGE));

    reject_unsigned_host(&config, &["host"]).expect("a covered host must be accepted");
    reject_unsigned_host(&config, &["x-amz-date", "Host"]).expect("the comparison must be case-insensitive");
    reject_unsigned_host(&config, &["HOST"]).expect("the comparison must be case-insensitive");

    config.require_signed_host = false;
    reject_unsigned_host(&config, &["x-amz-date"]).expect("the check must be disableable");
    reject_unsigned_host(&config, &[]).expect("the check must be disableable");
}

#[tokio::test]
async fn unsigned_host_is_rejected_by_default_on_header_auth() {
    for version in [Version::HTTP_11, Version::HTTP_2] {
        let test_s3 = Arc::new(TestS3::default());
        let s3: Arc<dyn crate::s3_trait::S3> = test_s3.clone();
        let config = test_config();
        let auth = SimpleAuth::from_single(ACCESS_KEY, SECRET_KEY);
        let ccx = test_context(&s3, &config, &auth);

        let mut req = signed_request_with_names(Method::PUT, version, URI, &["x-amz-content-sha256", "x-amz-date"]);
        let response = super::call(&mut req, &ccx)
            .await
            .expect("a host-less request must still be answered");
        assert_eq!(
            response.status,
            StatusCode::FORBIDDEN,
            "{version:?}: a host-less signed list must be rejected, got {:?}",
            response.status
        );
        let body = response.body.bytes().expect("error body is buffered");
        let body = std::str::from_utf8(&body).expect("error body is UTF-8");
        assert_access_denied_body(version, body);
        assert_eq!(
            test_s3.put_object.load(Ordering::SeqCst),
            0,
            "{version:?}: a rejected request must not upload"
        );
    }
}

#[tokio::test]
async fn unsigned_host_is_accepted_when_the_check_is_disabled_on_header_auth() {
    for version in [Version::HTTP_11, Version::HTTP_2] {
        let test_s3 = Arc::new(TestS3::default());
        let s3: Arc<dyn crate::s3_trait::S3> = test_s3.clone();
        let config = config_with_require_signed_host(false);
        let auth = SimpleAuth::from_single(ACCESS_KEY, SECRET_KEY);
        let ccx = test_context(&s3, &config, &auth);

        let mut req = signed_request_with_names(Method::PUT, version, URI, &["x-amz-content-sha256", "x-amz-date"]);
        let response = super::call(&mut req, &ccx)
            .await
            .expect("a request signed without host must be routed");
        assert!(
            response.status.is_success(),
            "{version:?}: a host-less signed list must keep working when disabled, got {:?}",
            response.status
        );
        assert_eq!(test_s3.put_object.load(Ordering::SeqCst), 1, "{version:?}: the upload must be routed");
    }
}

#[tokio::test]
async fn signed_host_is_accepted_when_required_on_header_auth() {
    for version in [Version::HTTP_11, Version::HTTP_2] {
        let test_s3 = Arc::new(TestS3::default());
        let s3: Arc<dyn crate::s3_trait::S3> = test_s3.clone();
        let config = config_with_require_signed_host(true);
        let auth = SimpleAuth::from_single(ACCESS_KEY, SECRET_KEY);
        let ccx = test_context(&s3, &config, &auth);

        let mut req = signed_request_with_names(Method::PUT, version, URI, &["host", "x-amz-content-sha256", "x-amz-date"]);
        let response = super::call(&mut req, &ccx)
            .await
            .expect("a request signed with host must be routed");
        assert!(
            response.status.is_success(),
            "{version:?}: a signed host must be accepted, got {:?}",
            response.status
        );
        assert_eq!(test_s3.put_object.load(Ordering::SeqCst), 1, "{version:?}: the upload must be routed");
    }
}

#[tokio::test]
async fn unsigned_host_is_rejected_by_default_on_presigned_url() {
    for version in [Version::HTTP_11, Version::HTTP_2] {
        let test_s3 = Arc::new(TestS3::default());
        let s3: Arc<dyn crate::s3_trait::S3> = test_s3.clone();
        let config = test_config();
        let auth = SimpleAuth::from_single(ACCESS_KEY, SECRET_KEY);
        let ccx = test_context(&s3, &config, &auth);

        let uri = presigned_put_uri(&[("x-amz-content-sha256", EMPTY_SHA256)]);
        let mut req = presigned_request(version, uri, &[("x-amz-content-sha256", EMPTY_SHA256)]);
        let response = super::call(&mut req, &ccx)
            .await
            .expect("a host-less presigned request must still be answered");
        assert_eq!(
            response.status,
            StatusCode::FORBIDDEN,
            "{version:?}: a host-less X-Amz-SignedHeaders must be rejected, got {:?}",
            response.status
        );
        let body = response.body.bytes().expect("error body is buffered");
        let body = std::str::from_utf8(&body).expect("error body is UTF-8");
        assert_access_denied_body(version, body);
        assert_eq!(
            test_s3.put_object.load(Ordering::SeqCst),
            0,
            "{version:?}: a rejected request must not upload"
        );
    }
}

#[tokio::test]
async fn unsigned_host_is_accepted_when_the_check_is_disabled_on_presigned_url() {
    for version in [Version::HTTP_11, Version::HTTP_2] {
        let test_s3 = Arc::new(TestS3::default());
        let s3: Arc<dyn crate::s3_trait::S3> = test_s3.clone();
        let config = config_with_require_signed_host(false);
        let auth = SimpleAuth::from_single(ACCESS_KEY, SECRET_KEY);
        let ccx = test_context(&s3, &config, &auth);

        let uri = presigned_put_uri(&[("x-amz-content-sha256", EMPTY_SHA256)]);
        let mut req = presigned_request(version, uri, &[("x-amz-content-sha256", EMPTY_SHA256)]);
        let response = super::call(&mut req, &ccx)
            .await
            .expect("a presigned URL signed without host must be routed");
        assert!(
            response.status.is_success(),
            "{version:?}: a host-less X-Amz-SignedHeaders must keep working when disabled, got {:?}",
            response.status
        );
        assert_eq!(test_s3.put_object.load(Ordering::SeqCst), 1, "{version:?}: the upload must be routed");
    }
}

#[tokio::test]
async fn signed_host_is_accepted_when_required_on_presigned_url() {
    for version in [Version::HTTP_11, Version::HTTP_2] {
        let test_s3 = Arc::new(TestS3::default());
        let s3: Arc<dyn crate::s3_trait::S3> = test_s3.clone();
        let config = config_with_require_signed_host(true);
        let auth = SimpleAuth::from_single(ACCESS_KEY, SECRET_KEY);
        let ccx = test_context(&s3, &config, &auth);

        let uri = presigned_put_uri(&[("host", "localhost")]);
        let mut req = presigned_request(version, uri, &[]);
        let response = super::call(&mut req, &ccx)
            .await
            .expect("a presigned URL signed with host must be routed");
        assert!(
            response.status.is_success(),
            "{version:?}: a signed host must be accepted, got {:?}",
            response.status
        );
        assert_eq!(test_s3.put_object.load(Ordering::SeqCst), 1, "{version:?}: the upload must be routed");
    }
}
