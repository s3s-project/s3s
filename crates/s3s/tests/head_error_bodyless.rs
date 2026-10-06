// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

//! A `HEAD` request is answered without a body (RFC 9110 §9.3.2), also when the
//! error comes from the implementation rather than from the request pipeline.

use async_trait::async_trait;
use http::{Method, Request, StatusCode};
use http_body_util::BodyExt;
use s3s::dto::{GetObjectInput, GetObjectOutput, HeadBucketInput, HeadBucketOutput, HeadObjectInput, HeadObjectOutput};
use s3s::service::{S3Service, S3ServiceBuilder};
use s3s::{Body, S3, S3Error, S3ErrorCode, S3Request, S3Response, S3Result};

/// Every read reports a missing resource.
#[derive(Clone)]
struct MissingS3;

#[async_trait]
impl S3 for MissingS3 {
    async fn head_object(&self, _req: S3Request<HeadObjectInput>) -> S3Result<S3Response<HeadObjectOutput>> {
        Err(S3Error::new(S3ErrorCode::NoSuchKey))
    }

    async fn get_object(&self, _req: S3Request<GetObjectInput>) -> S3Result<S3Response<GetObjectOutput>> {
        Err(S3Error::new(S3ErrorCode::NoSuchKey))
    }

    async fn head_bucket(&self, _req: S3Request<HeadBucketInput>) -> S3Result<S3Response<HeadBucketOutput>> {
        Err(S3Error::new(S3ErrorCode::NoSuchBucket))
    }
}

async fn send(service: &S3Service, method: Method, uri: &str) -> (StatusCode, Vec<u8>) {
    let req = Request::builder()
        .method(method)
        .uri(uri)
        .body(Body::empty())
        .expect("request builds");
    let resp = service.call(req).await.expect("service call returns a response");
    let status = resp.status();
    let body = resp.into_body().collect().await.expect("body collects").to_bytes().to_vec();
    (status, body)
}

#[tokio::test]
async fn head_object_error_from_the_implementation_is_bodyless() {
    let service = S3ServiceBuilder::new(MissingS3).build();

    let (status, body) = send(&service, Method::HEAD, "/bucket/key").await;

    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(body.is_empty(), "a HEAD response carries no body: {body:?}");
}

#[tokio::test]
async fn head_bucket_error_from_the_implementation_is_bodyless() {
    let service = S3ServiceBuilder::new(MissingS3).build();

    let (status, body) = send(&service, Method::HEAD, "/bucket").await;

    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(body.is_empty(), "a HEAD response carries no body: {body:?}");
}

#[tokio::test]
async fn get_error_from_the_implementation_keeps_the_xml_body() {
    let service = S3ServiceBuilder::new(MissingS3).build();

    let (status, body) = send(&service, Method::GET, "/bucket/key").await;

    assert_eq!(status, StatusCode::NOT_FOUND);
    let text = String::from_utf8(body).expect("the error body is UTF-8");
    assert!(text.contains("<Code>NoSuchKey</Code>"), "a GET keeps the XML error body: {text}");
}
