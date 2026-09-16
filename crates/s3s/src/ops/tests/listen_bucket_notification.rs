// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

//! `MinIO` `ListenBucketNotification` routing (`feature = "minio"`).

use super::*;

use crate::ops::generated::resolve_route;
use crate::path::S3Path;

fn make_get_request(uri: &str) -> crate::http::Request {
    crate::http::Request::from(
        hyper::Request::builder()
            .method(Method::GET)
            .uri(uri)
            .body(crate::http::Body::empty())
            .unwrap(),
    )
}

fn resolve(uri: &str) -> &'static str {
    let req = make_get_request(uri);
    let path = S3Path::Bucket { bucket: "bucket".into() };
    let query = req.uri.query().unwrap_or_default();
    let pairs: Vec<(String, String)> = serde_urlencoded::from_str(query).unwrap();
    let qs = crate::http::OrderedQs::from_vec_unchecked(pairs);
    resolve_route(&req, &path, Some(&qs)).unwrap().name()
}

#[test]
fn route_events_to_listen_bucket_notification() {
    // The `events` query parameter is the MinIO ListenBucketNotification extension.
    assert_eq!(resolve("http://localhost/bucket?events=s3:ObjectCreated:*"), "ListenBucketNotification");
    // Values are not matched, only presence.
    assert_eq!(resolve("http://localhost/bucket?events"), "ListenBucketNotification");
    // Extra query parameters do not change the outcome.
    assert_eq!(
        resolve("http://localhost/bucket?events=s3:ObjectCreated:*&prefix=zz&suffix=yy"),
        "ListenBucketNotification"
    );
}

#[test]
fn deserialize_repeated_events_keys() {
    // Clients like `mc watch` send one `events` key per event name.
    let mut req = make_get_request(
        "http://localhost/bucket?events=s3:ObjectCreated:*&events=s3:ObjectRemoved:*&events=s3:ObjectAccessed:*",
    );
    let query = req.uri.query().unwrap_or_default();
    let pairs: Vec<(String, String)> = serde_urlencoded::from_str(query).unwrap();
    req.s3ext.qs = Some(crate::http::OrderedQs::from_vec_unchecked(pairs));
    req.s3ext.s3_path = Some(S3Path::Bucket { bucket: "bucket".into() });
    let input = crate::ops::generated::ListenBucketNotification::deserialize_http(&mut req).unwrap();
    assert_eq!(input.events.as_deref(), Some("s3:ObjectCreated:*,s3:ObjectRemoved:*,s3:ObjectAccessed:*"));
    assert_eq!(input.prefix.as_deref(), None);
}

#[test]
fn route_bucket_without_events_unchanged() {
    // No `events` parameter: falls through to the usual bucket listing ops.
    assert_eq!(resolve("http://localhost/bucket"), "ListObjects");
    assert_eq!(resolve("http://localhost/bucket?list-type=2"), "ListObjectsV2");
    assert_eq!(resolve("http://localhost/bucket?versions"), "ListObjectVersions");
}
