// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

//! Object key length: a request-target key is limited to 1024 bytes.

use super::common::*;

use crate::auth::SimpleAuth;

use hyper::{Method, StatusCode, Version};
use std::sync::Arc;
use std::sync::atomic::Ordering;

const BUCKET: &str = "test-bucket";

fn uri_with_key_of(bytes: usize) -> String {
    format!("http://localhost/{BUCKET}/{}", "a".repeat(bytes))
}

#[tokio::test]
async fn request_target_key_at_the_byte_limit_is_dispatched() {
    let test_s3 = Arc::new(TestS3::default());
    let s3: Arc<dyn crate::s3_trait::S3> = test_s3.clone();
    let config = test_config();
    let auth = SimpleAuth::from_single(ACCESS_KEY, SECRET_KEY);
    let ccx = test_context(&s3, &config, &auth);

    let uri = uri_with_key_of(1024);
    let mut req = signed_request(Method::GET, Version::HTTP_11, &uri, EMPTY_SHA256, &[]);
    let response = super::call(&mut req, &ccx).await.expect("a 1024-byte key must be routed");
    assert!(response.status.is_success(), "a 1024-byte key got {:?}", response.status);
    assert_eq!(test_s3.get_object.load(Ordering::SeqCst), 1, "the operation must be dispatched");
}

#[tokio::test]
async fn request_target_key_over_the_byte_limit_is_rejected_before_dispatch() {
    let test_s3 = Arc::new(TestS3::default());
    let s3: Arc<dyn crate::s3_trait::S3> = test_s3.clone();
    let config = test_config();
    let auth = SimpleAuth::from_single(ACCESS_KEY, SECRET_KEY);
    let ccx = test_context(&s3, &config, &auth);

    let uri = uri_with_key_of(1025);
    let mut req = signed_request(Method::GET, Version::HTTP_11, &uri, EMPTY_SHA256, &[]);
    let response = super::call(&mut req, &ccx).await.expect("a 1025-byte key must be answered");
    assert_eq!(response.status, StatusCode::BAD_REQUEST, "a 1025-byte key got {:?}", response.status);
    let body = response.body.bytes().expect("error body is buffered");
    let body = std::str::from_utf8(&body).expect("error body is UTF-8");
    assert!(body.contains("<Code>KeyTooLongError</Code>"), "unexpected error body: {body:?}");
    assert_eq!(
        test_s3.get_object.load(Ordering::SeqCst),
        0,
        "the implementation must not be reached for an over-long key"
    );
}
