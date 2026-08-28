// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

//! `ListenBucketNotification` is a `MinIO` extension with no aws-sdk-s3
//! counterpart, so the generated proxy cannot forward it. It is forwarded
//! through the official `MinIO` SDK (`minio` crate) instead, streaming the
//! upstream response body through untouched.
//!
//! The generated proxy delegates to [`listen_bucket_notification`].

use hyper::HeaderMap;
use hyper::header::HeaderValue;
use minio::s3::MinioClient;
use minio::s3::multimap_ext::{Multimap, MultimapExt};
use minio::s3::types::ToS3Request;

use s3s::dto::{ListenBucketNotificationInput, ListenBucketNotificationOutput, StreamingBlob};
use s3s::{S3Result, s3_error};

pub async fn listen_bucket_notification(
    minio: &MinioClient,
    req: s3s::S3Request<ListenBucketNotificationInput>,
) -> S3Result<s3s::S3Response<ListenBucketNotificationOutput>> {
    let input = req.input;
    tracing::debug!(?input);

    // `prefix` and `suffix` go through `extra_query_params` so that the typed
    // builder chain stays unconditional.
    let mut extra_params = Multimap::default();
    if let Some(ref prefix) = input.prefix {
        extra_params.add("prefix", prefix);
    }
    if let Some(ref suffix) = input.suffix {
        extra_params.add("suffix", suffix);
    }

    let events: Vec<String> = match input.events {
        Some(ref events) => events
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .collect(),
        None => ["s3:ObjectCreated:*", "s3:ObjectRemoved:*", "s3:ObjectAccessed:*"]
            .into_iter()
            .map(str::to_owned)
            .collect(),
    };

    let builder = minio
        .listen_bucket_notification(input.bucket.as_str())
        .map_err(|e| s3_error!(e, InternalError, "invalid bucket"))?;
    let value = builder.extra_query_params(extra_params).events(events).build();
    let mut s3_request = value
        .to_s3request()
        .map_err(|e| s3_error!(e, InternalError, "failed to build request"))?;
    let resp = s3_request
        .execute()
        .await
        .map_err(|e| s3_error!(e, InternalError, "failed to send request"))?;

    let status = resp.status();
    if !status.is_success() {
        let message = resp.text().await.unwrap_or_else(|_| String::from("<no response body>"));
        return Err(s3_error!(InternalError, "upstream error: {status}: {message}"));
    }

    let mut headers = HeaderMap::new();
    for name in [hyper::header::CONTENT_TYPE, hyper::header::CACHE_CONTROL] {
        if let Some(value) = resp.headers().get(&name) {
            let value =
                HeaderValue::from_bytes(value.as_bytes()).map_err(|e| s3_error!(e, InternalError, "invalid upstream header"))?;
            headers.insert(name, value);
        }
    }

    let stream = resp.bytes_stream();
    let payload = StreamingBlob::wrap(stream);

    Ok(s3s::S3Response::with_headers(
        ListenBucketNotificationOutput { payload: Some(payload) },
        headers,
    ))
}
