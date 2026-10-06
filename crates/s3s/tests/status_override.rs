// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

//! An implementation-set `S3Response::status` reaches the response on the
//! generated operation path.
//!
//! The modeled status of an operation is only the default: some answers can be
//! decided by the implementation alone (a ranged `HEAD` answers `206`, a
//! conditional read hit answers `304`). A status that must not carry a body
//! (`1xx`, `204`, `205` and `304`) is answered without one, as required by
//! RFC 9110 §6.4.1.

use async_trait::async_trait;
use bytes::Bytes;
use http::{HeaderMap, Method, Request};
use http_body_util::BodyExt;
use s3s::dto::{
    DeleteObjectInput, DeleteObjectOutput, GetObjectInput, GetObjectOutput, HeadObjectInput, HeadObjectOutput, StreamingBlob,
};
use s3s::service::{S3Service, S3ServiceBuilder};
use s3s::{Body, S3, S3Request, S3Response, S3Result};

use http::StatusCode;

/// Answers a ranged `HEAD` with the `206` the framework cannot derive: only
/// the implementation knows the object length and whether the range is
/// satisfiable.
#[derive(Clone)]
struct RangedHead;

#[async_trait]
impl S3 for RangedHead {
    async fn head_object(&self, _req: S3Request<HeadObjectInput>) -> S3Result<S3Response<HeadObjectOutput>> {
        Ok(S3Response::with_status(
            HeadObjectOutput {
                content_length: Some(4),
                content_range: Some("bytes 0-3/10".to_owned()),
                ..Default::default()
            },
            StatusCode::PARTIAL_CONTENT,
        ))
    }
}

/// Answers a conditional read hit with `304` while the output still carries the
/// body a plain `GET` would return.
#[derive(Clone)]
struct ConditionalHit;

#[async_trait]
impl S3 for ConditionalHit {
    async fn get_object(&self, _req: S3Request<GetObjectInput>) -> S3Result<S3Response<GetObjectOutput>> {
        Ok(S3Response::with_status(
            GetObjectOutput {
                content_length: Some(10),
                body: Some(StreamingBlob::from_bytes(Bytes::from_static(b"0123456789"))),
                ..Default::default()
            },
            StatusCode::NOT_MODIFIED,
        ))
    }
}

/// Sets the `content_range` that derives `206` and then overrides it with
/// `200`, so the explicit status wins over the derived one.
#[derive(Clone)]
struct DerivedThenOverridden;

#[async_trait]
impl S3 for DerivedThenOverridden {
    async fn get_object(&self, _req: S3Request<GetObjectInput>) -> S3Result<S3Response<GetObjectOutput>> {
        Ok(S3Response::with_status(
            GetObjectOutput {
                content_length: Some(4),
                content_range: Some("bytes 0-3/10".to_owned()),
                body: Some(StreamingBlob::from_bytes(Bytes::from_static(b"0123"))),
                ..Default::default()
            },
            StatusCode::OK,
        ))
    }
}

/// Leaves the status unset, so the modeled code of the operation is answered.
#[derive(Clone)]
struct ModeledStatus;

#[async_trait]
impl S3 for ModeledStatus {
    async fn delete_object(&self, _req: S3Request<DeleteObjectInput>) -> S3Result<S3Response<DeleteObjectOutput>> {
        Ok(S3Response::new(DeleteObjectOutput::default()))
    }
}

async fn send(service: &S3Service, method: Method, uri: &str) -> (StatusCode, HeaderMap, Vec<u8>) {
    let req = Request::builder()
        .method(method)
        .uri(uri)
        .body(Body::empty())
        .expect("request builds");
    let resp = service.call(req).await.expect("service call returns a response");
    let status = resp.status();
    let headers = resp.headers().clone();
    let body = resp.into_body().collect().await.expect("body collects").to_bytes().to_vec();
    (status, headers, body)
}

#[tokio::test]
async fn head_object_status_override_reaches_the_response() {
    let service = S3ServiceBuilder::new(RangedHead).build();

    let (status, headers, body) = send(&service, Method::HEAD, "/bucket/key").await;

    assert_eq!(status, StatusCode::PARTIAL_CONTENT);
    assert_eq!(headers.get("content-range").expect("content-range is set"), "bytes 0-3/10");
    assert!(body.is_empty(), "a HEAD response carries no body: {body:?}");
}

#[tokio::test]
async fn not_modified_status_override_is_bodyless() {
    let service = S3ServiceBuilder::new(ConditionalHit).build();

    let (status, headers, body) = send(&service, Method::GET, "/bucket/key").await;

    assert_eq!(status, StatusCode::NOT_MODIFIED);
    assert!(body.is_empty(), "304 must not carry a body: {body:?}");
    assert!(!headers.contains_key("content-length"), "content-length: {headers:?}");
    assert!(!headers.contains_key("content-type"), "content-type: {headers:?}");
}

#[tokio::test]
async fn explicit_status_wins_over_the_derived_one() {
    let service = S3ServiceBuilder::new(DerivedThenOverridden).build();

    let (status, headers, body) = send(&service, Method::GET, "/bucket/key").await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers.get("content-range").expect("content-range is set"), "bytes 0-3/10");
    assert_eq!(body, b"0123");
}

#[tokio::test]
async fn modeled_status_is_answered_without_an_override() {
    let service = S3ServiceBuilder::new(ModeledStatus).build();

    let (status, _headers, body) = send(&service, Method::DELETE, "/bucket/key").await;

    assert_eq!(status, StatusCode::NO_CONTENT);
    assert!(body.is_empty(), "204 must not carry a body: {body:?}");
}
