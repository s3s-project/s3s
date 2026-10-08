// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

//! End-to-end `POST Object` (browser form upload) through the request pipeline.
//!
//! Every case builds a real `multipart/form-data` body with a signed policy, sends it through an
//! `S3Service` built from `S3ServiceBuilder`, and asserts what a client observes: the status line,
//! the response headers, the response body, and the object the `S3` implementation was asked to
//! store. The expectations are the invariant baseline for `POST Object`, whose implementation is
//! being moved from a bridge over `PutObject` to a first-class operation.
//!
//! Part of that baseline is a recorded deviation. Amazon S3 answers the 200, 201 and 204 success
//! forms with an `ETag` header (and a `Location`); this crate currently answers without either. The
//! deviation is deliberate and pinned here, so that closing it turns the assertions below red on
//! purpose instead of changing the behaviour silently.

use std::fmt::Write as _;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use bytes::Bytes;
use futures::StreamExt;
use http::{HeaderMap, Method, Request, StatusCode};
use http_body_util::BodyExt;
use md5::{Digest as _, Md5};
use s3s::auth::{SecretKey, SimpleAuth};
use s3s::config::{S3Config, StaticConfigProvider};
use s3s::dto::{ETag, PutObjectInput, PutObjectOutput, StreamingBlob};
use s3s::service::{S3Service, S3ServiceBuilder};
use s3s::{Body, FileStreamError, S3, S3Error, S3ErrorCode, S3Request, S3Response, S3Result};
use serde_json::json;

const ACCESS_KEY: &str = "AKIAIOSFODNN7EXAMPLE";
const SECRET_KEY: &str = "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY";
const REGION: &str = "us-east-1";
const SERVICE: &str = "s3";
const ALGORITHM: &str = "AWS4-HMAC-SHA256";
const AMZ_DATE: &str = "20250101T000000Z";
const CREDENTIAL: &str = "AKIAIOSFODNN7EXAMPLE/20250101/us-east-1/s3/aws4_request";
const BOUNDARY: &str = "------------------------wireboundary1234";
const BUCKET: &str = "test-bucket";
const KEY: &str = "test-key";
const CONTENT_TYPE: &str = "text/plain";
const FILE_NAME: &str = "test.txt";
const POLICY_EXPIRATION: &str = "2030-01-01T00:00:00.000Z";

/// The file part every case uploads unless it overrides it.
const FILE_CONTENT: &str = "hello world";

/// The base64 MD5 of [`FILE_CONTENT`], as a form's `Content-MD5` field carries it.
const FILE_CONTENT_MD5: &str = "XrY7u+Ae7tCTyyK7j1rNww==";

/// The hex MD5 of [`FILE_CONTENT`]: the `ETag` the recording implementation reports, so the
/// tests connect the value a client sees to the bytes that were stored.
const STORED_ETAG: &str = "5eb63bbbe01eeed093cb22bb8f5acdc3";

/// The 201 response body for [`FILE_CONTENT`] under [`BUCKET`] and [`KEY`].
const POST_RESPONSE_XML: &str = "<?xml version=\"1.0\" encoding=\"UTF-8\"?><PostResponse><Location>/test-bucket/test-key</Location><Bucket>test-bucket</Bucket><Key>test-key</Key><ETag>5eb63bbbe01eeed093cb22bb8f5acdc3</ETag></PostResponse>";

/// What the implementation was asked to store.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Stored {
    bucket: String,
    key: String,
    content_md5: Option<String>,
    content_type: Option<String>,
    body: Vec<u8>,
}

/// The record a successful upload of [`FILE_CONTENT`] leaves behind.
fn stored_file(content_md5: Option<&str>) -> Stored {
    Stored {
        bucket: BUCKET.to_owned(),
        key: KEY.to_owned(),
        content_md5: content_md5.map(str::to_owned),
        content_type: Some(CONTENT_TYPE.to_owned()),
        body: FILE_CONTENT.as_bytes().to_vec(),
    }
}

#[derive(Clone)]
struct TestS3 {
    stored: Arc<Mutex<Vec<Stored>>>,
    etag: ETag,
}

impl TestS3 {
    fn new() -> Self {
        Self {
            stored: Arc::new(Mutex::new(Vec::new())),
            etag: ETag::Strong(STORED_ETAG.to_owned()),
        }
    }

    fn stored(&self) -> Vec<Stored> {
        self.stored.lock().expect("test mutex").clone()
    }
}

#[async_trait]
impl S3 for TestS3 {
    async fn put_object(&self, mut req: S3Request<PutObjectInput>) -> S3Result<S3Response<PutObjectOutput>> {
        let body = match req.input.body.take() {
            Some(blob) => collect_blob(blob).await?,
            None => Vec::new(),
        };
        self.stored.lock().expect("test mutex").push(Stored {
            bucket: req.input.bucket.clone(),
            key: req.input.key.clone(),
            content_md5: req.input.content_md5.clone(),
            content_type: req.input.content_type.clone(),
            body,
        });
        Ok(S3Response::new(PutObjectOutput {
            e_tag: Some(self.etag.clone()),
            ..Default::default()
        }))
    }
}

/// Reads a file part to the end, mapping a failure to the error code a conforming implementation
/// reports for it.
async fn collect_blob(blob: StreamingBlob) -> S3Result<Vec<u8>> {
    let chunks: Vec<Result<Bytes, Box<dyn std::error::Error + Send + Sync>>> = StreamExt::collect(blob).await;
    let mut body = Vec::new();
    for chunk in chunks {
        match chunk {
            Ok(bytes) => body.extend_from_slice(&bytes),
            Err(err) => return Err(file_stream_error(err.as_ref())),
        }
    }
    Ok(body)
}

fn file_stream_error(err: &(dyn std::error::Error + Send + Sync + 'static)) -> S3Error {
    match err.downcast_ref::<FileStreamError>() {
        Some(err) => S3Error::from(err.to_s3_error_code()),
        None => S3Error::from(S3ErrorCode::InternalError),
    }
}

fn service(s3: TestS3) -> S3Service {
    let mut builder = S3ServiceBuilder::new(s3);
    let mut config = S3Config::default();
    config.presigned_url_max_skew_time_secs = u32::MAX;
    config.expected_region = Some(REGION.parse().expect("valid test region"));
    config.post_object_max_file_size = 10 * 1024 * 1024;
    builder.set_config(Arc::new(StaticConfigProvider::new(Arc::new(config))));
    builder.set_auth(SimpleAuth::from_single(ACCESS_KEY, SECRET_KEY));
    builder.build()
}

/// A signed form upload: the fields every form carries, plus what a case varies.
#[derive(Clone)]
struct PostForm {
    /// The form's `key` field; `None` omits the field.
    key: Option<String>,
    /// The policy's `$key` equality condition; `None` omits the condition.
    key_condition: Option<String>,
    /// The form's `Content-Type` field; `None` omits the field and its condition.
    content_type: Option<String>,
    /// Additional policy conditions.
    extra_conditions: Vec<serde_json::Value>,
    /// Additional form fields; each one needs a condition in `extra_conditions`.
    extra_fields: Vec<(String, String)>,
    /// The content of the `file` part.
    file: String,
}

impl PostForm {
    fn new(file: &str) -> Self {
        Self {
            key: Some(KEY.to_owned()),
            key_condition: Some(KEY.to_owned()),
            content_type: Some(CONTENT_TYPE.to_owned()),
            extra_conditions: Vec::new(),
            extra_fields: Vec::new(),
            file: file.to_owned(),
        }
    }

    fn field(mut self, name: &str, value: &str) -> Self {
        self.extra_fields.push((name.to_owned(), value.to_owned()));
        self
    }

    fn condition(mut self, condition: serde_json::Value) -> Self {
        self.extra_conditions.push(condition);
        self
    }

    fn key_condition(mut self, value: Option<&str>) -> Self {
        self.key_condition = value.map(str::to_owned);
        self
    }

    fn key(mut self, value: Option<&str>) -> Self {
        self.key = value.map(str::to_owned);
        self
    }

    fn no_content_type(mut self) -> Self {
        self.content_type = None;
        self
    }

    fn to_request(&self) -> Request<Body> {
        let mut conditions: Vec<serde_json::Value> = vec![json!({ "bucket": BUCKET })];
        if let Some(key) = &self.key_condition {
            conditions.push(json!(["eq", "$key", key]));
        }
        conditions.push(json!(["starts-with", "$x-amz-algorithm", ""]));
        conditions.push(json!(["starts-with", "$x-amz-credential", ""]));
        conditions.push(json!(["starts-with", "$x-amz-date", ""]));
        conditions.push(json!({ "x-amz-date": AMZ_DATE }));
        conditions.push(json!({ "x-amz-credential": CREDENTIAL }));
        conditions.push(json!({ "x-amz-algorithm": ALGORITHM }));
        if let Some(content_type) = &self.content_type {
            conditions.push(json!(["eq", "$Content-Type", content_type]));
        }
        conditions.extend(self.extra_conditions.iter().cloned());

        let policy_json = json!({
            "expiration": POLICY_EXPIRATION,
            "conditions": conditions,
        })
        .to_string();
        let policy_b64 = base64_simd::STANDARD.encode_to_string(&policy_json);
        let secret_key = SecretKey::from(SECRET_KEY);
        let amz_date = s3s_sigv4::AmzDate::parse(AMZ_DATE).expect("valid test date");
        let signature = s3s_sigv4::calculate_signature(&policy_b64, secret_key.expose(), &amz_date, REGION, SERVICE);

        let mut fields: Vec<(String, String)> = vec![
            ("x-amz-signature".to_owned(), signature.as_str().to_owned()),
            ("bucket".to_owned(), BUCKET.to_owned()),
            ("policy".to_owned(), policy_b64),
            ("x-amz-algorithm".to_owned(), ALGORITHM.to_owned()),
            ("x-amz-credential".to_owned(), CREDENTIAL.to_owned()),
            ("x-amz-date".to_owned(), AMZ_DATE.to_owned()),
        ];
        if let Some(key) = &self.key {
            fields.push(("key".to_owned(), key.clone()));
        }
        if let Some(content_type) = &self.content_type {
            fields.push(("Content-Type".to_owned(), content_type.clone()));
        }
        fields.extend(self.extra_fields.iter().cloned());

        let body = multipart_body(&fields, &self.file);
        Request::builder()
            .method(Method::POST)
            .uri(format!("http://localhost/{BUCKET}"))
            .header("host", "localhost")
            .header("content-type", format!("multipart/form-data; boundary={BOUNDARY}"))
            .header("content-length", body.len())
            .body(Body::from(body))
            .expect("valid request")
    }
}

fn multipart_body(fields: &[(String, String)], file: &str) -> String {
    let mut body = String::new();
    for (name, value) in fields {
        write!(body, "--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n")
            .expect("writing to a String cannot fail");
    }
    write!(
        body,
        "--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"{FILE_NAME}\"\r\nContent-Type: text/plain\r\n\r\n{file}\r\n--{BOUNDARY}--\r\n"
    )
    .expect("writing to a String cannot fail");
    body
}

struct Wire {
    status: StatusCode,
    headers: HeaderMap,
    body: String,
}

async fn send(service: &S3Service, req: Request<Body>) -> Wire {
    let resp = service.call(req).await.expect("service call returns a response");
    let status = resp.status();
    let headers = resp.headers().clone();
    let bytes = BodyExt::collect(resp.into_body()).await.expect("body collects").to_bytes();
    Wire {
        status,
        headers,
        body: String::from_utf8_lossy(&bytes).into_owned(),
    }
}

fn header<'a>(wire: &'a Wire, name: &str) -> Option<&'a str> {
    wire.headers.get(name).and_then(|value| value.to_str().ok())
}

fn error_code(body: &str) -> Option<&str> {
    let start = body.find("<Code>")? + "<Code>".len();
    let rest = body.get(start..)?;
    let end = rest.find("</Code>")?;
    rest.get(..end)
}

/// Asserts the success forms do not name the stored object in a response header.
///
/// This is the recorded deviation described in the module docs: both headers are absent on this
/// baseline, and closing the deviation has to fail here.
fn assert_the_object_is_not_named_in_a_header(wire: &Wire) {
    assert_eq!(header(wire, "etag"), None, "this baseline sends no ETag header: {}", wire.body);
    assert_eq!(header(wire, "location"), None, "this baseline sends no Location header: {}", wire.body);
}

fn assert_error(wire: &Wire, status: StatusCode, code: &str) {
    assert_eq!(wire.status, status, "{}", wire.body);
    assert_eq!(error_code(&wire.body), Some(code), "{}", wire.body);
    assert_eq!(header(wire, "content-type"), Some("application/xml"), "{}", wire.body);
}

#[tokio::test]
async fn default_success_is_204_without_a_body() {
    let s3 = TestS3::new();
    let service = service(s3.clone());
    let wire = send(&service, PostForm::new(FILE_CONTENT).to_request()).await;

    assert_eq!(wire.status, StatusCode::NO_CONTENT, "{}", wire.body);
    assert_eq!(wire.body, "");
    assert_the_object_is_not_named_in_a_header(&wire);
    assert_eq!(s3.stored(), vec![stored_file(None)]);
}

#[tokio::test]
async fn success_action_status_200_is_200_without_a_body() {
    let s3 = TestS3::new();
    let service = service(s3.clone());
    let form = PostForm::new(FILE_CONTENT)
        .field("success_action_status", "200")
        .condition(json!(["eq", "$success_action_status", "200"]));
    let wire = send(&service, form.to_request()).await;

    assert_eq!(wire.status, StatusCode::OK, "{}", wire.body);
    assert_eq!(wire.body, "");
    assert_the_object_is_not_named_in_a_header(&wire);
    assert_eq!(s3.stored(), vec![stored_file(None)]);
}

#[tokio::test]
async fn success_action_status_201_writes_the_post_response_xml() {
    let s3 = TestS3::new();
    let service = service(s3.clone());
    let form = PostForm::new(FILE_CONTENT)
        .field("success_action_status", "201")
        .condition(json!(["eq", "$success_action_status", "201"]));
    let wire = send(&service, form.to_request()).await;

    assert_eq!(wire.status, StatusCode::CREATED, "{}", wire.body);
    assert_eq!(header(&wire, "content-type"), Some("application/xml"));
    assert_eq!(wire.body, POST_RESPONSE_XML);
    assert_the_object_is_not_named_in_a_header(&wire);
    assert_eq!(s3.stored(), vec![stored_file(None)]);
}

#[tokio::test]
async fn an_unrecognized_success_action_status_is_204() {
    let s3 = TestS3::new();
    let service = service(s3.clone());
    let form = PostForm::new(FILE_CONTENT)
        .field("success_action_status", "202")
        .condition(json!(["eq", "$success_action_status", "202"]));
    let wire = send(&service, form.to_request()).await;

    assert_eq!(wire.status, StatusCode::NO_CONTENT, "{}", wire.body);
    assert_eq!(wire.body, "");
    assert_the_object_is_not_named_in_a_header(&wire);
    assert_eq!(s3.stored(), vec![stored_file(None)]);
}

#[tokio::test]
async fn success_action_redirect_is_303_with_the_appended_query() {
    let s3 = TestS3::new();
    let service = service(s3.clone());
    let redirect = "https://example.com/done";
    let form = PostForm::new(FILE_CONTENT)
        .field("success_action_redirect", redirect)
        .condition(json!(["eq", "$success_action_redirect", redirect]));
    let wire = send(&service, form.to_request()).await;

    assert_eq!(wire.status, StatusCode::SEE_OTHER, "{}", wire.body);
    assert_eq!(
        header(&wire, "location"),
        Some("https://example.com/done?bucket=test-bucket&key=test-key&etag=5eb63bbbe01eeed093cb22bb8f5acdc3")
    );
    assert_eq!(wire.body, "");
    assert_eq!(s3.stored(), vec![stored_file(None)]);
}

#[tokio::test]
async fn success_action_redirect_appends_to_an_existing_query() {
    let s3 = TestS3::new();
    let service = service(s3.clone());
    let redirect = "https://example.com/done?token=abc";
    let form = PostForm::new(FILE_CONTENT)
        .field("success_action_redirect", redirect)
        .condition(json!(["eq", "$success_action_redirect", redirect]));
    let wire = send(&service, form.to_request()).await;

    assert_eq!(wire.status, StatusCode::SEE_OTHER, "{}", wire.body);
    assert_eq!(
        header(&wire, "location"),
        Some("https://example.com/done?token=abc&bucket=test-bucket&key=test-key&etag=5eb63bbbe01eeed093cb22bb8f5acdc3")
    );
    assert_eq!(wire.body, "");
    assert_eq!(s3.stored(), vec![stored_file(None)]);
}

#[tokio::test]
async fn a_policy_condition_mismatch_is_invalid_policy_document() {
    let s3 = TestS3::new();
    let service = service(s3.clone());
    let form = PostForm::new(FILE_CONTENT).key_condition(Some("other-key"));
    let wire = send(&service, form.to_request()).await;

    assert_error(&wire, StatusCode::BAD_REQUEST, "InvalidPolicyDocument");
    assert_eq!(s3.stored(), Vec::new());
}

#[tokio::test]
async fn a_form_field_without_a_condition_is_access_denied() {
    let s3 = TestS3::new();
    let service = service(s3.clone());
    let form = PostForm::new(FILE_CONTENT).field("x-amz-meta-note", "hello");
    let wire = send(&service, form.to_request()).await;

    assert_error(&wire, StatusCode::FORBIDDEN, "AccessDenied");
    assert_eq!(s3.stored(), Vec::new());
}

#[tokio::test]
async fn a_form_without_a_key_is_invalid_request() {
    let s3 = TestS3::new();
    let service = service(s3.clone());
    let form = PostForm::new(FILE_CONTENT).key(None).key_condition(None).no_content_type();
    let wire = send(&service, form.to_request()).await;

    assert_error(&wire, StatusCode::BAD_REQUEST, "InvalidRequest");
    assert_eq!(s3.stored(), Vec::new());
}

#[tokio::test]
async fn a_file_over_the_policy_maximum_is_entity_too_large() {
    let s3 = TestS3::new();
    let service = service(s3.clone());
    let form = PostForm::new("0123456789abcdef").condition(json!(["content-length-range", 0, 10]));
    let wire = send(&service, form.to_request()).await;

    assert_error(&wire, StatusCode::BAD_REQUEST, "EntityTooLarge");
    assert_eq!(s3.stored(), Vec::new());
}

#[tokio::test]
async fn a_file_below_the_policy_minimum_is_entity_too_small() {
    let s3 = TestS3::new();
    let service = service(s3.clone());
    let form = PostForm::new(FILE_CONTENT).condition(json!(["content-length-range", 100, 10240]));
    let wire = send(&service, form.to_request()).await;

    assert_error(&wire, StatusCode::BAD_REQUEST, "EntityTooSmall");
    assert_eq!(s3.stored(), Vec::new());
}

#[tokio::test]
async fn a_successful_upload_reaches_the_implementation_byte_for_byte() {
    let s3 = TestS3::new();
    let service = service(s3.clone());
    let form = PostForm::new(FILE_CONTENT)
        .field("Content-MD5", FILE_CONTENT_MD5)
        .condition(json!(["eq", "$Content-MD5", FILE_CONTENT_MD5]));
    let wire = send(&service, form.to_request()).await;

    assert_eq!(wire.status, StatusCode::NO_CONTENT, "{}", wire.body);
    assert_eq!(s3.stored(), vec![stored_file(Some(FILE_CONTENT_MD5))]);

    // The bytes and the digest the implementation reads are the file part's, and the ETag it
    // reports is the MD5 of exactly those bytes: the 201 form of the same upload carries it.
    let stored = s3.stored();
    let stored = stored.first().expect("the upload reached the implementation");
    assert_eq!(stored.body, FILE_CONTENT.as_bytes());

    // Both pinned digests have to be the MD5 of the pinned bytes, not merely well-formed
    // constants: deriving them here is what makes the claim above checkable.
    let digest = Md5::digest(FILE_CONTENT.as_bytes());
    let mut hex = String::new();
    for byte in &digest {
        write!(hex, "{byte:02x}").expect("writing to a String cannot fail");
    }
    assert_eq!(hex, STORED_ETAG);
    assert_eq!(base64_simd::STANDARD.encode_to_string(digest.as_slice()), FILE_CONTENT_MD5);
}
