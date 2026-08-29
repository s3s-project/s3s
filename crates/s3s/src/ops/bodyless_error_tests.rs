// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

use super::*;

use crate::ops;

use http_body::Body;

fn xml_body(err: S3Error) -> String {
    let res = ops::serialize_error(err, false).unwrap();
    let bytes = res.body.bytes().unwrap();
    String::from_utf8(bytes.to_vec()).unwrap()
}

fn assert_empty_body(err: S3Error) -> crate::http::Response {
    let res = ops::serialize_error(err, false).unwrap();
    assert!(Body::is_end_stream(&res.body), "body must be empty");
    assert!(!res.headers.contains_key(hyper::header::CONTENT_LENGTH), "{:?}", res.headers);
    assert!(!res.headers.contains_key(hyper::header::CONTENT_TYPE), "{:?}", res.headers);
    assert!(!res.headers.contains_key(hyper::header::TRANSFER_ENCODING), "{:?}", res.headers);
    res
}

#[test]
fn not_modified_error_is_bodyless() {
    let res = assert_empty_body(s3_error!(NotModified));
    assert_eq!(res.status, hyper::StatusCode::NOT_MODIFIED);
}

#[test]
fn no_content_status_is_bodyless() {
    let mut err = s3_error!(NoSuchKey);
    err.set_status_code(hyper::StatusCode::NO_CONTENT);
    let res = assert_empty_body(err);
    assert_eq!(res.status, hyper::StatusCode::NO_CONTENT);
}

#[test]
fn informational_status_is_bodyless() {
    let mut err = s3_error!(NoSuchKey);
    err.set_status_code(hyper::StatusCode::CONTINUE);
    let res = assert_empty_body(err);
    assert_eq!(res.status, hyper::StatusCode::CONTINUE);
}

#[test]
fn reset_content_status_is_bodyless() {
    let mut err = s3_error!(NoSuchKey);
    err.set_status_code(hyper::StatusCode::RESET_CONTENT);
    let res = assert_empty_body(err);
    assert_eq!(res.status, hyper::StatusCode::RESET_CONTENT);
}

#[test]
fn to_http_response_is_bodyless_for_not_modified() {
    let res = s3_error!(NotModified).to_http_response().unwrap();
    assert!(Body::is_end_stream(res.body()));
    assert_eq!(res.status(), hyper::StatusCode::NOT_MODIFIED);
}

#[test]
fn regular_error_keeps_xml_body() {
    let xml = xml_body(s3_error!(NoSuchKey));
    assert!(xml.contains("<Code>NoSuchKey</Code>"), "{xml}");
}

#[test]
fn bodyless_error_preserves_custom_headers() {
    let mut err = s3_error!(NotModified);
    err.set_headers({
        let mut headers = hyper::HeaderMap::new();
        headers.insert(crate::header::X_AMZ_REQUEST_ID, "test-request-id".parse().unwrap());
        headers
    });
    let res = assert_empty_body(err);
    assert_eq!(res.headers.get(crate::header::X_AMZ_REQUEST_ID).unwrap(), "test-request-id");
}
