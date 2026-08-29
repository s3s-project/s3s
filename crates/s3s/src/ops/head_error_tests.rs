// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

use super::*;

use crate::ops;

use http_body::Body;
use hyper::Method;

#[test]
fn head_error_response_is_bodyless() {
    let res = ops::serialize_error_for_method(&Method::HEAD, s3_error!(NoSuchKey), false).unwrap();
    assert_eq!(res.status, hyper::StatusCode::NOT_FOUND);
    assert!(Body::is_end_stream(&res.body), "HEAD response body must be empty");
    assert!(!res.headers.contains_key(hyper::header::TRANSFER_ENCODING), "{:?}", res.headers);
}

#[test]
fn get_error_response_keeps_xml_body() {
    let res = ops::serialize_error_for_method(&Method::GET, s3_error!(NoSuchKey), false).unwrap();
    assert!(!Body::is_end_stream(&res.body), "GET response must keep the XML body");
    assert_eq!(res.headers.get(hyper::header::CONTENT_TYPE).unwrap(), "application/xml");
}

#[test]
fn head_not_modified_is_bodyless() {
    let res = ops::serialize_error_for_method(&Method::HEAD, s3_error!(NotModified), false).unwrap();
    assert!(Body::is_end_stream(&res.body));
}
