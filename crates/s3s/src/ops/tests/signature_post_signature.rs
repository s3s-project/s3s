// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

//! POST form signatures.

use super::common::*;
use crate::ops::signature::*;

use crate::error::S3ErrorCode;
use crate::http::Body;
use hyper::{HeaderMap, Method, Uri};
use mime::Mime;
use s3s_sigv4::AmzDate;

#[tokio::test]
async fn post_signature_allows_anonymous() {
    use crate::config::{S3ConfigProvider, StaticConfigProvider};
    use std::sync::Arc;

    let boundary = "boundary123";
    let body = format!(
        "\r\n--{boundary}\r\n\
Content-Disposition: form-data; name=\"key\"; filename=\"key\"\r\n\r\n\
foo.txt\r\n\
--{boundary}\r\n\
Content-Disposition: form-data; name=\"file\"; filename=\"file.txt\"\r\n\
Content-Type: text/plain\r\n\r\n\
file content\r\n\
--{boundary}--\r\n"
    );
    let mut body = Body::from(body);
    let mime: Mime = format!("multipart/form-data; boundary={boundary}").parse().unwrap();

    let config: Arc<dyn S3ConfigProvider> = Arc::new(StaticConfigProvider::default());
    let method = Method::POST;
    let uri = Uri::from_static("http://localhost/test-bucket");
    let headers = HeaderMap::new();

    let mut cx = SignatureContext {
        auth: None,
        config: &config,
        req_version: ::http::Version::HTTP_11,
        req_method: &method,
        req_uri: &uri,
        req_body: &mut body,
        qs: None,
        hs: &headers,
        decoded_uri_path: "/test-bucket",
        raw_uri_path: "/test-bucket",
        vh_bucket: None,
        content_length: None,
        mime: Some(mime),
        decoded_content_length: None,
        transformed_body: None,
        multipart: None,
        trailing_headers: None,
    };

    let credentials = cx.check().await.unwrap();
    assert!(credentials.is_none(), "anonymous POST should not require credentials");

    let multipart = cx.multipart.expect("multipart should be stored");
    assert_eq!(multipart.find_field_value("key"), Some("foo.txt"));
    assert_eq!(multipart.file.name, "file.txt");
}

#[tokio::test]
async fn sig_v2_post_rejected_when_disabled() {
    let boundary = "boundary123";
    let body = format!(
        "\r\n--{boundary}\r\n\
Content-Disposition: form-data; name=\"signature\"\r\n\r\n\
abc\r\n\
--{boundary}\r\n\
Content-Disposition: form-data; name=\"key\"; filename=\"key\"\r\n\r\n\
foo.txt\r\n\
--{boundary}\r\n\
Content-Disposition: form-data; name=\"file\"; filename=\"file.txt\"\r\n\
Content-Type: text/plain\r\n\r\n\
file content\r\n\
--{boundary}--\r\n"
    );
    let mut body = Body::from(body);
    let mime: Mime = format!("multipart/form-data; boundary={boundary}").parse().unwrap();

    let config = sig_v2_test_config(false);
    let method = Method::POST;
    let uri = Uri::from_static("http://localhost/test-bucket");
    let headers = HeaderMap::new();
    let mut cx = sig_v2_test_context(&config, None, &method, &uri, &mut body, None, &headers, Some(mime));

    let err = cx.check().await.expect_err("SigV2 POST must be rejected when disabled");
    assert_eq!(err.code(), &S3ErrorCode::AccessDenied);
}

#[tokio::test]
async fn v4_post_signature_rejects_stale_request_time() {
    use crate::S3ErrorCode;
    use crate::auth::SecretKey;
    use crate::auth::SimpleAuth;
    use crate::config::{S3ConfigProvider, StaticConfigProvider};
    use std::sync::Arc;

    let access_key = "AKIAIOSFODNN7EXAMPLE";
    let secret_key: SecretKey = "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY".into();
    let auth = SimpleAuth::from_single(access_key, secret_key.clone());
    let config: Arc<dyn S3ConfigProvider> = Arc::new(StaticConfigProvider::default());

    let skew = time::Duration::seconds(i64::from(config.snapshot().presigned_url_max_skew_time_secs));
    let request_time = time::OffsetDateTime::now_utc() - skew - time::Duration::minutes(1);
    let amz_date_str = fmt_current_amz_date(request_time);
    let amz_date = AmzDate::parse(&amz_date_str).unwrap();

    // Construct a proper POST policy JSON with the required eq conditions
    let policy_json = format!(
        r#"{{"expiration":"2099-01-01T00:00:00Z","conditions":[{{"x-amz-date":"{amz_date}"}},{{"x-amz-credential":"{access_key}/{date}/us-east-1/s3/aws4_request"}},{{"x-amz-algorithm":"AWS4-HMAC-SHA256"}}]}}"#,
        amz_date = amz_date_str,
        access_key = access_key,
        date = amz_date.fmt_date(),
    );
    let policy_b64 = base64_simd::STANDARD.encode_to_string(&policy_json);
    let signature = s3s_sigv4::calculate_signature(&policy_b64, secret_key.expose(), &amz_date, "us-east-1", "s3");
    let boundary = "boundary123";
    let body = format!(
        concat!(
            "\r\n--{boundary}\r\n",
            "Content-Disposition: form-data; name=\"x-amz-signature\"\r\n\r\n",
            "{signature}\r\n",
            "--{boundary}\r\n",
            "Content-Disposition: form-data; name=\"policy\"\r\n\r\n",
            "{policy_b64}\r\n",
            "--{boundary}\r\n",
            "Content-Disposition: form-data; name=\"x-amz-algorithm\"\r\n\r\n",
            "AWS4-HMAC-SHA256\r\n",
            "--{boundary}\r\n",
            "Content-Disposition: form-data; name=\"x-amz-credential\"\r\n\r\n",
            "{access_key}/{date}/us-east-1/s3/aws4_request\r\n",
            "--{boundary}\r\n",
            "Content-Disposition: form-data; name=\"x-amz-date\"\r\n\r\n",
            "{amz_date}\r\n",
            "--{boundary}\r\n",
            "Content-Disposition: form-data; name=\"file\"; filename=\"a.txt\"\r\n",
            "Content-Type: text/plain\r\n\r\n",
            "hello\r\n",
            "--{boundary}--\r\n"
        ),
        access_key = access_key,
        amz_date = amz_date_str,
        boundary = boundary,
        date = amz_date.fmt_date(),
        policy_b64 = policy_b64,
        signature = signature.as_str(),
    );

    let mime: Mime = format!("multipart/form-data; boundary={boundary}").parse().unwrap();
    let method = Method::POST;
    let uri = Uri::from_static("http://localhost/test-bucket");
    let headers = HeaderMap::new();
    let mut body = Body::from(body);
    let mut cx = SignatureContext {
        auth: Some(&auth),
        config: &config,
        req_version: ::http::Version::HTTP_11,
        req_method: &method,
        req_uri: &uri,
        req_body: &mut body,
        qs: None,
        hs: &headers,
        decoded_uri_path: "/test-bucket",
        raw_uri_path: "/test-bucket",
        vh_bucket: None,
        content_length: None,
        mime: Some(mime),
        decoded_content_length: None,
        transformed_body: None,
        multipart: None,
        trailing_headers: None,
    };

    let err = cx
        .check_post_signature()
        .await
        .expect_err("stale signed POST policy should be rejected");
    assert_eq!(err.code(), &S3ErrorCode::RequestTimeTooSkewed);
}
