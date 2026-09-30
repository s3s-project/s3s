// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

//! Independent SDK fixtures exercise the public service and custom-route boundary.

use http::{Extensions, HeaderMap, Method, Request, StatusCode, Uri};
use http_body_util::BodyExt;
use s3s::auth::{SigV4PathEncoding, SimpleAuth};
use s3s::config::{S3Config, StaticConfigProvider};
use s3s::dto::{GetObjectInput, GetObjectOutput};
use s3s::route::S3Route;
use s3s::service::{S3Service, S3ServiceBuilder};
use s3s::{Body, S3, S3Request, S3Response, S3Result};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::sync::Arc;

const ACCESS_KEY: &str = "catalog-test-access";
const SECRET_KEY: &str = "catalog-test-secret";

#[derive(Deserialize)]
struct Fixtures {
    requests: Vec<Fixture>,
}

#[derive(Deserialize)]
struct Fixture {
    name: String,
    method: String,
    uri: String,
    headers: BTreeMap<String, String>,
    body: String,
}

fn fixture(name: &str) -> Request<Body> {
    // Captured from botocore 1.42.97 independently of s3s canonicalization.
    // The streaming fixture uses the AWS chunk HMAC chain after SigV4Auth.
    // Only this test service accepts the fixed fixture clock.
    let fixtures: Fixtures = serde_json::from_str(include_str!("fixtures/sigv4_path_encoding.json")).unwrap();
    let fixture = fixtures.requests.into_iter().find(|fixture| fixture.name == name).unwrap();
    let mut builder = Request::builder().method(fixture.method.as_str()).uri(fixture.uri);
    for (name, value) in fixture.headers {
        builder = builder.header(name, value);
    }
    builder.body(Body::from(fixture.body)).unwrap()
}

#[derive(Clone)]
struct OriginalRequest {
    uri: Uri,
    authorization: Option<http::HeaderValue>,
}

struct MissingTableRoute {
    encoding: Option<SigV4PathEncoding>,
}

#[async_trait::async_trait]
impl S3Route for MissingTableRoute {
    fn is_match(&self, _: &Method, uri: &Uri, headers: &HeaderMap, extensions: &mut Extensions) -> bool {
        // Also set the override on unmatched requests to exercise the native S3 boundary.
        if let Some(encoding) = self.encoding {
            extensions.insert(encoding);
        }
        extensions.insert(OriginalRequest {
            uri: uri.clone(),
            authorization: headers.get("authorization").cloned(),
        });
        uri.path().starts_with("/iceberg/v1/") || uri.path().starts_with("/_iceberg/v1/")
    }

    async fn call(&self, req: S3Request<Body>) -> S3Result<S3Response<Body>> {
        assert_eq!(req.credentials.as_ref().unwrap().access_key, ACCESS_KEY);
        let original = req.extensions.get::<OriginalRequest>().unwrap();
        assert_eq!(req.uri, original.uri);
        assert_eq!(req.headers.get("authorization"), original.authorization.as_ref());
        // Consuming the body also exercises payload and streaming signature checks.
        req.input.collect().await.map_err(|_| s3s::s3_error!(InvalidRequest))?;
        let mut response = S3Response::new(Body::from(
            r#"{"error":{"type":"NoSuchTableException","message":"table not found","code":404}}"#.to_owned(),
        ));
        response.status = Some(StatusCode::NOT_FOUND);
        Ok(response)
    }
}

struct TestS3;

#[async_trait::async_trait]
impl S3 for TestS3 {
    async fn get_object(&self, _: S3Request<GetObjectInput>) -> S3Result<S3Response<GetObjectOutput>> {
        Ok(S3Response::new(GetObjectOutput::default()))
    }
}

fn service(encoding: Option<SigV4PathEncoding>) -> S3Service {
    let mut builder = S3ServiceBuilder::new(TestS3);
    builder.set_auth(SimpleAuth::from_single(ACCESS_KEY, SECRET_KEY));
    builder.set_route(MissingTableRoute { encoding });
    let mut config = S3Config::default();
    config.presigned_url_max_skew_time_secs = u32::MAX;
    builder.set_config(Arc::new(StaticConfigProvider::new(Arc::new(config))));
    builder.build()
}

async fn assert_response(service: &S3Service, request: Request<Body>, status: StatusCode, expected: &str) {
    let uri = request.uri().clone();
    let response = service.call(request).await.unwrap();
    let actual = response.status();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let body = String::from_utf8_lossy(&body);
    assert_eq!(actual, status, "{uri}: {body}");
    assert!(body.contains(expected), "{body}");
}

#[tokio::test]
async fn metadata_table_probe_accepts_double_encoded_signature() {
    let service = service(Some(SigV4PathEncoding::DoubleEncoded));
    assert_response(&service, fixture("generic_probe"), StatusCode::NOT_FOUND, "NoSuchTableException").await;
}

#[tokio::test]
async fn encoding_modes_verify_only_the_selected_canonical_uri() {
    for encoding in [None, Some(SigV4PathEncoding::S3), Some(SigV4PathEncoding::DoubleEncoded)] {
        let service = service(encoding);
        let double = encoding == Some(SigV4PathEncoding::DoubleEncoded);
        for (name, accepts_double) in [
            ("generic_probe", true),
            ("legacy_probe", false),
            ("generic_presigned", true),
            ("legacy_presigned", false),
        ] {
            if double == accepts_double {
                assert_response(&service, fixture(name), StatusCode::NOT_FOUND, "NoSuchTableException").await;
            } else {
                assert_response(&service, fixture(name), StatusCode::FORBIDDEN, "SignatureDoesNotMatch").await;
            }
        }
        assert_response(&service, fixture("generic_base"), StatusCode::NOT_FOUND, "NoSuchTableException").await;
    }
}

#[tokio::test]
async fn unmatched_routes_keep_native_s3_encoding() {
    let service = service(Some(SigV4PathEncoding::DoubleEncoded));
    assert_response(&service, fixture("generic_s3_path"), StatusCode::FORBIDDEN, "SignatureDoesNotMatch").await;
    assert_response(&service, fixture("legacy_s3_path"), StatusCode::OK, "").await;
}

#[tokio::test]
async fn double_encoded_mode_preserves_payload_verification() {
    let service = service(Some(SigV4PathEncoding::DoubleEncoded));
    for name in ["generic_commit", "generic_streaming"] {
        assert_response(&service, fixture(name), StatusCode::NOT_FOUND, "NoSuchTableException").await;
        let mut request = fixture(name);
        let body = std::mem::replace(request.body_mut(), Body::empty())
            .collect()
            .await
            .unwrap()
            .to_bytes();
        *request.body_mut() = Body::from(String::from_utf8(body.to_vec()).unwrap().replace('{', "["));
        assert_response(&service, request, StatusCode::BAD_REQUEST, "InvalidRequest").await;
    }
}
