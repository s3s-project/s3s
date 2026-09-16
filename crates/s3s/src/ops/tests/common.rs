// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

//! Harness shared by the in-crate `ops` tests and micro-benchmarks.
//!
//! Only compiled under `cfg(test)`. Items are `pub(crate)` because they are used from
//! outside the `tests` subtree: `ops/signature.rs` tests and `ops/benches/`.

use super::*;

use crate::auth::{SecretKey, SimpleAuth};
use crate::config::StaticConfigProvider;
use crate::host::SingleDomain;
use crate::protocol::S3Response;
use crate::route::S3Route;
use hyper::Version;
use hyper::header::{HeaderName, HeaderValue};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

pub(crate) struct NeverGetSecretKeyAuth;

#[async_trait::async_trait]
impl crate::auth::S3Auth for NeverGetSecretKeyAuth {
    async fn get_secret_key(&self, _access_key: &str) -> crate::error::S3Result<crate::auth::SecretKey> {
        panic!("secret key lookup must not occur for a wrong-region request")
    }
}

pub(crate) const ACCESS_KEY: &str = "test-access";

pub(crate) const SECRET_KEY: &str = "test-secret";

pub(crate) const AMZ_DATE: &str = "20260828T000000Z";

pub(crate) const REGION: &str = "us-east-1";

pub(crate) const SERVICE: &str = "s3";

pub(crate) const EMPTY_SHA256: &str = s3s_sigv4::EMPTY_STRING_SHA256_HASH;

pub(crate) const UNSIGNED_PAYLOAD: &str = "UNSIGNED-PAYLOAD";

#[derive(Default)]
pub(crate) struct TestS3 {
    pub(crate) get_object: AtomicUsize,
    pub(crate) head_object: AtomicUsize,
    pub(crate) delete_object: AtomicUsize,
    pub(crate) copy_object: AtomicUsize,
    pub(crate) upload_part_copy: AtomicUsize,
    pub(crate) put_object: AtomicUsize,
    pub(crate) upload_part: AtomicUsize,
    /// Records the `acl` field parsed from the last `PutObject` input.
    pub(crate) last_put_acl: Mutex<Option<String>>,
}

#[async_trait::async_trait]
impl crate::s3_trait::S3 for TestS3 {
    async fn get_object(
        &self,
        _req: crate::S3Request<crate::dto::GetObjectInput>,
    ) -> crate::error::S3Result<S3Response<crate::dto::GetObjectOutput>> {
        self.get_object.fetch_add(1, Ordering::SeqCst);
        Ok(S3Response::new(crate::dto::GetObjectOutput::default()))
    }

    async fn head_object(
        &self,
        _req: crate::S3Request<crate::dto::HeadObjectInput>,
    ) -> crate::error::S3Result<S3Response<crate::dto::HeadObjectOutput>> {
        self.head_object.fetch_add(1, Ordering::SeqCst);
        Ok(S3Response::new(crate::dto::HeadObjectOutput::default()))
    }

    async fn delete_object(
        &self,
        _req: crate::S3Request<crate::dto::DeleteObjectInput>,
    ) -> crate::error::S3Result<S3Response<crate::dto::DeleteObjectOutput>> {
        self.delete_object.fetch_add(1, Ordering::SeqCst);
        Ok(S3Response::new(crate::dto::DeleteObjectOutput::default()))
    }

    async fn copy_object(
        &self,
        _req: crate::S3Request<crate::dto::CopyObjectInput>,
    ) -> crate::error::S3Result<S3Response<crate::dto::CopyObjectOutput>> {
        self.copy_object.fetch_add(1, Ordering::SeqCst);
        Ok(S3Response::new(crate::dto::CopyObjectOutput::default()))
    }

    async fn upload_part_copy(
        &self,
        _req: crate::S3Request<crate::dto::UploadPartCopyInput>,
    ) -> crate::error::S3Result<S3Response<crate::dto::UploadPartCopyOutput>> {
        self.upload_part_copy.fetch_add(1, Ordering::SeqCst);
        Ok(S3Response::new(crate::dto::UploadPartCopyOutput::default()))
    }

    async fn put_object(
        &self,
        req: crate::S3Request<crate::dto::PutObjectInput>,
    ) -> crate::error::S3Result<S3Response<crate::dto::PutObjectOutput>> {
        self.put_object.fetch_add(1, Ordering::SeqCst);
        let acl = req.input.acl.map(|acl| {
            let value: std::borrow::Cow<'static, str> = acl.into();
            value.into_owned()
        });
        *self.last_put_acl.lock().unwrap() = acl;
        Ok(S3Response::new(crate::dto::PutObjectOutput::default()))
    }

    async fn upload_part(
        &self,
        _req: crate::S3Request<crate::dto::UploadPartInput>,
    ) -> crate::error::S3Result<S3Response<crate::dto::UploadPartOutput>> {
        self.upload_part.fetch_add(1, Ordering::SeqCst);
        Ok(S3Response::new(crate::dto::UploadPartOutput::default()))
    }
}

pub(crate) fn empty_unknown_length_body() -> Body {
    let stream = futures::stream::empty::<Result<http_body::Frame<Bytes>, std::convert::Infallible>>();
    Body::http_body(http_body_util::StreamBody::new(stream))
}

pub(crate) fn test_config() -> Arc<dyn S3ConfigProvider> {
    Arc::new(StaticConfigProvider::new(Arc::new(S3Config {
        presigned_url_max_skew_time_secs: u32::MAX,
        expected_region: Some(REGION.parse().expect("valid test region")),
        ..Default::default()
    })))
}

pub(crate) fn test_context<'a>(
    s3: &'a Arc<dyn crate::s3_trait::S3>,
    config: &'a Arc<dyn S3ConfigProvider>,
    auth: &'a SimpleAuth,
) -> CallContext<'a> {
    CallContext {
        s3,
        config,
        host: None,
        auth: Some(auth),
        access: None,
        route: None,
        validation: None,
    }
}

pub(crate) fn sign_request(
    method: &Method,
    uri: &Uri,
    payload_sha256: &str,
    extra_headers: &[(&'static str, &'static str)],
) -> String {
    let amz_date = s3s_sigv4::AmzDate::parse(AMZ_DATE).unwrap();
    let amz_date_str = amz_date.fmt_iso8601();
    let host = uri.authority().expect("test URI has authority").as_str();
    let qs = uri.query().map(OrderedQs::parse).transpose().unwrap();
    let empty_query = &[] as &[(String, String)];
    let query = qs.as_ref().map_or(empty_query, AsRef::as_ref);
    let mut signed_headers = vec![
        ("host", host),
        ("x-amz-content-sha256", payload_sha256),
        ("x-amz-date", amz_date_str.as_str()),
    ];
    signed_headers.extend(extra_headers.iter().copied());
    signed_headers.sort_unstable_by(|lhs, rhs| lhs.0.cmp(rhs.0));
    let signed_header_names = signed_headers.iter().map(|(name, _)| *name).collect::<Vec<_>>().join(";");
    let payload = if payload_sha256 == UNSIGNED_PAYLOAD {
        s3s_sigv4::Payload::Unsigned
    } else {
        s3s_sigv4::Payload::SingleChunk(payload_sha256)
    };

    let canonical_request = s3s_sigv4::create_canonical_request(method.as_str(), uri.path(), query, signed_headers, payload);
    let string_to_sign = s3s_sigv4::create_string_to_sign(&canonical_request, &amz_date, REGION, SERVICE);
    let signature = s3s_sigv4::calculate_signature(&string_to_sign, SECRET_KEY, &amz_date, REGION, SERVICE);

    format!(
        "AWS4-HMAC-SHA256 Credential={ACCESS_KEY}/{}/{REGION}/{SERVICE}/aws4_request, \
         SignedHeaders={signed_header_names}, Signature={}",
        amz_date.fmt_date(),
        signature.as_str()
    )
}

pub(crate) fn signed_request(
    method: Method,
    version: Version,
    uri: &str,
    payload_sha256: &'static str,
    extra_headers: &[(&'static str, &'static str)],
) -> Request {
    let uri = uri.parse::<Uri>().unwrap();
    let authorization = sign_request(&method, &uri, payload_sha256, extra_headers);
    let mut builder = hyper::Request::builder()
        .method(method)
        .version(version)
        .uri(uri.clone())
        .header(crate::header::X_AMZ_CONTENT_SHA256, payload_sha256)
        .header(crate::header::X_AMZ_DATE, AMZ_DATE)
        .header(crate::header::AUTHORIZATION, authorization);

    if version == Version::HTTP_11 {
        builder = builder.header(crate::header::HOST, uri.authority().unwrap().as_str());
    }

    for &(name, value) in extra_headers {
        builder = builder.header(name, value);
    }

    Request::from(builder.body(empty_unknown_length_body()).unwrap())
}

pub(crate) struct ContentLengthRecordingS3 {
    pub(crate) received: Mutex<Option<(Option<i64>, Option<u64>)>>, // (input.content_length, headers content-length)
}

#[async_trait::async_trait]
impl crate::s3_trait::S3 for ContentLengthRecordingS3 {
    async fn put_object(
        &self,
        req: crate::S3Request<crate::dto::PutObjectInput>,
    ) -> crate::error::S3Result<S3Response<crate::dto::PutObjectOutput>> {
        let header_cl = req.headers.get(hyper::header::CONTENT_LENGTH).map(|v| {
            v.to_str()
                .expect("test header is ASCII")
                .parse::<u64>()
                .expect("test header parses")
        });
        *self.received.lock().unwrap() = Some((req.input.content_length, header_cl));
        Ok(S3Response::new(crate::dto::PutObjectOutput::default()))
    }
}

pub(crate) mod post_policy_test_helpers {
    use std::fmt::Write;

    use crate::S3Request;
    use crate::auth::{S3Auth, SecretKey, SimpleAuth};
    use crate::config::{S3Config, S3ConfigProvider, StaticConfigProvider};
    use crate::http::{Body, Request};
    use crate::ops::CallContext;
    use bytes::Bytes;
    use hyper::Method;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    pub struct TestS3WithPostTracking {
        pub post_calls: AtomicUsize,
    }

    #[async_trait::async_trait]
    impl crate::s3_trait::S3 for TestS3WithPostTracking {
        async fn post_object(
            &self,
            _req: S3Request<crate::dto::PostObjectInput>,
        ) -> crate::error::S3Result<crate::protocol::S3Response<crate::dto::PostObjectOutput>> {
            self.post_calls.fetch_add(1, Ordering::SeqCst);
            Ok(crate::protocol::S3Response::new(crate::dto::PostObjectOutput::default()))
        }
    }

    pub struct TestS3NoOp;

    #[async_trait::async_trait]
    impl crate::s3_trait::S3 for TestS3NoOp {}

    /// Create a test config with custom `post_object_max_file_size`
    pub fn create_test_config(post_object_max_file_size: u64) -> Arc<dyn S3ConfigProvider> {
        let config = S3Config {
            presigned_url_max_skew_time_secs: u32::MAX,
            post_object_max_file_size,
            expected_region: Some("us-east-1".parse().expect("valid test region")),
            ..Default::default()
        };
        Arc::new(StaticConfigProvider::new(Arc::new(config)))
    }

    /// Create auth and `CallContext` for testing
    pub fn create_test_context<'a>(
        s3: &'a Arc<dyn crate::s3_trait::S3>,
        config: &'a Arc<dyn S3ConfigProvider>,
        auth: &'a dyn S3Auth,
    ) -> CallContext<'a> {
        CallContext {
            s3,
            config,
            host: None,
            auth: Some(auth),
            access: None,
            route: None,
            validation: None,
        }
    }

    /// Create a `SimpleAuth` for testing
    pub fn create_test_auth() -> SimpleAuth {
        let access_key = "AKIAIOSFODNN7EXAMPLE";
        let secret_key: SecretKey = "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY".into();
        SimpleAuth::from_single(access_key, secret_key)
    }

    /// Standard conditions that cover the form fields added by `build_post_object_request`.
    ///
    /// Per the S3 spec, each non-exempt form field must have a matching condition in the policy.
    /// The fields added by the helper are: bucket, key, x-amz-algorithm, x-amz-credential,
    /// x-amz-date.  (x-amz-signature, policy, and file are exempt.)
    pub const BASE_CONDITIONS: &str = r#"{"bucket":"test-bucket"},["eq","$key","test-key"],["starts-with","$x-amz-algorithm",""],["starts-with","$x-amz-credential",""],["starts-with","$x-amz-date",""]"#;

    pub fn build_multipart_fields(list: &[(&str, &str)], boundary: &str) -> String {
        let mut d = String::new();
        for (name, value) in list {
            write!(
                &mut d,
                "--{boundary}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n"
            )
            .unwrap();
        }
        d
    }

    pub fn build_multipart_file_field(
        field_name: &str,
        filename: &str,
        content_type: &str,
        file_content: &str,
        boundary: &str,
    ) -> String {
        format!(
            concat!(
                "--{boundary}\r\n",
                "Content-Disposition: form-data; name=\"{field_name}\"; filename=\"{filename}\"\r\n",
                "Content-Type: {content_type}\r\n\r\n",
                "{file_content}\r\n",
                "--{boundary}--\r\n",
            ),
            boundary = boundary,
            field_name = field_name,
            filename = filename,
            content_type = content_type,
            file_content = file_content,
        )
    }

    /// Augment a test POST policy JSON with the required `SigV4` eq conditions
    /// (`x-amz-date`, `x-amz-credential`, `x-amz-algorithm`) so the request
    /// passes the verifier's policy-field-matching checks.
    pub fn augment_post_policy_for_test(policy_json: &str, amz_date: &str, credential: &str, algorithm: &str) -> String {
        let mut policy: serde_json::Value = serde_json::from_str(policy_json).expect("invalid test policy JSON");
        let conditions = policy["conditions"]
            .as_array_mut()
            .expect("policy must have a conditions array");
        conditions.push(serde_json::json!({"x-amz-date": amz_date}));
        conditions.push(serde_json::json!({"x-amz-credential": credential}));
        conditions.push(serde_json::json!({"x-amz-algorithm": algorithm}));
        policy.to_string()
    }

    /// Build a POST object request with a policy.
    ///
    /// The provided `policy_json` is augmented with the required `SigV4` POST
    /// policy eq conditions (`x-amz-date`, `x-amz-credential`, `x-amz-algorithm`)
    /// so the request passes the verifier's policy-field-matching checks.
    pub fn build_post_object_request(
        policy_json: &str,
        file_content: &str,
        secret_key: &SecretKey,
        with_content_type: bool,
    ) -> Request {
        let boundary = "------------------------test12345678";
        let bucket = "test-bucket";
        let key = "test-key";
        let amz_date = s3s_sigv4::AmzDate::parse("20250101T000000Z").unwrap();
        let amz_date_str = amz_date.fmt_iso8601();
        let region = "us-east-1";
        let service = "s3";
        let content_type = "text/plain";
        let algorithm = "AWS4-HMAC-SHA256";
        let credential = "AKIAIOSFODNN7EXAMPLE/20250101/us-east-1/s3/aws4_request";

        let policy_json = augment_post_policy_for_test(policy_json, amz_date_str.as_str(), credential, algorithm);
        let policy_b64 = base64_simd::STANDARD.encode_to_string(&policy_json);
        let signature = s3s_sigv4::calculate_signature(&policy_b64, secret_key.expose(), &amz_date, region, service);

        let fields = {
            let mut f = vec![
                ("x-amz-signature", signature.as_str()),
                ("bucket", bucket),
                ("policy", policy_b64.as_str()),
                ("x-amz-algorithm", algorithm),
                ("x-amz-credential", credential),
                ("x-amz-date", amz_date_str.as_str()),
                ("key", key),
            ];
            if with_content_type {
                f.push(("Content-Type", content_type));
            }
            f
        };

        let body = build_multipart_fields(&fields, boundary)
            + build_multipart_file_field("file", "test.txt", content_type, file_content, boundary).as_str();

        Request::from(
            hyper::Request::builder()
                .method(Method::POST)
                .uri(format!("http://localhost/{bucket}"))
                .header(crate::header::HOST, "localhost")
                .header(
                    crate::header::CONTENT_TYPE,
                    hyper::header::HeaderValue::from_str(&format!("multipart/form-data; boundary={boundary}")).unwrap(),
                )
                .header(hyper::header::CONTENT_LENGTH, body.len())
                .body(Body::from(Bytes::from(body)))
                .unwrap(),
        )
    }

    /// Build a POST object request whose body is split into many small chunks.
    ///
    /// This ensures that `aggregate_file_stream_limited` returns a `Vec<Bytes>`
    /// with multiple entries, so tests can distinguish between
    /// `vec_bytes.len()` (chunk count) and the total byte count.
    ///
    /// The provided `policy_json` is augmented with the required `SigV4` POST
    /// policy eq conditions so the request passes the verifier's checks.
    pub fn build_post_object_request_chunked(
        policy_json: &str,
        file_content: &str,
        secret_key: &SecretKey,
        chunk_size: usize,
    ) -> Request {
        let boundary = "------------------------test12345678";
        let bucket = "test-bucket";
        let key = "test-key";
        let amz_date = s3s_sigv4::AmzDate::parse("20250101T000000Z").unwrap();
        let amz_date_str = amz_date.fmt_iso8601();
        let region = "us-east-1";
        let service = "s3";
        let content_type = "text/plain";
        let algorithm = "AWS4-HMAC-SHA256";
        let credential = "AKIAIOSFODNN7EXAMPLE/20250101/us-east-1/s3/aws4_request";

        let policy_json = augment_post_policy_for_test(policy_json, amz_date_str.as_str(), credential, algorithm);
        let policy_b64 = base64_simd::STANDARD.encode_to_string(&policy_json);
        let signature = s3s_sigv4::calculate_signature(&policy_b64, secret_key.expose(), &amz_date, region, service);

        let body = build_multipart_fields(
            &[
                ("x-amz-signature", signature.as_str()),
                ("bucket", bucket),
                ("policy", &policy_b64),
                ("x-amz-algorithm", algorithm),
                ("x-amz-credential", credential),
                ("x-amz-date", amz_date_str.as_str()),
                ("key", key),
            ],
            boundary,
        ) + &build_multipart_file_field("file", "test.txt", content_type, file_content, boundary);

        // Split the body into small chunks to simulate a multi-chunk stream
        let body_bytes: Vec<u8> = body.into_bytes();
        let chunks: Vec<Result<http_body::Frame<Bytes>, std::convert::Infallible>> = body_bytes
            .chunks(chunk_size)
            .map(|c| Ok(http_body::Frame::data(Bytes::copy_from_slice(c))))
            .collect();

        let stream_body = http_body_util::StreamBody::new(futures::stream::iter(chunks));

        Request::from(
            hyper::Request::builder()
                .method(Method::POST)
                .uri(format!("http://localhost/{bucket}"))
                .header(crate::header::HOST, "localhost")
                .header(
                    crate::header::CONTENT_TYPE,
                    hyper::header::HeaderValue::from_str(&format!("multipart/form-data; boundary={boundary}")).unwrap(),
                )
                .body(Body::http_body(stream_body))
                .unwrap(),
        )
    }
}

/// `S3` implementation that does nothing: the tests exercise routing, authentication and
/// serialization rather than operation bodies.
pub(crate) struct NoopS3;

#[async_trait::async_trait]
impl crate::s3_trait::S3 for NoopS3 {}

/// Owned halves of a [`CallContext`] that routing tests build once and borrow.
pub(crate) struct CtxParts {
    pub(crate) s3: Arc<dyn crate::s3_trait::S3>,
    pub(crate) config: Arc<dyn S3ConfigProvider>,
    pub(crate) auth: Option<SimpleAuth>,
    pub(crate) host: Option<SingleDomain>,
}

/// Context without auth, host or route wiring.
pub(crate) fn ctx() -> CtxParts {
    CtxParts {
        s3: Arc::new(NoopS3),
        config: Arc::new(StaticConfigProvider::default()),
        auth: None,
        host: None,
    }
}

/// Context with the canonical test credentials and a `SingleDomain` host.
pub(crate) fn ctx_with_auth() -> CtxParts {
    CtxParts {
        auth: Some(SimpleAuth::from_single(
            "AKIAIOSFODNN7EXAMPLE",
            SecretKey::from("wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY"),
        )),
        host: Some(SingleDomain::new("example.com").expect("valid domain")),
        ..ctx()
    }
}

/// Borrowed context; `auth` and `host` select whether the owned halves are wired in.
pub(crate) fn ccx<'a>(parts: &'a CtxParts, auth: bool, host: bool, route: Option<&'a dyn S3Route>) -> CallContext<'a> {
    CallContext {
        s3: &parts.s3,
        config: &parts.config,
        host: if host {
            parts.host.as_ref().map(|h| h as &dyn crate::host::S3Host)
        } else {
            None
        },
        auth: if auth {
            parts.auth.as_ref().map(|a| a as &dyn crate::auth::S3Auth)
        } else {
            None
        },
        access: None,
        route,
        validation: None,
    }
}

pub(crate) fn headers_from_slice(slice: &[(&str, &str)]) -> HeaderMap {
    let mut headers = HeaderMap::new();
    for &(name, value) in slice {
        headers.append(
            HeaderName::from_bytes(name.as_bytes()).expect("valid test header name"),
            HeaderValue::from_bytes(value.as_bytes()).expect("valid test header value"),
        );
    }
    headers
}

pub(crate) fn fmt_current_amz_date(dt: time::OffsetDateTime) -> String {
    format!(
        "{:04}{:02}{:02}T{:02}{:02}{:02}Z",
        dt.year(),
        u8::from(dt.month()),
        dt.day(),
        dt.hour(),
        dt.minute(),
        dt.second()
    )
}

pub(crate) fn sig_v2_test_config(enable_sig_v2: bool) -> Arc<dyn S3ConfigProvider> {
    use crate::config::{S3Config, StaticConfigProvider};

    let config = S3Config {
        enable_sig_v2,
        ..Default::default()
    };
    Arc::new(StaticConfigProvider::new(Arc::new(config)))
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn sig_v2_test_context<'a>(
    config: &'a Arc<dyn S3ConfigProvider>,
    auth: Option<&'a dyn crate::auth::S3Auth>,
    method: &'a Method,
    uri: &'a Uri,
    body: &'a mut Body,
    qs: Option<&'a OrderedQs>,
    hs: &'a HeaderMap,
    mime: Option<Mime>,
) -> SignatureContext<'a> {
    SignatureContext {
        auth,
        config,
        req_version: ::http::Version::HTTP_11,
        req_method: method,
        req_uri: uri,
        req_body: body,
        qs,
        hs,
        decoded_uri_path: "/test.txt",
        raw_uri_path: "/test.txt",
        vh_bucket: None,
        content_length: None,
        mime,
        decoded_content_length: None,
        transformed_body: None,
        multipart: None,
        trailing_headers: None,
    }
}
