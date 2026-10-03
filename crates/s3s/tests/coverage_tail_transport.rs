// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

//! Coverage tests for the hyper transport path: a real HTTP/1 connection hands
//! the service a request whose body is a hyper incoming body.

#![allow(clippy::too_many_lines)]

use std::sync::Arc;

use http::{Request, StatusCode};
use http_body_util::BodyExt;
use hyper::body::Incoming;
use hyper::service::service_fn;
use hyper_util::rt::TokioIo;
use s3s::config::{S3Config, StaticConfigProvider};
use s3s::dto::{GetObjectInput, GetObjectOutput, PutObjectInput, PutObjectOutput};
use s3s::service::{S3Service, S3ServiceBuilder};
use s3s::{S3, S3Error, S3ErrorCode, S3Request, S3Response, S3Result};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[derive(Clone)]
struct TestS3;

#[async_trait::async_trait]
impl S3 for TestS3 {
    async fn get_object(&self, _req: S3Request<GetObjectInput>) -> S3Result<S3Response<GetObjectOutput>> {
        Ok(S3Response::new(GetObjectOutput::default()))
    }

    async fn put_object(&self, mut req: S3Request<PutObjectInput>) -> S3Result<S3Response<PutObjectOutput>> {
        // Drain the incoming body so the transport body is polled to the end.
        let mut len = 0_usize;
        if let Some(body) = req.input.body.as_mut() {
            while let Some(chunk) = futures::StreamExt::next(body).await {
                len += chunk.map_err(|e| S3Error::with_source(S3ErrorCode::InternalError, e))?.len();
            }
        }
        assert_eq!(len, 5, "the incoming body must be readable");
        Ok(S3Response::new(PutObjectOutput::default()))
    }
}

#[tokio::test]
async fn incoming_request_bodies_flow_through_the_tower_service() {
    let mut builder = S3ServiceBuilder::new(TestS3);
    builder.set_config(Arc::new(StaticConfigProvider::new(Arc::new(S3Config::default()))));
    let service = builder.build();

    let (mut client, server) = tokio::io::duplex(8192);

    let handler = {
        let service = service.clone();
        service_fn(move |req: Request<Incoming>| {
            let mut service = service.clone();
            async move { <S3Service as tower::Service<Request<Incoming>>>::call(&mut service, req).await }
        })
    };

    let server_task = tokio::spawn(async move {
        hyper::server::conn::http1::Builder::new()
            .serve_connection(TokioIo::new(server), handler)
            .await
    });

    client
        .write_all(b"PUT /bucket/key HTTP/1.1\r\nHost: s3.example.com\r\nContent-Length: 5\r\nConnection: close\r\n\r\nhello")
        .await
        .expect("request is written");

    let mut response = Vec::new();
    client.read_to_end(&mut response).await.expect("response is read");
    let response = String::from_utf8(response).expect("utf-8 response");
    assert!(response.starts_with("HTTP/1.1 200"), "{response}");

    server_task
        .await
        .expect("server task completes")
        .expect("connection completes");

    // A second round trip reuses the buffered response path of the service.
    let resp = service
        .call(
            Request::builder()
                .method(http::Method::GET)
                .uri("/bucket/key")
                .body(s3s::Body::empty())
                .expect("valid request"),
        )
        .await
        .expect("response");
    assert_eq!(resp.status(), StatusCode::OK);
    let _ = resp.into_body().collect().await.expect("body collects");

    // A chunked request carries no content length, so the length of the
    // incoming body has to be taken from the body itself.
    let chunked_status = chunked_upload().await;
    assert_eq!(chunked_status, StatusCode::OK);
}

async fn chunked_upload() -> StatusCode {
    let mut builder = S3ServiceBuilder::new(TestS3);
    builder.set_config(Arc::new(StaticConfigProvider::new(Arc::new(S3Config::default()))));
    let service = builder.build();

    let (mut client, server) = tokio::io::duplex(8192);
    let handler = {
        let service = service.clone();
        service_fn(move |req: Request<Incoming>| {
            let mut service = service.clone();
            async move { <S3Service as tower::Service<Request<Incoming>>>::call(&mut service, req).await }
        })
    };
    let server_task = tokio::spawn(async move {
        hyper::server::conn::http1::Builder::new()
            .serve_connection(TokioIo::new(server), handler)
            .await
    });

    client
        .write_all(
            b"PUT /bucket/key HTTP/1.1\r\nHost: s3.example.com\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n5\r\nhello\r\n0\r\n\r\n",
        )
        .await
        .expect("request is written");

    let mut response = Vec::new();
    client.read_to_end(&mut response).await.expect("response is read");
    let response = String::from_utf8(response).expect("utf-8 response");
    server_task
        .await
        .expect("server task completes")
        .expect("connection completes");

    let status_line = response.lines().next().unwrap_or_default();
    if status_line.starts_with("HTTP/1.1 200") {
        StatusCode::OK
    } else {
        panic!("unexpected response: {response}");
    }
}
