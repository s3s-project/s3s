// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

//! A server that routes on a rewritten path can supply the path the client signed
//! through [`SignedUriPath`]; the signature then binds that path while routing
//! keeps using the request URI.

use http::{Request, StatusCode};
use http_body_util::BodyExt;
use s3s::auth::{SignedUriPath, SimpleAuth};
use s3s::config::{S3Config, StaticConfigProvider};
use s3s::dto::{GetObjectInput, GetObjectOutput};
use s3s::service::{S3Service, S3ServiceBuilder};
use s3s::{Body, S3, S3Request, S3Response, S3Result};
use s3s_sigv4::{AmzDate, Payload};
use std::sync::{Arc, Mutex};

const ACCESS_KEY: &str = "signed-path-access";
const SECRET_KEY: &str = "signed-path-secret";
const HOST: &str = "s3.example.test";
const SIGNED_PATH: &str = "/prefix/bkt/k";
const ROUTED_PATH: &str = "/bkt/k";

type Seen = Arc<Mutex<Option<(String, String)>>>;

struct RecordingS3 {
    seen: Seen,
}

#[async_trait::async_trait]
impl S3 for RecordingS3 {
    async fn get_object(&self, req: S3Request<GetObjectInput>) -> S3Result<S3Response<GetObjectOutput>> {
        *self.seen.lock().unwrap() = Some((req.input.bucket, req.input.key));
        Ok(S3Response::new(GetObjectOutput::default()))
    }
}

fn service() -> (S3Service, Seen) {
    let seen = Seen::default();
    let mut builder = S3ServiceBuilder::new(RecordingS3 { seen: Arc::clone(&seen) });
    builder.set_auth(SimpleAuth::from_single(ACCESS_KEY, SECRET_KEY));
    let mut config = S3Config::default();
    config.enable_sig_v2 = true;
    builder.set_config(Arc::new(StaticConfigProvider::new(Arc::new(config))));
    (builder.build(), seen)
}

fn now_amz_date() -> String {
    jiff::Timestamp::now().strftime("%Y%m%dT%H%M%SZ").to_string()
}

fn now_http_date() -> String {
    jiff::Timestamp::now().strftime("%a, %d %b %Y %H:%M:%S GMT").to_string()
}

/// Which signature scheme a request is signed with.
#[derive(Clone, Copy)]
enum Scheme {
    V4Header,
    V4Presigned,
    V2Header,
    V2Presigned,
}

const SCHEMES: [(&str, Scheme); 4] = [
    ("sigv4 header", Scheme::V4Header),
    ("sigv4 presigned", Scheme::V4Presigned),
    ("sigv2 header", Scheme::V2Header),
    ("sigv2 presigned", Scheme::V2Presigned),
];

/// Builds a GET request signed over `signed_path` and dispatched to [`ROUTED_PATH`].
fn signed_request(scheme: Scheme, signed_path: &str) -> Request<Body> {
    let amz_date_str = now_amz_date();
    let amz_date = AmzDate::parse(&amz_date_str).unwrap();
    let scope = format!("{}/us-east-1/s3/aws4_request", amz_date.fmt_date());
    let no_query: &[(String, String)] = &[];

    match scheme {
        Scheme::V4Header => {
            let headers = [
                ("host", HOST),
                ("x-amz-content-sha256", "UNSIGNED-PAYLOAD"),
                ("x-amz-date", amz_date_str.as_str()),
            ];
            let canonical = s3s_sigv4::create_canonical_request("GET", signed_path, no_query, headers, Payload::Unsigned);
            let to_sign = s3s_sigv4::create_string_to_sign(&canonical, &amz_date, "us-east-1", "s3");
            let signature = s3s_sigv4::calculate_signature(&to_sign, SECRET_KEY, &amz_date, "us-east-1", "s3");
            let authorization = format!(
                "AWS4-HMAC-SHA256 Credential={ACCESS_KEY}/{scope}, SignedHeaders=host;x-amz-content-sha256;x-amz-date, Signature={signature}"
            );
            Request::builder()
                .method("GET")
                .uri(ROUTED_PATH)
                .header("host", HOST)
                .header("x-amz-content-sha256", "UNSIGNED-PAYLOAD")
                .header("x-amz-date", amz_date_str)
                .header("authorization", authorization)
                .body(Body::empty())
                .unwrap()
        }
        Scheme::V4Presigned => {
            let credential = format!("{ACCESS_KEY}/{scope}");
            // Decoded query pairs, sorted by name, as the verifier reconstructs them.
            let qs = vec![
                ("X-Amz-Algorithm".to_owned(), "AWS4-HMAC-SHA256".to_owned()),
                ("X-Amz-Credential".to_owned(), credential.clone()),
                ("X-Amz-Date".to_owned(), amz_date_str.clone()),
                ("X-Amz-Expires".to_owned(), "600".to_owned()),
                ("X-Amz-SignedHeaders".to_owned(), "host".to_owned()),
            ];
            let canonical = s3s_sigv4::create_presigned_canonical_request("GET", signed_path, &qs, [("host", HOST)]);
            let to_sign = s3s_sigv4::create_string_to_sign(&canonical, &amz_date, "us-east-1", "s3");
            let signature = s3s_sigv4::calculate_signature(&to_sign, SECRET_KEY, &amz_date, "us-east-1", "s3");
            let uri = format!(
                "{ROUTED_PATH}?X-Amz-Algorithm=AWS4-HMAC-SHA256&X-Amz-Credential={}&X-Amz-Date={amz_date_str}&X-Amz-Expires=600&X-Amz-SignedHeaders=host&X-Amz-Signature={signature}",
                credential.replace('/', "%2F")
            );
            Request::builder()
                .method("GET")
                .uri(uri)
                .header("host", HOST)
                .body(Body::empty())
                .unwrap()
        }
        Scheme::V2Header => {
            let date = now_http_date();
            let to_sign = s3s_sigv2::create_string_to_sign(
                s3s_sigv2::Mode::HeaderAuth,
                "GET",
                signed_path,
                None,
                &[("date", date.as_str())],
                None,
            );
            let signature = s3s_sigv2::calculate_signature(SECRET_KEY, &to_sign);
            Request::builder()
                .method("GET")
                .uri(ROUTED_PATH)
                .header("host", HOST)
                .header("date", date)
                .header("authorization", format!("AWS {ACCESS_KEY}:{signature}"))
                .body(Body::empty())
                .unwrap()
        }
        Scheme::V2Presigned => {
            let expires = (jiff::Timestamp::now().as_second() + 600).to_string();
            // Decoded query pairs, sorted by name.
            let qs = vec![
                ("AWSAccessKeyId".to_owned(), ACCESS_KEY.to_owned()),
                ("Expires".to_owned(), expires.clone()),
            ];
            let to_sign =
                s3s_sigv2::create_string_to_sign(s3s_sigv2::Mode::PresignedUrl, "GET", signed_path, Some(&qs), &[], None);
            let signature = s3s_sigv2::calculate_signature(SECRET_KEY, &to_sign);
            let encoded = signature.replace('+', "%2B").replace('/', "%2F").replace('=', "%3D");
            Request::builder()
                .method("GET")
                .uri(format!("{ROUTED_PATH}?AWSAccessKeyId={ACCESS_KEY}&Expires={expires}&Signature={encoded}"))
                .header("host", HOST)
                .body(Body::empty())
                .unwrap()
        }
    }
}

async fn send(service: &S3Service, mut request: Request<Body>, extension: Option<&str>) -> (StatusCode, String) {
    if let Some(path) = extension {
        request.extensions_mut().insert(SignedUriPath::new(path));
    }
    let response = service.call(request).await.unwrap();
    let status = response.status();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    (status, String::from_utf8_lossy(&body).into_owned())
}

#[test]
fn signed_uri_path_accessors() {
    let path = SignedUriPath::new(String::from("/prefix/bkt/k"));
    assert_eq!(path.as_str(), "/prefix/bkt/k");
    assert_eq!(path.clone(), path);
    assert_ne!(path, SignedUriPath::new("/other"));
}

#[tokio::test]
async fn signed_uri_path_verifies_and_routes_on_the_request_uri() {
    for (name, scheme) in SCHEMES {
        let (service, seen) = service();
        let (status, body) = send(&service, signed_request(scheme, SIGNED_PATH), Some(SIGNED_PATH)).await;
        assert_eq!(status, StatusCode::OK, "{name}: {body}");
        assert_eq!(
            seen.lock().unwrap().clone(),
            Some(("bkt".to_owned(), "k".to_owned())),
            "{name}: routing must use the request URI"
        );
    }
}

#[tokio::test]
async fn without_signed_uri_path_a_rewritten_request_is_rejected() {
    for (name, scheme) in SCHEMES {
        let (service, seen) = service();
        let (status, body) = send(&service, signed_request(scheme, SIGNED_PATH), None).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{name}: {body}");
        assert!(body.contains("SignatureDoesNotMatch"), "{name}: {body}");
        assert!(seen.lock().unwrap().is_none(), "{name}: handler must not run");
    }
}

#[tokio::test]
async fn signed_uri_path_that_differs_from_the_signed_path_is_rejected() {
    for (name, scheme) in SCHEMES {
        for wrong in ["/other/bkt/k", "/prefix/bkt/k/", ROUTED_PATH] {
            let (service, seen) = service();
            let (status, body) = send(&service, signed_request(scheme, SIGNED_PATH), Some(wrong)).await;
            assert_eq!(status, StatusCode::FORBIDDEN, "{name} with {wrong}: {body}");
            assert!(body.contains("SignatureDoesNotMatch"), "{name} with {wrong}: {body}");
            assert!(seen.lock().unwrap().is_none(), "{name} with {wrong}: handler must not run");
        }
    }
}

#[tokio::test]
async fn requests_without_the_extension_keep_verifying_over_the_uri_path() {
    for (name, scheme) in SCHEMES {
        let (service, seen) = service();
        let (status, body) = send(&service, signed_request(scheme, ROUTED_PATH), None).await;
        assert_eq!(status, StatusCode::OK, "{name}: {body}");
        assert_eq!(seen.lock().unwrap().clone(), Some(("bkt".to_owned(), "k".to_owned())), "{name}");
    }
}
