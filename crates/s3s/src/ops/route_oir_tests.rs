// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

//! OIR routing tests (`x-id` query).
//!
//! Contract under test: `prepare` resolves the client-declared operation
//! intent directly (skipping the full-router probing) by dispatching on the
//! (HTTP method, path shape) partition and matching the declared name
//! exactly. The declaration is authoritative — required query strings /
//! headers / query tags are validated later by `deserialize_http`. Unknown /
//! duplicate declarations, and declarations whose shape does not match, are
//! rejected with `InvalidRequest` (after authentication, so auth errors still
//! take precedence). When the feature is disabled the signal is ignored
//! entirely and normal routing applies.

use super::*;
use crate::config::{S3Config, S3ConfigProvider, StaticConfigProvider};
use crate::error::S3ErrorCode;
use crate::s3_trait::S3;
use hyper::Method;
use std::sync::Arc;

struct NoopS3;

#[async_trait::async_trait]
impl S3 for NoopS3 {}

struct CtxParts {
    s3: Arc<dyn S3>,
    config: Arc<dyn S3ConfigProvider>,
}

fn ccx(parts: &CtxParts) -> CallContext<'_> {
    CallContext {
        s3: &parts.s3,
        config: &parts.config,
        host: None,
        auth: None,
        access: None,
        route: None,
        validation: None,
    }
}

fn ctx() -> CtxParts {
    CtxParts {
        s3: Arc::new(NoopS3),
        config: Arc::new(StaticConfigProvider::default()),
    }
}

fn ctx_with_operation_id_routing(enabled: bool) -> CtxParts {
    let config = S3Config {
        operation_id_routing: enabled,
        ..Default::default()
    };
    CtxParts {
        s3: Arc::new(NoopS3),
        config: Arc::new(StaticConfigProvider::new(Arc::new(config))),
    }
}

fn build_request(method: &str, uri_path: &str, headers: &[(&str, &str)]) -> Request {
    let mut builder = hyper::Request::builder()
        .method(Method::from_bytes(method.as_bytes()).unwrap())
        .uri(format!("http://localhost{uri_path}"))
        .header(crate::header::HOST, "localhost");
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    Request::from(builder.body(Body::empty()).unwrap())
}

async fn expect_op(parts: &CtxParts, req: &mut Request, expected: &str) {
    let ans = prepare(req, &ccx(parts))
        .await
        .unwrap_or_else(|e| panic!("{expected}: unexpected {e:?}"));
    match ans {
        Prepare::S3(op) => assert_eq!(op.name(), expected),
        Prepare::CustomRoute => panic!("{expected}: unexpected custom route"),
    }
}

async fn expect_invalid_request(parts: &CtxParts, req: &mut Request, why: &str) {
    let err = prepare(req, &ccx(parts)).await.err().expect("expected error");
    assert_eq!(*err.code(), S3ErrorCode::InvalidRequest, "{why}: {err:?}");
}

#[tokio::test]
async fn x_id_oir_get_object() {
    let parts = ctx();
    let mut req = build_request("GET", "/bucket/key?x-id=GetObject", &[]);
    expect_op(&parts, &mut req, "GetObject").await;
}

#[tokio::test]
async fn x_id_deep_probing_analytics_get() {
    // `analytics` + `id` confirms GetBucketAnalyticsConfiguration without the
    // full shared-tag probing chain.
    let parts = ctx();
    let mut req = build_request("GET", "/bucket?analytics&id=cfg1&x-id=GetBucketAnalyticsConfiguration", &[]);
    expect_op(&parts, &mut req, "GetBucketAnalyticsConfiguration").await;
}

#[tokio::test]
async fn unknown_operation_rejected() {
    let parts = ctx();
    let mut req = build_request("GET", "/bucket/key?x-id=NoSuchOperation", &[]);
    expect_invalid_request(&parts, &mut req, "unknown operation id").await;
}

#[tokio::test]
async fn wrong_method_rejected() {
    // GetObject requires GET; a PUT declaration must not be silently adopted.
    let parts = ctx();
    let mut req = build_request("PUT", "/bucket/key?x-id=GetObject", &[]);
    expect_invalid_request(&parts, &mut req, "method mismatch").await;
}

#[tokio::test]
async fn missing_required_query_deferred_to_deserialize() {
    // GetBucketAnalyticsConfiguration requires the `id` query. The OIR path
    // no longer performs shape confirmation: the declaration is authoritative
    // and the missing required query is validated later by `deserialize_http`.
    let parts = ctx();
    let mut req = build_request("GET", "/bucket?analytics&x-id=GetBucketAnalyticsConfiguration", &[]);
    expect_op(&parts, &mut req, "GetBucketAnalyticsConfiguration").await;
}

#[tokio::test]
async fn duplicate_x_id_rejected() {
    let parts = ctx();
    let mut req = build_request("GET", "/bucket/key?x-id=GetObject&x-id=PutObject", &[]);
    expect_invalid_request(&parts, &mut req, "duplicate x-id").await;
}

#[tokio::test]
async fn required_header_deferred_to_deserialize() {
    // CopyObject requires `x-amz-copy-source`. The OIR path dispatches the
    // declared operation; the header requirement is enforced by deserialize.
    let parts = ctx();
    let mut req = build_request("PUT", "/bucket/key?x-id=CopyObject", &[]);
    expect_op(&parts, &mut req, "CopyObject").await;
}

#[tokio::test]
async fn config_disabled_falls_back_to_normal_routing() {
    let parts = ctx_with_operation_id_routing(false);

    // A bogus declaration that would be rejected on the OIR path is ignored
    // entirely when the feature is off: normal routing picks PutObject.
    let mut req = build_request("PUT", "/bucket/key?x-id=GetObject", &[]);
    expect_op(&parts, &mut req, "PutObject").await;

    // Real signals are also ignored.
    let mut req = build_request("GET", "/bucket/key?x-id=GetObject", &[]);
    expect_op(&parts, &mut req, "GetObject").await;
}

#[tokio::test]
async fn no_signal_uses_normal_routing() {
    let parts = ctx();
    let mut req = build_request("GET", "/bucket/key", &[]);
    expect_op(&parts, &mut req, "GetObject").await;
}

#[tokio::test]
#[cfg(feature = "minio")]
async fn minio_only_operation_resolves_via_x_id() {
    // MinIO-only operations participate in OIR: a client that sends `x-id`
    // uniformly must not have its minio-only requests rejected.
    let parts = ctx();
    let mut req = build_request("GET", "/bucket?events=s3:ObjectCreated:*&x-id=ListenBucketNotification", &[]);
    expect_op(&parts, &mut req, "ListenBucketNotification").await;
}

#[tokio::test]
#[cfg(feature = "minio")]
async fn minio_only_operation_without_x_id_routes_normally() {
    let parts = ctx();
    let mut req = build_request("GET", "/bucket?events=s3:ObjectCreated:*", &[]);
    expect_op(&parts, &mut req, "ListenBucketNotification").await;
}
