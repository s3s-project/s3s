// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

//! Signature coverage of request metadata: which `x-amz-*` headers must be signed, the
//! allowlist, and the signature-path switches.

use super::common::*;

use crate::auth::SimpleAuth;
use crate::config::{S3Config, S3ConfigProvider, StaticConfigProvider};
use crate::http::{Body, Request};

use hyper::{Method, StatusCode, Uri, Version};
use std::sync::Arc;

use std::sync::atomic::Ordering;

#[tokio::test]
async fn presigned_url_expires_0_should_be_expired() {
    use crate::S3ErrorCode;
    use crate::config::{S3ConfigProvider, StaticConfigProvider};
    use crate::http::{Body, OrderedQs};
    use crate::ops::signature::SignatureContext;
    use hyper::{HeaderMap, Method, Uri};
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
    let headers = HeaderMap::new();
    let mut body = Body::empty();

    let mut cx = SignatureContext {
        path_encoding: crate::auth::SigV4PathEncoding::S3,
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

    let result = cx.v4_check_presigned_url().await;
    assert!(result.is_err());
    let err = result.unwrap_err();
    assert_eq!(err.code(), &S3ErrorCode::AccessDenied);
}

fn config_with(apply: impl FnOnce(&mut S3Config)) -> Arc<dyn S3ConfigProvider> {
    let mut config = S3Config {
        presigned_url_max_skew_time_secs: u32::MAX,
        expected_region: Some(REGION.parse().expect("valid test region")),
        ..Default::default()
    };
    apply(&mut config);
    Arc::new(StaticConfigProvider::new(Arc::new(config)))
}

fn test_config_with_allowlist(entries: &[&str]) -> Arc<dyn S3ConfigProvider> {
    Arc::new(StaticConfigProvider::new(Arc::new(S3Config {
        presigned_url_max_skew_time_secs: u32::MAX,
        expected_region: Some(REGION.parse().expect("valid test region")),
        unsigned_amz_header_allowlist: entries.iter().map(|name| (*name).to_owned()).collect(),
        ..Default::default()
    })))
}

/// A `PUT` carrying `x-s3s-payload-length`, never listed in the signed-header list.
fn declaration_request(method: Method, version: Version, uri: &str, declared: &str) -> Request {
    let uri = uri.parse::<Uri>().unwrap();
    let authorization = sign_request(&method, &uri, EMPTY_SHA256, &[]);
    let mut builder = hyper::Request::builder()
        .method(method)
        .version(version)
        .uri(uri.clone())
        .header(crate::header::X_AMZ_CONTENT_SHA256, EMPTY_SHA256)
        .header(crate::header::X_AMZ_DATE, AMZ_DATE)
        .header(crate::header::AUTHORIZATION, authorization)
        .header(hyper::header::CONTENT_LENGTH, declared)
        .header(hyper::header::HeaderName::from_static("x-s3s-payload-length"), declared);
    if version == Version::HTTP_11 {
        builder = builder.header(crate::header::HOST, uri.authority().unwrap().as_str());
    }
    Request::from(builder.body(empty_unknown_length_body()).unwrap())
}

/// The `x-s3s-` prefix carries the same weight as `x-amz-*` on both sides: an unsigned
/// declaration is rejected by default, and `S3Config::unsigned_s3s_header_allowlist` is what
/// permits it.
#[tokio::test]
async fn the_declaration_header_must_be_signed_unless_allowlisted() {
    const URI: &str = "http://localhost/test-bucket/test-key.txt";
    const NAME: &str = "x-s3s-payload-length";
    let auth = SimpleAuth::from_single(ACCESS_KEY, SECRET_KEY);

    // Unsigned, with the default (empty) allowlists: rejected before routing.
    let test_s3 = Arc::new(TestS3::default());
    let s3: Arc<dyn crate::s3_trait::S3> = test_s3.clone();
    let config = test_config();
    let ccx = test_context(&s3, &config, &auth);
    let mut rejected = declaration_request(Method::PUT, Version::HTTP_11, URI, "0");
    let response = super::call(&mut rejected, &ccx).await.expect("must be routed");
    assert_eq!(response.status, StatusCode::FORBIDDEN, "an unsigned declaration must be rejected");
    assert_eq!(
        test_s3.put_object.load(Ordering::SeqCst),
        0,
        "a rejected request must not reach PutObject"
    );

    // The same request, with the header named in the allowlist: served, and the declaration
    // agrees with the framing it declares.
    let test_s3 = Arc::new(TestS3::default());
    let s3: Arc<dyn crate::s3_trait::S3> = test_s3.clone();
    let config = test_config_with_s3s_allowlist(&[NAME]);
    let ccx = test_context(&s3, &config, &auth);
    let mut allowed = declaration_request(Method::PUT, Version::HTTP_11, URI, "0");
    let response = super::call(&mut allowed, &ccx).await.expect("must be routed");
    assert_eq!(response.status, StatusCode::OK, "an allowlisted declaration must be served");
    assert_eq!(test_s3.put_object.load(Ordering::SeqCst), 1, "the allowlisted request reaches PutObject");

    // The other family's allowlist does not exempt an extension header: each guard reads the list
    // that belongs to its own prefix.
    let test_s3 = Arc::new(TestS3::default());
    let s3: Arc<dyn crate::s3_trait::S3> = test_s3.clone();
    let config = test_config_with_allowlist(&[NAME]);
    let ccx = test_context(&s3, &config, &auth);
    let mut crossed = declaration_request(Method::PUT, Version::HTTP_11, URI, "0");
    let response = super::call(&mut crossed, &ccx).await.expect("must be routed");
    assert_eq!(
        response.status,
        StatusCode::FORBIDDEN,
        "the x-amz- allowlist must not exempt an x-s3s- header"
    );
    assert_eq!(
        test_s3.put_object.load(Ordering::SeqCst),
        0,
        "a rejected request must not reach PutObject"
    );
}

/// With the extension turned off the header is an ordinary unknown header: unsigned, and served.
#[tokio::test]
async fn disabling_the_extension_leaves_the_declaration_header_unread() {
    const URI: &str = "http://localhost/test-bucket/test-key.txt";
    let auth = SimpleAuth::from_single(ACCESS_KEY, SECRET_KEY);
    let test_s3 = Arc::new(TestS3::default());
    let s3: Arc<dyn crate::s3_trait::S3> = test_s3.clone();
    let config = config_with(|config| config.payload_length_extension = false);
    let ccx = test_context(&s3, &config, &auth);
    let mut req = declaration_request(Method::PUT, Version::HTTP_11, URI, "0");

    let response = super::call(&mut req, &ccx).await.expect("must be routed");
    assert_eq!(response.status, StatusCode::OK, "an opt-out deployment must not read the header");
    assert_eq!(test_s3.put_object.load(Ordering::SeqCst), 1);
}

/// A declaration that disagrees with the framing is refused while the body is prepared, on the
/// request path rather than only in the unit tests of the extension.
#[tokio::test]
async fn a_declaration_that_disagrees_with_the_framing_is_refused() {
    const URI: &str = "http://localhost/test-bucket/test-key.txt";
    const NAME: &str = "x-s3s-payload-length";
    let auth = SimpleAuth::from_single(ACCESS_KEY, SECRET_KEY);
    let test_s3 = Arc::new(TestS3::default());
    let s3: Arc<dyn crate::s3_trait::S3> = test_s3.clone();
    let config = test_config_with_s3s_allowlist(&[NAME]);
    let ccx = test_context(&s3, &config, &auth);
    let mut req = declaration_request(Method::PUT, Version::HTTP_11, URI, "5");
    // The declaration says five bytes and the framing says none; neither is in the signed-header
    // list, so the exchange does not disturb the signature under test.
    req.headers
        .insert(hyper::header::CONTENT_LENGTH, hyper::header::HeaderValue::from_static("0"));

    let err = super::prepare(&mut req, &ccx)
        .await
        .err()
        .expect("a declaration that disagrees with the framing must be refused");
    assert_eq!(err.code(), &crate::error::S3ErrorCode::EntityTooSmall);
    assert_eq!(test_s3.put_object.load(Ordering::SeqCst), 0, "the request must not reach PutObject");
}

/// Each header family names its exceptions in its own allowlist: an `x-s3s-*` name belongs in
/// [`S3Config::unsigned_s3s_header_allowlist`], an `x-amz-*` name in
/// [`S3Config::unsigned_amz_header_allowlist`].
fn test_config_with_s3s_allowlist(entries: &[&str]) -> Arc<dyn S3ConfigProvider> {
    Arc::new(StaticConfigProvider::new(Arc::new(S3Config {
        presigned_url_max_skew_time_secs: u32::MAX,
        expected_region: Some(REGION.parse().expect("valid test region")),
        unsigned_s3s_header_allowlist: entries.iter().map(|name| (*name).to_owned()).collect(),
        ..Default::default()
    })))
}

fn copy_source_request(method: Method, version: Version, uri: &str, sign_copy_source: bool) -> Request {
    const COPY_SOURCE: (&str, &str) = ("x-amz-copy-source", "/source-bucket/source-key");
    let uri = uri.parse::<Uri>().unwrap();
    let signed_extra = if sign_copy_source { &[COPY_SOURCE][..] } else { &[] };
    let authorization = sign_request(&method, &uri, EMPTY_SHA256, signed_extra);
    let mut builder = hyper::Request::builder()
        .method(method)
        .version(version)
        .uri(uri.clone())
        .header(crate::header::X_AMZ_CONTENT_SHA256, EMPTY_SHA256)
        .header(crate::header::X_AMZ_DATE, AMZ_DATE)
        .header(crate::header::AUTHORIZATION, authorization)
        .header(hyper::header::CONTENT_LENGTH, 0)
        .header(COPY_SOURCE.0, COPY_SOURCE.1);
    if version == Version::HTTP_11 {
        builder = builder.header(crate::header::HOST, uri.authority().unwrap().as_str());
    }
    Request::from(builder.body(empty_unknown_length_body()).unwrap())
}

fn presigned_put_uri() -> Uri {
    presigned_put_uri_with_headers(&[])
}

fn presigned_put_uri_with_headers(extra_headers: &[(&str, &str)]) -> Uri {
    presigned_put_uri_with_headers_and_payload(extra_headers, UNSIGNED_PAYLOAD)
}

/// Builds a presigned `PUT` URL signed over `host` plus `extra_headers`, with `payload_line` as the
/// canonical payload line, so a test can sign a URL the way a client that declares a body digest does.
fn presigned_put_uri_with_headers_and_payload(extra_headers: &[(&str, &str)], payload_line: &str) -> Uri {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock after epoch")
        .as_secs();
    let datetime = jiff::Timestamp::from_second(i64::try_from(now).expect("fits i64")).expect("valid timestamp");
    let amz_date_str = datetime.strftime("%Y%m%dT%H%M%SZ").to_string();
    let date_stamp = &amz_date_str[..8];
    let mut signed_headers = vec![("host", "localhost")];
    signed_headers.extend(extra_headers.iter().copied());
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
    let canonical_request = s3s_sigv4::create_presigned_canonical_request_with_payload(
        "PUT",
        "/test-bucket/test-key.txt",
        &pairs_ref,
        signed_headers,
        payload_line,
    );
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
    use std::fmt::Write as _;

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

fn presigned_request(version: Version, uri: Uri, extra: &[(&str, &str)]) -> Request {
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

#[tokio::test]
async fn unsigned_copy_source_is_rejected_on_header_signed_requests() {
    const URI: &str = "http://localhost/test-bucket/test-key.txt";
    for version in [Version::HTTP_11, Version::HTTP_2] {
        let test_s3 = Arc::new(TestS3::default());
        let s3: Arc<dyn crate::s3_trait::S3> = test_s3.clone();
        let config = test_config();
        let auth = SimpleAuth::from_single(ACCESS_KEY, SECRET_KEY);
        let ccx = test_context(&s3, &config, &auth);

        // Positive control: covered by the signature, so it must keep working.
        let mut signed = copy_source_request(Method::PUT, version, URI, true);
        let response = super::call(&mut signed, &ccx)
            .await
            .expect("signed copy source must be routed");
        assert!(response.status.is_success(), "{version:?}: signed copy source got {:?}", response.status);
        assert_eq!(
            test_s3.copy_object.load(Ordering::SeqCst),
            1,
            "{version:?}: control must invoke CopyObject"
        );

        // Not covered by the signature: must be rejected before routing.
        let mut unsigned = copy_source_request(Method::PUT, version, URI, false);
        let response = super::call(&mut unsigned, &ccx)
            .await
            .expect("unsigned copy source must be routed");
        assert_eq!(
            response.status,
            StatusCode::FORBIDDEN,
            "{version:?}: an unsigned x-amz-copy-source must be rejected, got {:?}",
            response.status
        );
        assert_eq!(
            test_s3.copy_object.load(Ordering::SeqCst),
            1,
            "{version:?}: a rejected request must not reach CopyObject"
        );
    }
}

#[tokio::test]
async fn unsigned_copy_source_is_rejected_on_presigned_requests() {
    for version in [Version::HTTP_11, Version::HTTP_2] {
        let test_s3 = Arc::new(TestS3::default());
        let s3: Arc<dyn crate::s3_trait::S3> = test_s3.clone();
        let config = test_config();
        let auth = SimpleAuth::from_single(ACCESS_KEY, SECRET_KEY);
        let ccx = test_context(&s3, &config, &auth);

        // The presigned URL used as issued is an upload.
        let mut plain = presigned_request(version, presigned_put_uri(), &[]);
        let response = super::call(&mut plain, &ccx).await.expect("presigned PUT must be routed");
        assert!(response.status.is_success(), "{version:?}: presigned PUT got {:?}", response.status);
        assert_eq!(test_s3.put_object.load(Ordering::SeqCst), 1, "{version:?}: control must invoke PutObject");

        // One unsigned header added by the URL holder.
        let mut added = presigned_request(version, presigned_put_uri(), &[("x-amz-copy-source", "/source-bucket/source-key")]);
        let response = super::call(&mut added, &ccx)
            .await
            .expect("unsigned copy source must be routed");
        assert_eq!(
            response.status,
            StatusCode::FORBIDDEN,
            "{version:?}: an unsigned x-amz-copy-source must be rejected, got {:?}",
            response.status
        );
        assert_eq!(
            test_s3.copy_object.load(Ordering::SeqCst),
            0,
            "{version:?}: a presigned PUT must not become a copy"
        );
        assert_eq!(
            test_s3.put_object.load(Ordering::SeqCst),
            1,
            "{version:?}: the rejected request must not upload"
        );
    }
}

#[tokio::test]
async fn unsigned_acl_is_rejected_and_not_parsed() {
    const URI: &str = "http://localhost/test-bucket/test-key.txt";
    let test_s3 = Arc::new(TestS3::default());
    let s3: Arc<dyn crate::s3_trait::S3> = test_s3.clone();
    let config = test_config();
    let auth = SimpleAuth::from_single(ACCESS_KEY, SECRET_KEY);
    let ccx = test_context(&s3, &config, &auth);

    // Signed over `host;x-amz-content-sha256;x-amz-date`, then one unsigned `x-amz-acl`
    // appended by the sender.
    let mut injected = signed_request(Method::PUT, Version::HTTP_11, URI, EMPTY_SHA256, &[]);
    injected
        .headers
        .insert(crate::header::X_AMZ_ACL, hyper::header::HeaderValue::from_static("public-read"));
    injected
        .headers
        .insert(hyper::header::CONTENT_LENGTH, hyper::header::HeaderValue::from_static("0"));
    let response = super::call(&mut injected, &ccx).await.expect("injected PUT must be routed");
    assert_eq!(
        response.status,
        StatusCode::FORBIDDEN,
        "an unsigned x-amz-acl must be rejected, got {:?}",
        response.status
    );
    assert_eq!(
        test_s3.put_object.load(Ordering::SeqCst),
        0,
        "a rejected request must not reach PutObject"
    );
    assert_eq!(*test_s3.last_put_acl.lock().unwrap(), None, "an unsigned acl must never be parsed");
}

/// A presigned URL signed over `UNSIGNED-PAYLOAD` keeps accepting an unsigned
/// `x-amz-content-sha256` header, because the header value is the payload line and the marker
/// reproduces the one the URL was signed with. This is the shape every default client produces.
#[tokio::test]
async fn unsigned_content_sha256_is_still_accepted_on_presigned_requests() {
    let test_s3 = Arc::new(TestS3::default());
    let s3: Arc<dyn crate::s3_trait::S3> = test_s3.clone();
    let config = test_config();
    let auth = SimpleAuth::from_single(ACCESS_KEY, SECRET_KEY);
    let ccx = test_context(&s3, &config, &auth);

    let mut req = presigned_request(Version::HTTP_11, presigned_put_uri(), &[("x-amz-content-sha256", UNSIGNED_PAYLOAD)]);
    let response = super::call(&mut req, &ccx).await.expect("presigned PUT must be routed");
    assert!(
        response.status.is_success(),
        "x-amz-content-sha256 must keep working outside SignedHeaders on a presigned PUT, got {:?}",
        response.status
    );
    assert_eq!(test_s3.put_object.load(Ordering::SeqCst), 1, "the upload must still be routed");
}

/// The header value is the payload line, so a digest sent over a URL signed with
/// `UNSIGNED-PAYLOAD` breaks the signature instead of installing a body check. S3 rejects the same
/// shape, so the unsigned-header exemption does not widen what a presigned URL accepts.
#[tokio::test]
async fn presigned_content_sha256_that_is_not_the_payload_line_is_rejected() {
    let test_s3 = Arc::new(TestS3::default());
    let s3: Arc<dyn crate::s3_trait::S3> = test_s3.clone();
    let config = test_config();
    let auth = SimpleAuth::from_single(ACCESS_KEY, SECRET_KEY);
    let ccx = test_context(&s3, &config, &auth);

    let digest = other_body_digest();
    let mut req = presigned_request(Version::HTTP_11, presigned_put_uri(), &[("x-amz-content-sha256", digest.as_str())]);
    let response = super::call(&mut req, &ccx).await.expect("presigned PUT must be answered");
    assert_eq!(
        response.status,
        StatusCode::FORBIDDEN,
        "a digest outside the signed payload line must not verify"
    );
    let body = response.body.bytes().expect("error body is buffered");
    assert!(
        std::str::from_utf8(&body)
            .expect("error body is UTF-8")
            .contains("<Code>SignatureDoesNotMatch</Code>"),
        "expected SignatureDoesNotMatch"
    );
    assert_eq!(test_s3.put_object.load(Ordering::SeqCst), 0, "a rejected request must not be routed");
}

const REQUEST_METADATA_HEADERS: &[(&str, &str)] = &[
    ("x-amz-decoded-content-length", "0"),
    ("x-amz-trailer", "x-amz-checksum-sha256"),
    ("x-amz-checksum-algorithm", "SHA256"),
];

#[tokio::test]
async fn unsigned_request_metadata_is_rejected() {
    const URI: &str = "http://localhost/test-bucket/test-key.txt";
    let config = test_config();
    let auth = SimpleAuth::from_single(ACCESS_KEY, SECRET_KEY);
    for version in [Version::HTTP_11, Version::HTTP_2] {
        for &(name, value) in REQUEST_METADATA_HEADERS {
            let test_s3 = Arc::new(TestS3::default());
            let s3: Arc<dyn crate::s3_trait::S3> = test_s3.clone();
            let ccx = test_context(&s3, &config, &auth);
            let mut req = signed_request(Method::PUT, version, URI, EMPTY_SHA256, &[]);
            req.headers
                .insert(hyper::header::CONTENT_LENGTH, hyper::header::HeaderValue::from_static("0"));
            req.headers.insert(
                hyper::header::HeaderName::from_static(name),
                hyper::header::HeaderValue::from_static(value),
            );

            let response = super::call(&mut req, &ccx)
                .await
                .expect("header-authenticated PUT must be routed");
            assert_eq!(response.status, StatusCode::FORBIDDEN, "header auth, {version:?}, {name}");
            let body = response.body.bytes().expect("error body is buffered");
            assert!(
                std::str::from_utf8(&body)
                    .expect("error body is UTF-8")
                    .contains("<Code>AccessDenied</Code>"),
                "header auth, {version:?}, {name}: {body:?}"
            );
            assert_eq!(test_s3.put_object.load(Ordering::SeqCst), 0, "header auth, {version:?}, {name}");

            let test_s3 = Arc::new(TestS3::default());
            let s3: Arc<dyn crate::s3_trait::S3> = test_s3.clone();
            let ccx = test_context(&s3, &config, &auth);
            let mut req = presigned_request(version, presigned_put_uri(), &[(name, value)]);

            let response = super::call(&mut req, &ccx).await.expect("presigned PUT must be routed");
            assert_eq!(response.status, StatusCode::FORBIDDEN, "presigned, {version:?}, {name}");
            let body = response.body.bytes().expect("error body is buffered");
            assert!(
                std::str::from_utf8(&body)
                    .expect("error body is UTF-8")
                    .contains("<Code>AccessDenied</Code>"),
                "presigned, {version:?}, {name}: {body:?}"
            );
            assert_eq!(test_s3.put_object.load(Ordering::SeqCst), 0, "presigned, {version:?}, {name}");
        }
    }
}

#[tokio::test]
async fn signed_request_metadata_is_accepted() {
    const URI: &str = "http://localhost/test-bucket/test-key.txt";
    let config = test_config();
    let auth = SimpleAuth::from_single(ACCESS_KEY, SECRET_KEY);
    for version in [Version::HTTP_11, Version::HTTP_2] {
        let test_s3 = Arc::new(TestS3::default());
        let s3: Arc<dyn crate::s3_trait::S3> = test_s3.clone();
        let ccx = test_context(&s3, &config, &auth);
        let mut req = signed_request(Method::PUT, version, URI, EMPTY_SHA256, REQUEST_METADATA_HEADERS);
        req.headers
            .insert(hyper::header::CONTENT_LENGTH, hyper::header::HeaderValue::from_static("0"));

        let response = super::call(&mut req, &ccx)
            .await
            .expect("header-authenticated PUT must be routed");
        assert!(
            response.status.is_success(),
            "signed request metadata must be accepted with header auth over {version:?}, got {:?}",
            response.status
        );
        assert_eq!(test_s3.put_object.load(Ordering::SeqCst), 1, "header auth, {version:?}");

        let test_s3 = Arc::new(TestS3::default());
        let s3: Arc<dyn crate::s3_trait::S3> = test_s3.clone();
        let ccx = test_context(&s3, &config, &auth);
        let uri = presigned_put_uri_with_headers(REQUEST_METADATA_HEADERS);
        let mut req = presigned_request(version, uri, REQUEST_METADATA_HEADERS);

        let response = super::call(&mut req, &ccx).await.expect("presigned PUT must be routed");
        assert!(
            response.status.is_success(),
            "signed request metadata must be accepted with a presigned URL over {version:?}, got {:?}",
            response.status
        );
        assert_eq!(test_s3.put_object.load(Ordering::SeqCst), 1, "presigned, {version:?}");
    }
}

#[tokio::test]
async fn configured_allowlist_permits_unsigned_request_metadata() {
    const URI: &str = "http://localhost/test-bucket/test-key.txt";
    let auth = SimpleAuth::from_single(ACCESS_KEY, SECRET_KEY);
    for &(name, value) in REQUEST_METADATA_HEADERS {
        let config = test_config_with_allowlist(&[name]);
        for version in [Version::HTTP_11, Version::HTTP_2] {
            let test_s3 = Arc::new(TestS3::default());
            let s3: Arc<dyn crate::s3_trait::S3> = test_s3.clone();
            let ccx = test_context(&s3, &config, &auth);
            let mut req = signed_request(Method::PUT, version, URI, EMPTY_SHA256, &[]);
            req.headers
                .insert(hyper::header::CONTENT_LENGTH, hyper::header::HeaderValue::from_static("0"));
            req.headers.insert(
                hyper::header::HeaderName::from_static(name),
                hyper::header::HeaderValue::from_static(value),
            );
            let response = super::call(&mut req, &ccx).await.expect("allowlisted PUT must be routed");
            assert_eq!(response.status, StatusCode::OK, "header auth, {version:?}, {name}");
            assert_eq!(test_s3.put_object.load(Ordering::SeqCst), 1, "header auth, {version:?}, {name}");

            let test_s3 = Arc::new(TestS3::default());
            let s3: Arc<dyn crate::s3_trait::S3> = test_s3.clone();
            let ccx = test_context(&s3, &config, &auth);
            let mut req = presigned_request(version, presigned_put_uri(), &[(name, value)]);
            let response = super::call(&mut req, &ccx).await.expect("allowlisted PUT must be routed");
            assert_eq!(response.status, StatusCode::OK, "presigned, {version:?}, {name}");
            assert_eq!(test_s3.put_object.load(Ordering::SeqCst), 1, "presigned, {version:?}, {name}");
        }
    }
}

#[tokio::test]
async fn configured_allowlist_permits_the_named_unsigned_header() {
    const URI: &str = "http://localhost/test-bucket/test-key.txt";
    let test_s3 = Arc::new(TestS3::default());
    let s3: Arc<dyn crate::s3_trait::S3> = test_s3.clone();
    let config = test_config_with_allowlist(&["x-amz-copy-source"]);
    let auth = SimpleAuth::from_single(ACCESS_KEY, SECRET_KEY);
    let ccx = test_context(&s3, &config, &auth);

    let mut allowed = copy_source_request(Method::PUT, Version::HTTP_11, URI, false);
    let response = super::call(&mut allowed, &ccx)
        .await
        .expect("allowlisted request must be routed");
    assert!(
        response.status.is_success(),
        "an allowlisted unsigned header must be accepted, got {:?}",
        response.status
    );
    assert_eq!(
        test_s3.copy_object.load(Ordering::SeqCst),
        1,
        "the allowlist is the documented escape hatch and must restore the previous routing"
    );
}

#[tokio::test]
async fn allowlist_entry_is_matched_case_sensitively() {
    const URI: &str = "http://localhost/test-bucket/test-key.txt";
    let test_s3 = Arc::new(TestS3::default());
    let s3: Arc<dyn crate::s3_trait::S3> = test_s3.clone();
    let auth = SimpleAuth::from_single(ACCESS_KEY, SECRET_KEY);
    let config = test_config_with_allowlist(&["X-Amz-Copy-Source"]);
    let ccx = test_context(&s3, &config, &auth);

    let mut req = copy_source_request(Method::PUT, Version::HTTP_11, URI, false);
    let response = super::call(&mut req, &ccx).await.expect("ops::call serializes errors");
    assert_eq!(
        response.status,
        StatusCode::FORBIDDEN,
        "a non-lowercase allowlist entry must not exempt the header"
    );
    assert_eq!(test_s3.copy_object.load(Ordering::SeqCst), 0, "the request must not be routed");
}

#[tokio::test]
async fn presigned_url_is_rejected_when_the_path_is_disabled() {
    let test_s3 = Arc::new(TestS3::default());
    let s3: Arc<dyn crate::s3_trait::S3> = test_s3.clone();
    let auth = SimpleAuth::from_single(ACCESS_KEY, SECRET_KEY);
    let switch_message = "Presigned URL authentication is disabled by server configuration";

    // Control: enabled by default, the presigned URL is served (an upload is dispatched).
    let enabled: Arc<dyn S3ConfigProvider> = test_config();
    let ccx = test_context(&s3, &enabled, &auth);
    let mut req = presigned_request(Version::HTTP_11, presigned_put_uri(), &[]);
    let response = super::call(&mut req, &ccx).await.expect("ops::call serializes errors");
    assert!(
        response.status.is_success(),
        "a presigned URL must work by default, got {:?}",
        response.status
    );
    assert_eq!(test_s3.put_object.load(Ordering::SeqCst), 1, "the control request must upload");

    // Disabled: rejected, and the rejection comes from the switch.
    let disabled = config_with(|config| config.allow_presigned_url = false);
    let ccx = test_context(&s3, &disabled, &auth);
    let mut req = presigned_request(Version::HTTP_11, presigned_put_uri(), &[]);
    let response = super::call(&mut req, &ccx).await.expect("ops::call serializes errors");
    assert_eq!(
        response.status,
        StatusCode::FORBIDDEN,
        "a presigned request must be rejected when presigned URLs are disabled"
    );
    let body = response.body.bytes().expect("error body is buffered");
    let body = String::from_utf8_lossy(&body);
    assert!(
        body.contains(switch_message),
        "the rejection must come from the presigned-URL switch, got: {body}"
    );
    assert_eq!(test_s3.put_object.load(Ordering::SeqCst), 1, "the rejected request must not upload");
}

#[tokio::test]
async fn disabling_presigned_urls_leaves_header_auth_working() {
    const URI: &str = "http://localhost/test-bucket/test-key.txt";
    let test_s3 = Arc::new(TestS3::default());
    let s3: Arc<dyn crate::s3_trait::S3> = test_s3.clone();
    let auth = SimpleAuth::from_single(ACCESS_KEY, SECRET_KEY);
    let disabled = config_with(|config| config.allow_presigned_url = false);
    let ccx = test_context(&s3, &disabled, &auth);

    let mut req = signed_request(Method::PUT, Version::HTTP_11, URI, EMPTY_SHA256, &[]);
    req.headers
        .insert(hyper::header::CONTENT_LENGTH, hyper::header::HeaderValue::from_static("0"));
    let response = super::call(&mut req, &ccx).await.expect("header auth must still be routed");
    assert!(
        response.status.is_success(),
        "header authentication is a different path and must be unaffected, got {:?}",
        response.status
    );
    assert_eq!(test_s3.put_object.load(Ordering::SeqCst), 1);
}

fn signed_post_form() -> Request {
    use crate::auth::SecretKey;

    let secret_key: SecretKey = "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY".into();
    let policy_json = &format!(
        r#"{{"expiration":"2030-01-01T00:00:00.000Z","conditions":[{}]}}"#,
        post_policy_test_helpers::BASE_CONDITIONS,
    );
    post_policy_test_helpers::build_post_object_request(policy_json, "hello", &secret_key, false)
}

fn post_form_config(apply: impl FnOnce(&mut S3Config)) -> Arc<dyn S3ConfigProvider> {
    config_with(|config| {
        config.post_object_max_file_size = 1024;
        apply(config);
    })
}

#[tokio::test]
async fn signed_post_form_is_rejected_when_the_path_is_disabled() {
    let auth = post_policy_test_helpers::create_test_auth();

    // Control: enabled by default, the signed form is accepted.
    let enabled = post_form_config(|_| {});
    let mut req = signed_post_form();
    let s3: Arc<dyn crate::s3_trait::S3> = Arc::new(TestS3::default());
    let ccx = post_policy_test_helpers::create_test_context(&s3, &enabled, &auth);
    let response = super::call(&mut req, &ccx).await.expect("ops::call serializes errors");
    assert!(
        response.status.is_success(),
        "a signed POST form must be accepted by default, got {:?}",
        response.status
    );

    // Disabled: rejected, and the rejection comes from the switch.
    let disabled = post_form_config(|config| config.allow_post_signature = false);
    let mut req = signed_post_form();
    let s3: Arc<dyn crate::s3_trait::S3> = Arc::new(TestS3::default());
    let ccx = post_policy_test_helpers::create_test_context(&s3, &disabled, &auth);
    let response = super::call(&mut req, &ccx).await.expect("ops::call serializes errors");
    assert_eq!(
        response.status,
        StatusCode::FORBIDDEN,
        "a signed POST form must be rejected when POST signatures are disabled"
    );
    let body = response.body.bytes().expect("error body is buffered");
    let body = String::from_utf8_lossy(&body);
    assert!(
        body.contains("POST signature authentication is disabled by server configuration"),
        "the rejection must come from the POST signature switch, got: {body}"
    );
}

#[tokio::test]
async fn disabling_post_signatures_leaves_anonymous_forms_untouched() {
    let s3: Arc<dyn crate::s3_trait::S3> = Arc::new(post_policy_test_helpers::TestS3NoOp);
    let auth = post_policy_test_helpers::create_test_auth();
    let disabled = post_form_config(|config| config.allow_post_signature = false);
    let ccx = post_policy_test_helpers::create_test_context(&s3, &disabled, &auth);

    // A form without any signature is not a POST *signature* request, so the switch must not
    // touch it: `check()` reports no credentials and the access policy decides.
    let boundary = "------------------------test12345678";
    let body = format!(
        "\r\n--{boundary}\r\n\
Content-Disposition: form-data; name=\"key\"\r\n\r\n\
anonymous.txt\r\n\
--{boundary}\r\n\
Content-Disposition: form-data; name=\"file\"; filename=\"anonymous.txt\"\r\n\
Content-Type: text/plain\r\n\r\n\
data\r\n\
--{boundary}--\r\n"
    );
    let mut req = Request::from(
        hyper::Request::builder()
            .method(Method::POST)
            .version(Version::HTTP_11)
            .uri("http://localhost/test-bucket")
            .header(crate::header::HOST, "localhost")
            .header(hyper::header::CONTENT_TYPE, format!("multipart/form-data; boundary={boundary}"))
            .header(hyper::header::CONTENT_LENGTH, body.len())
            .body(Body::from(body))
            .unwrap(),
    );
    let response = super::call(&mut req, &ccx).await.expect("ops::call serializes errors");
    let body = response
        .body
        .bytes()
        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned());
    assert!(
        !body
            .as_deref()
            .unwrap_or_default()
            .contains("disabled by server configuration"),
        "an anonymous form must not be rejected by the POST signature switch, got: {body:?}"
    );
}

/// Builds a header-authenticated request whose `SignedHeaders` is exactly `host;x-amz-date`: the
/// canonical request carries `payload_sha256` as its payload line while the request also sends
/// `x-amz-content-sha256: header_value`, which is deliberately left out of the signed-header list.
#[allow(clippy::too_many_arguments)]
fn header_auth_without_signed_content_sha256(
    method: Method,
    version: Version,
    uri: &str,
    payload_sha256: &str,
    header_value: &str,
    content_length: u64,
    body: Body,
) -> Request {
    let uri = uri.parse::<Uri>().unwrap();
    let amz_date = s3s_sigv4::AmzDate::parse(AMZ_DATE).unwrap();
    let amz_date_str = amz_date.fmt_iso8601();
    let host = uri.authority().unwrap().as_str();
    let signed_headers = vec![("host", host), ("x-amz-date", amz_date_str.as_str())];
    let signed_header_names = signed_headers.iter().map(|(name, _)| *name).collect::<Vec<_>>().join(";");
    let payload = if payload_sha256 == UNSIGNED_PAYLOAD {
        s3s_sigv4::Payload::Unsigned
    } else {
        s3s_sigv4::Payload::SingleChunk(payload_sha256)
    };
    let empty_query = &[] as &[(String, String)];
    let canonical_request =
        s3s_sigv4::create_canonical_request(method.as_str(), uri.path(), empty_query, signed_headers, payload);
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
        .header(crate::header::X_AMZ_CONTENT_SHA256, header_value)
        .header(crate::header::X_AMZ_DATE, AMZ_DATE)
        .header(crate::header::AUTHORIZATION, authorization)
        .header(hyper::header::CONTENT_LENGTH, content_length);
    if version == Version::HTTP_11 {
        builder = builder.header(crate::header::HOST, host);
    }
    Request::from(builder.body(body).unwrap())
}

/// Digest of `hello`, used as a value that differs from the signed payload line.
fn other_body_digest() -> String {
    crate::utils::crypto::hex_sha256(b"hello", str::to_owned)
}

/// `x-amz-content-sha256` is exempt from the unsigned-header check, and the exemption is safe:
/// the value is bound through the payload line of the canonical request, not through
/// `SignedHeaders`. This pins the accepted shape.
#[tokio::test]
async fn unsigned_content_sha256_is_still_accepted_on_header_auth() {
    const URI: &str = "http://localhost/test-bucket/test-key.txt";
    let test_s3 = Arc::new(TestS3::default());
    let s3: Arc<dyn crate::s3_trait::S3> = test_s3.clone();
    let config = test_config();
    let auth = SimpleAuth::from_single(ACCESS_KEY, SECRET_KEY);
    let ccx = test_context(&s3, &config, &auth);

    // Control: the same request with the header listed in SignedHeaders.
    let mut signed = signed_request(Method::PUT, Version::HTTP_11, URI, EMPTY_SHA256, &[]);
    signed
        .headers
        .insert(hyper::header::CONTENT_LENGTH, hyper::header::HeaderValue::from_static("0"));
    let response = super::call(&mut signed, &ccx).await.expect("ops::call serializes errors");
    assert!(response.status.is_success(), "signed control got {:?}", response.status);

    let mut unsigned = header_auth_without_signed_content_sha256(
        Method::PUT,
        Version::HTTP_11,
        URI,
        EMPTY_SHA256,
        EMPTY_SHA256,
        0,
        empty_unknown_length_body(),
    );
    let response = super::call(&mut unsigned, &ccx).await.expect("ops::call serializes errors");
    assert!(
        response.status.is_success(),
        "an x-amz-content-sha256 outside SignedHeaders must be accepted while it matches the payload line, got {:?}",
        response.status
    );
    assert_eq!(test_s3.put_object.load(Ordering::SeqCst), 2, "both requests must reach PutObject");
}

/// The exemption cannot be used to swap the declared payload hash: changing the value changes the
/// payload line, so the signature stops verifying.
#[tokio::test]
async fn changed_content_sha256_is_rejected_on_header_auth() {
    const URI: &str = "http://localhost/test-bucket/test-key.txt";
    let test_s3 = Arc::new(TestS3::default());
    let s3: Arc<dyn crate::s3_trait::S3> = test_s3.clone();
    let config = test_config();
    let auth = SimpleAuth::from_single(ACCESS_KEY, SECRET_KEY);
    let ccx = test_context(&s3, &config, &auth);

    let changed = other_body_digest();
    let mut req = header_auth_without_signed_content_sha256(
        Method::PUT,
        Version::HTTP_11,
        URI,
        EMPTY_SHA256,
        changed.as_str(),
        0,
        empty_unknown_length_body(),
    );
    let response = super::call(&mut req, &ccx).await.expect("ops::call serializes errors");
    assert_eq!(response.status, StatusCode::FORBIDDEN);
    let body = response.body.bytes().expect("error body is buffered");
    assert!(
        std::str::from_utf8(&body)
            .expect("error body is UTF-8")
            .contains("<Code>SignatureDoesNotMatch</Code>"),
        "expected SignatureDoesNotMatch: {body:?}"
    );
    assert_eq!(test_s3.put_object.load(Ordering::SeqCst), 0, "the request must not be routed");
}

/// A presigned URL that lists `x-amz-content-sha256` in `X-Amz-SignedHeaders` and signs the digest
/// as its payload line binds the value: the declared value is accepted, a different value and a
/// missing header are both rejected.
#[tokio::test]
async fn presigned_content_sha256_is_bound_when_listed_in_signed_headers() {
    let test_s3 = Arc::new(TestS3::default());
    let s3: Arc<dyn crate::s3_trait::S3> = test_s3.clone();
    let config = test_config();
    let auth = SimpleAuth::from_single(ACCESS_KEY, SECRET_KEY);
    let ccx = test_context(&s3, &config, &auth);
    let uri = presigned_put_uri_with_headers_and_payload(&[("x-amz-content-sha256", EMPTY_SHA256)], EMPTY_SHA256);

    let mut declared = presigned_request(Version::HTTP_11, uri.clone(), &[("x-amz-content-sha256", EMPTY_SHA256)]);
    let response = super::call(&mut declared, &ccx).await.expect("ops::call serializes errors");
    assert!(response.status.is_success(), "the declared value must verify, got {:?}", response.status);
    assert_eq!(test_s3.put_object.load(Ordering::SeqCst), 1);

    let changed = other_body_digest();
    let mut tampered = presigned_request(Version::HTTP_11, uri.clone(), &[("x-amz-content-sha256", changed.as_str())]);
    let response = super::call(&mut tampered, &ccx).await.expect("ops::call serializes errors");
    assert_eq!(response.status, StatusCode::FORBIDDEN);
    let body = response.body.bytes().expect("error body is buffered");
    assert!(
        std::str::from_utf8(&body)
            .expect("error body is UTF-8")
            .contains("<Code>SignatureDoesNotMatch</Code>"),
        "a changed value must not verify: {body:?}"
    );

    let mut removed = presigned_request(Version::HTTP_11, uri, &[]);
    let response = super::call(&mut removed, &ccx).await.expect("ops::call serializes errors");
    assert_eq!(response.status, StatusCode::FORBIDDEN);
    let body = response.body.bytes().expect("error body is buffered");
    let body = String::from_utf8_lossy(&body);
    assert!(body.contains("<Code>SignatureDoesNotMatch</Code>"), "{body}");
    assert!(body.contains("missing signed header: x-amz-content-sha256"), "{body}");
    assert_eq!(test_s3.put_object.load(Ordering::SeqCst), 1, "only the declared request may upload");
}

/// Listing the header in `X-Amz-SignedHeaders` does not change the payload line: a URL signed over
/// `UNSIGNED-PAYLOAD` whose header lists a digest is rejected, as S3 rejects it. The shape comes
/// from presigners that put the header in the signed-header list while keeping the unsigned marker
/// in the payload line.
#[tokio::test]
async fn presigned_content_sha256_listed_but_not_the_payload_line_is_rejected() {
    let test_s3 = Arc::new(TestS3::default());
    let s3: Arc<dyn crate::s3_trait::S3> = test_s3.clone();
    let config = test_config();
    let auth = SimpleAuth::from_single(ACCESS_KEY, SECRET_KEY);
    let ccx = test_context(&s3, &config, &auth);

    let uri = presigned_put_uri_with_headers(&[("x-amz-content-sha256", EMPTY_SHA256)]);
    let mut req = presigned_request(Version::HTTP_11, uri, &[("x-amz-content-sha256", EMPTY_SHA256)]);
    let response = super::call(&mut req, &ccx).await.expect("ops::call serializes errors");
    assert_eq!(
        response.status,
        StatusCode::FORBIDDEN,
        "the payload line must decide the signature, not the signed-header list"
    );
    let body = response.body.bytes().expect("error body is buffered");
    assert!(
        std::str::from_utf8(&body)
            .expect("error body is UTF-8")
            .contains("<Code>SignatureDoesNotMatch</Code>"),
        "expected SignatureDoesNotMatch"
    );
    assert_eq!(test_s3.put_object.load(Ordering::SeqCst), 0, "a rejected request must not be routed");
}

/// The unsigned `UNSIGNED-PAYLOAD` marker is accepted as well, and the non-empty body reaches the
/// operation without a digest check, because the marker itself is bound through the payload line.
#[tokio::test]
async fn unsigned_unsigned_payload_marker_is_accepted_on_header_auth() {
    const URI: &str = "http://localhost/test-bucket/test-key.txt";
    let test_s3 = Arc::new(TestS3::default());
    let s3: Arc<dyn crate::s3_trait::S3> = test_s3.clone();
    let config = test_config();
    let auth = SimpleAuth::from_single(ACCESS_KEY, SECRET_KEY);
    let ccx = test_context(&s3, &config, &auth);

    let mut req = header_auth_without_signed_content_sha256(
        Method::PUT,
        Version::HTTP_11,
        URI,
        UNSIGNED_PAYLOAD,
        UNSIGNED_PAYLOAD,
        5,
        Body::from(b"hello".to_vec()),
    );
    let response = super::call(&mut req, &ccx).await.expect("ops::call serializes errors");
    assert!(
        response.status.is_success(),
        "the unsigned marker must be accepted, got {:?}",
        response.status
    );
    assert_eq!(test_s3.put_object.load(Ordering::SeqCst), 1);
}
