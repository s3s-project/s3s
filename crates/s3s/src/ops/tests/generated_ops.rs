// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

//! Route and serde coverage for newly added operations (`CreateSession`,
//! `ListDirectoryBuckets`).

use super::*;

#[test]
fn create_session_route_resolved() {
    use crate::http::{Body, OrderedQs};
    use crate::path::S3Path;

    let req = crate::http::Request::from(
        hyper::Request::builder()
            .method(Method::GET)
            .uri("http://localhost/my-bucket?session")
            .body(Body::empty())
            .unwrap(),
    );

    let s3_path = S3Path::Bucket {
        bucket: "my-bucket".into(),
    };
    let qs = OrderedQs::parse("session").unwrap();
    let op = generated::resolve_route(&req, &s3_path, Some(&qs)).unwrap();

    assert_eq!(op.name(), "CreateSession");
    assert!(!op.needs_full_body());
}

#[test]
fn create_session_deserialize_http() {
    use crate::http::Body;
    use crate::path::S3Path;

    let mut req = crate::http::Request::from(
        hyper::Request::builder()
            .method(Method::GET)
            .uri("http://localhost/my-bucket?session")
            .header("x-amz-create-session-mode", "ReadWrite")
            .body(Body::empty())
            .unwrap(),
    );

    req.s3ext.s3_path = Some(S3Path::Bucket {
        bucket: "my-bucket".into(),
    });

    let input = generated::CreateSession::deserialize_http(&mut req).unwrap();

    assert_eq!(input.bucket, "my-bucket");
    assert_eq!(input.session_mode.as_ref().map(crate::dto::SessionMode::as_str), Some("ReadWrite"));
    assert!(input.server_side_encryption.is_none());
    assert!(input.ssekms_key_id.is_none());
    assert!(input.ssekms_encryption_context.is_none());
    assert!(input.bucket_key_enabled.is_none());
}

#[test]
fn create_session_serialize_http() {
    use crate::dto::{CreateSessionOutput, SessionCredentials, Timestamp, TimestampFormat};

    let creds = SessionCredentials {
        access_key_id: "AKIAIOSFODNN7EXAMPLE".to_owned(),
        expiration: Timestamp::parse(TimestampFormat::DateTime, "2024-01-01T00:05:00.000Z").unwrap(),
        secret_access_key: "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY".to_owned(),
        session_token: "FwoGZXIvYXdzEBYaDHqa0A".to_owned(),
    };

    let output = CreateSessionOutput {
        credentials: creds,
        ..Default::default()
    };

    let resp = generated::CreateSession::serialize_http(output).unwrap();
    assert_eq!(resp.status, hyper::StatusCode::OK);
}

#[test]
fn list_directory_buckets_route_resolved() {
    use crate::http::{Body, OrderedQs};
    use crate::path::S3Path;

    let req = crate::http::Request::from(
        hyper::Request::builder()
            .method(Method::GET)
            .uri("http://localhost/?x-id=ListDirectoryBuckets")
            .body(Body::empty())
            .unwrap(),
    );

    let s3_path = S3Path::Root;
    let qs = OrderedQs::parse("x-id=ListDirectoryBuckets").unwrap();
    let op = generated::resolve_route(&req, &s3_path, Some(&qs)).unwrap();

    assert_eq!(op.name(), "ListDirectoryBuckets");
    assert!(!op.needs_full_body());
}

#[test]
fn list_buckets_route_still_default() {
    use crate::http::{Body, OrderedQs};
    use crate::path::S3Path;

    // With x-id=ListBuckets
    let req = crate::http::Request::from(
        hyper::Request::builder()
            .method(Method::GET)
            .uri("http://localhost/?x-id=ListBuckets")
            .body(Body::empty())
            .unwrap(),
    );

    let s3_path = S3Path::Root;
    let qs = OrderedQs::parse("x-id=ListBuckets").unwrap();
    let op = generated::resolve_route(&req, &s3_path, Some(&qs)).unwrap();
    assert_eq!(op.name(), "ListBuckets");

    // Without any query string
    let req2 = crate::http::Request::from(
        hyper::Request::builder()
            .method(Method::GET)
            .uri("http://localhost/")
            .body(Body::empty())
            .unwrap(),
    );
    let op2 = generated::resolve_route(&req2, &s3_path, None).unwrap();
    assert_eq!(op2.name(), "ListBuckets");
}

#[test]
fn list_directory_buckets_deserialize_http() {
    use crate::http::{Body, OrderedQs};

    let mut req = crate::http::Request::from(
        hyper::Request::builder()
            .method(Method::GET)
            .uri("http://localhost/?continuation-token=abc123&max-directory-buckets=10")
            .body(Body::empty())
            .unwrap(),
    );

    req.s3ext.s3_path = Some(crate::path::S3Path::Root);
    req.s3ext.qs = Some(OrderedQs::parse("continuation-token=abc123&max-directory-buckets=10").unwrap());

    let input = generated::ListDirectoryBuckets::deserialize_http(&mut req).unwrap();

    assert_eq!(input.continuation_token.as_deref(), Some("abc123"));
    assert_eq!(input.max_directory_buckets, Some(10));
}

#[test]
fn list_directory_buckets_serialize_http() {
    use crate::dto::ListDirectoryBucketsOutput;

    let output = ListDirectoryBucketsOutput { ..Default::default() };

    let resp = generated::ListDirectoryBuckets::serialize_http(output).unwrap();
    assert_eq!(resp.status, hyper::StatusCode::OK);
}

/// RFC 9110 requires a recipient to ignore an `If-Modified-Since` /
/// `If-Unmodified-Since` value that is not a valid HTTP date (§13.1.3, §13.1.4).
#[test]
fn get_object_ignores_invalid_conditional_dates() {
    use crate::http::Body;
    use crate::path::S3Path;

    let mut req = crate::http::Request::from(
        hyper::Request::builder()
            .method(Method::GET)
            .uri("http://localhost/my-bucket/my-key")
            .header("if-modified-since", "not-a-date")
            .header("if-unmodified-since", "not-a-date")
            .body(Body::empty())
            .unwrap(),
    );
    req.s3ext.s3_path = Some(S3Path::Object {
        bucket: "my-bucket".into(),
        key: "my-key".into(),
    });

    let input = generated::GetObject::deserialize_http(&mut req).expect("invalid conditional dates must be ignored");
    assert!(input.if_modified_since.is_none());
    assert!(input.if_unmodified_since.is_none());
}

#[test]
fn head_object_ignores_invalid_conditional_dates() {
    use crate::http::Body;
    use crate::path::S3Path;

    let mut req = crate::http::Request::from(
        hyper::Request::builder()
            .method(Method::HEAD)
            .uri("http://localhost/my-bucket/my-key")
            .header("if-modified-since", "not-a-date")
            .body(Body::empty())
            .unwrap(),
    );
    req.s3ext.s3_path = Some(S3Path::Object {
        bucket: "my-bucket".into(),
        key: "my-key".into(),
    });

    let input = generated::HeadObject::deserialize_http(&mut req).expect("invalid conditional date must be ignored");
    assert!(input.if_modified_since.is_none());
}

#[test]
fn get_object_parses_valid_conditional_dates() {
    use crate::http::Body;
    use crate::path::S3Path;

    let mut req = crate::http::Request::from(
        hyper::Request::builder()
            .method(Method::GET)
            .uri("http://localhost/my-bucket/my-key")
            .header("if-modified-since", "Wed, 21 Oct 2015 07:28:00 GMT")
            .body(Body::empty())
            .unwrap(),
    );
    req.s3ext.s3_path = Some(S3Path::Object {
        bucket: "my-bucket".into(),
        key: "my-key".into(),
    });

    let input = generated::GetObject::deserialize_http(&mut req).expect("a valid HTTP date must be parsed");
    assert!(input.if_modified_since.is_some());
}

/// The copy-source and rename-source conditionals are ignored the same way the standard
/// conditional dates are: an invalid value leaves the condition absent.
#[test]
fn copy_object_ignores_invalid_copy_source_conditional_dates() {
    use crate::http::Body;
    use crate::path::S3Path;

    let mut req = crate::http::Request::from(
        hyper::Request::builder()
            .method(Method::PUT)
            .uri("http://localhost/my-bucket/my-key")
            .header("x-amz-copy-source", "/source-bucket/source-key")
            .header("x-amz-copy-source-if-modified-since", "not-a-date")
            .header("x-amz-copy-source-if-unmodified-since", "not-a-date")
            .body(Body::empty())
            .unwrap(),
    );
    req.s3ext.s3_path = Some(S3Path::Object {
        bucket: "my-bucket".into(),
        key: "my-key".into(),
    });

    let input = generated::CopyObject::deserialize_http(&mut req).expect("invalid copy-source dates must be ignored");
    assert!(input.copy_source_if_modified_since.is_none());
    assert!(input.copy_source_if_unmodified_since.is_none());
}

/// A valid copy-source conditional date is still parsed.
#[test]
fn copy_object_parses_valid_copy_source_conditional_dates() {
    use crate::http::Body;
    use crate::path::S3Path;

    let mut req = crate::http::Request::from(
        hyper::Request::builder()
            .method(Method::PUT)
            .uri("http://localhost/my-bucket/my-key")
            .header("x-amz-copy-source", "/source-bucket/source-key")
            .header("x-amz-copy-source-if-modified-since", "Wed, 21 Oct 2015 07:28:00 GMT")
            .body(Body::empty())
            .unwrap(),
    );
    req.s3ext.s3_path = Some(S3Path::Object {
        bucket: "my-bucket".into(),
        key: "my-key".into(),
    });

    let input = generated::CopyObject::deserialize_http(&mut req).expect("a valid copy-source date must be parsed");
    assert!(input.copy_source_if_modified_since.is_some());
}

/// The rename-source conditionals follow the same rule.
#[test]
fn rename_object_ignores_invalid_rename_source_conditional_date() {
    use crate::http::Body;
    use crate::path::S3Path;

    let mut req = crate::http::Request::from(
        hyper::Request::builder()
            .method(Method::PUT)
            .uri("http://localhost/my-bucket/my-key?renameObject")
            .header("x-amz-rename-source", "/source-bucket/source-key")
            .header("x-amz-rename-source-if-modified-since", "not-a-date")
            .body(Body::empty())
            .unwrap(),
    );
    req.s3ext.s3_path = Some(S3Path::Object {
        bucket: "my-bucket".into(),
        key: "my-key".into(),
    });
    req.s3ext.qs = Some(crate::http::OrderedQs::from_vec_unchecked(vec![("renameObject".into(), String::new())]));

    let input = generated::RenameObject::deserialize_http(&mut req).expect("invalid rename-source dates must be ignored");
    assert!(input.source_if_modified_since.is_none());
}
