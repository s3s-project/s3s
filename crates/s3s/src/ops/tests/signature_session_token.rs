// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

//! Session tokens of temporary credentials: every authentication path hands the token to
//! `S3Auth::get_secret_key_with_context`, and the verified `Credentials` carry it.

use super::common::*;
use crate::ops::CallContext;

use crate::access::{S3Access, S3AccessContext};
use crate::auth::{S3Auth, S3AuthContext, SecretKey, SimpleAuth};
use crate::error::{S3ErrorCode, S3Result};
use crate::http::{Body, OrderedQs, Request};
use hyper::{HeaderMap, Method, StatusCode, Uri, Version};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};

const TOKEN: &str = "FwoGZXIvYXdzEBYaDHqa0A";

/// Serves the secret key only to a request that carries `token`: a path that drops the
/// token cannot authenticate at all, so a passing request proves the token was delivered.
struct TokenGatedAuth {
    access_key: &'static str,
    secret_key: &'static str,
    token: &'static str,
}

impl TokenGatedAuth {
    fn with_common_credentials() -> Self {
        Self {
            access_key: ACCESS_KEY,
            secret_key: SECRET_KEY,
            token: TOKEN,
        }
    }
}

#[async_trait::async_trait]
impl S3Auth for TokenGatedAuth {
    async fn get_secret_key(&self, _access_key: &str) -> S3Result<SecretKey> {
        panic!("the verifier must call get_secret_key_with_context")
    }

    async fn get_secret_key_with_context(&self, cx: &S3AuthContext<'_>) -> S3Result<SecretKey> {
        if cx.access_key() == self.access_key && cx.session_token() == Some(self.token) {
            Ok(SecretKey::from(self.secret_key))
        } else {
            Err(s3_error!(InvalidAccessKeyId))
        }
    }
}

/// Records the session token `S3Access::check` observes on each call.
#[derive(Default)]
struct RecordingAccess {
    observed: Mutex<Vec<Option<String>>>,
}

#[async_trait::async_trait]
impl S3Access for RecordingAccess {
    async fn check(&self, cx: &mut S3AccessContext<'_>) -> S3Result<()> {
        let token = cx.credentials().and_then(|cred| cred.session_token.clone());
        self.observed.lock().unwrap().push(token);
        Ok(())
    }
}

fn token_header_request(extra_headers: &[(&'static str, &'static str)]) -> Request {
    signed_request(
        Method::GET,
        Version::HTTP_11,
        "http://localhost/test-bucket/test-key",
        EMPTY_SHA256,
        extra_headers,
    )
}

#[tokio::test]
async fn v4_header_auth_passes_the_token_to_auth_and_access() {
    let test_s3 = Arc::new(TestS3::default());
    let s3: Arc<dyn crate::s3_trait::S3> = test_s3.clone();
    let config = test_config();
    let auth = TokenGatedAuth::with_common_credentials();
    let access = RecordingAccess::default();
    let ccx = CallContext {
        s3: &s3,
        config: &config,
        host: None,
        auth: Some(&auth),
        access: Some(&access),
        route: None,
        validation: None,
    };

    let mut req = token_header_request(&[("x-amz-security-token", TOKEN)]);
    let response = crate::ops::call(&mut req, &ccx).await.expect("the request must be answered");
    assert_eq!(response.status, StatusCode::OK);
    assert_eq!(test_s3.get_object.load(Ordering::SeqCst), 1);
    assert_eq!(*access.observed.lock().unwrap(), [Some(TOKEN.to_owned())]);
}

#[tokio::test]
async fn header_auth_without_a_token_passes_none() {
    let test_s3 = Arc::new(TestS3::default());
    let s3: Arc<dyn crate::s3_trait::S3> = test_s3.clone();
    let config = test_config();
    let auth = SimpleAuth::from_single(ACCESS_KEY, SECRET_KEY);
    let access = RecordingAccess::default();
    let ccx = CallContext {
        access: Some(&access),
        ..test_context(&s3, &config, &auth)
    };

    let mut req = token_header_request(&[]);
    let response = crate::ops::call(&mut req, &ccx).await.expect("the request must be answered");
    assert_eq!(response.status, StatusCode::OK);
    assert_eq!(*access.observed.lock().unwrap(), [None]);

    // Red control for the gated provider: without the token it refuses the credential.
    let auth = TokenGatedAuth::with_common_credentials();
    let ccx = CallContext {
        s3: &s3,
        config: &config,
        host: None,
        auth: Some(&auth),
        access: None,
        route: None,
        validation: None,
    };
    let mut req = token_header_request(&[]);
    let response = crate::ops::call(&mut req, &ccx).await.expect("the request must be answered");
    assert_eq!(response.status, StatusCode::FORBIDDEN);
    assert_eq!(
        test_s3.get_object.load(Ordering::SeqCst),
        1,
        "the rejected request must not be dispatched"
    );
}

#[tokio::test]
async fn v4_header_auth_rejects_a_repeated_token() {
    let config = test_config();
    let auth = TokenGatedAuth::with_common_credentials();
    let method = Method::GET;
    let uri = Uri::from_static("http://localhost/test.txt");
    let authorization = sign_request(&method, &uri, EMPTY_SHA256, &[("x-amz-security-token", TOKEN)]);
    let headers = headers_from_slice(&[
        ("host", "localhost"),
        ("x-amz-content-sha256", EMPTY_SHA256),
        ("x-amz-date", AMZ_DATE),
        ("x-amz-security-token", TOKEN),
        ("x-amz-security-token", "second"),
        ("authorization", authorization.as_str()),
    ]);
    let mut body = Body::empty();
    let mut cx = sig_v2_test_context(&config, Some(&auth), &method, &uri, &mut body, None, &headers, None);

    let err = cx
        .v4_check()
        .await
        .expect("header auth must be detected")
        .expect_err("a repeated token is ambiguous");
    assert_eq!(err.code(), &S3ErrorCode::InvalidRequest);
    assert_eq!(err.message(), Some("duplicate header: x-amz-security-token"));
}

#[tokio::test]
async fn v4_presigned_url_passes_the_token() {
    let config = test_config();
    let auth = TokenGatedAuth::with_common_credentials();
    let method = Method::GET;
    let uri = Uri::from_static("http://localhost/test.txt");
    let amz_date = s3s_sigv4::AmzDate::parse(&fmt_current_amz_date(jiff::Timestamp::now())).unwrap();
    let query_fields = vec![
        ("X-Amz-Algorithm".to_owned(), "AWS4-HMAC-SHA256".to_owned()),
        (
            "X-Amz-Credential".to_owned(),
            format!("{ACCESS_KEY}/{}/{REGION}/{SERVICE}/aws4_request", amz_date.fmt_date()),
        ),
        ("X-Amz-Date".to_owned(), amz_date.fmt_iso8601().to_string()),
        ("X-Amz-Expires".to_owned(), "3600".to_owned()),
        ("X-Amz-Security-Token".to_owned(), TOKEN.to_owned()),
        ("X-Amz-SignedHeaders".to_owned(), "host".to_owned()),
    ];
    let canonical_request =
        s3s_sigv4::create_presigned_canonical_request(method.as_str(), "/test.txt", &query_fields, [("host", "localhost")]);
    let string_to_sign = s3s_sigv4::create_string_to_sign(&canonical_request, &amz_date, REGION, SERVICE);
    let signature = s3s_sigv4::calculate_signature(&string_to_sign, SECRET_KEY, &amz_date, REGION, SERVICE);
    let mut query_fields = query_fields;
    query_fields.push(("X-Amz-Signature".to_owned(), signature));
    let qs = OrderedQs::from_vec_unchecked(query_fields);
    let headers = headers_from_slice(&[("host", "localhost")]);
    let mut body = Body::empty();
    let mut cx = sig_v2_test_context(&config, Some(&auth), &method, &uri, &mut body, Some(&qs), &headers, None);

    let cred = cx
        .v4_check()
        .await
        .expect("presigned auth must be detected")
        .expect("the token-derived secret must verify the URL");
    assert_eq!(cred.access_key, ACCESS_KEY);
    assert_eq!(cred.session_token.as_deref(), Some(TOKEN));
}

#[tokio::test]
async fn v4_post_policy_passes_the_token() {
    let test_s3 = Arc::new(post_policy_test_helpers::TestS3WithPostTracking {
        post_calls: std::sync::atomic::AtomicUsize::new(0),
    });
    let s3: Arc<dyn crate::s3_trait::S3> = test_s3.clone();
    let config = post_policy_test_helpers::create_test_config(10 * 1024);
    let auth = TokenGatedAuth {
        access_key: "AKIAIOSFODNN7EXAMPLE",
        secret_key: "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY",
        token: TOKEN,
    };
    let access = RecordingAccess::default();
    let ccx = CallContext {
        access: Some(&access),
        ..post_policy_test_helpers::create_test_context(&s3, &config, &auth)
    };

    let secret_key = SecretKey::from(auth.secret_key);
    let policy_json = format!(
        r#"{{"expiration":"2030-01-01T00:00:00.000Z","conditions":[["content-length-range",0,10240],["eq","$x-amz-security-token","{TOKEN}"],{}]}}"#,
        post_policy_test_helpers::BASE_CONDITIONS,
    );
    let mut req = post_policy_test_helpers::build_post_object_request_with(
        &policy_json,
        "hello",
        &secret_key,
        false,
        &[("x-amz-security-token", TOKEN)],
        &[],
    );

    let response = crate::ops::call(&mut req, &ccx).await.expect("the request must be answered");
    assert!(response.status.is_success(), "POST Object got {:?}", response.status);
    assert_eq!(test_s3.post_calls.load(Ordering::SeqCst), 1);
    assert_eq!(*access.observed.lock().unwrap(), [Some(TOKEN.to_owned())]);
}

#[tokio::test]
async fn sig_v2_header_auth_passes_the_token() {
    let config = sig_v2_test_config(true);
    let auth = TokenGatedAuth::with_common_credentials();
    let method = Method::GET;
    let uri = Uri::from_static("https://s3.amazonaws.com/test.txt");
    let x_amz_date = fmt_current_amz_date(jiff::Timestamp::now());
    let string_to_sign = s3s_sigv2::create_string_to_sign(
        s3s_sigv2::Mode::HeaderAuth,
        "GET",
        "/test.txt",
        None,
        &[("x-amz-date", &x_amz_date), ("x-amz-security-token", TOKEN)],
        None,
    );
    let signature = s3s_sigv2::calculate_signature(SECRET_KEY, &string_to_sign);
    let headers = headers_from_slice(&[
        ("authorization", &format!("AWS {ACCESS_KEY}:{signature}")),
        ("x-amz-date", &x_amz_date),
        ("x-amz-security-token", TOKEN),
    ]);
    let mut body = Body::empty();
    let mut cx = sig_v2_test_context(&config, Some(&auth), &method, &uri, &mut body, None, &headers, None);

    let cred = cx
        .v2_check()
        .await
        .expect("v2 header auth must be detected")
        .expect("the token-derived secret must verify the request");
    assert_eq!(cred.session_token.as_deref(), Some(TOKEN));
}

#[tokio::test]
async fn sig_v2_presigned_url_passes_the_token_from_the_query() {
    let config = sig_v2_test_config(true);
    let auth = TokenGatedAuth::with_common_credentials();
    let method = Method::GET;
    let uri = Uri::from_static("https://s3.amazonaws.com/test.txt");
    let expires = (jiff::Timestamp::now().as_second() + 3_600).to_string();
    let qs_pairs = vec![
        ("AWSAccessKeyId".to_owned(), ACCESS_KEY.to_owned()),
        ("Expires".to_owned(), expires),
        ("x-amz-security-token".to_owned(), TOKEN.to_owned()),
    ];
    let string_to_sign =
        s3s_sigv2::create_string_to_sign(s3s_sigv2::Mode::PresignedUrl, "GET", "/test.txt", Some(&qs_pairs), &[], None);
    let signature = s3s_sigv2::calculate_signature(SECRET_KEY, &string_to_sign);
    let mut qs_pairs = qs_pairs;
    qs_pairs.push(("Signature".to_owned(), signature));
    let qs = OrderedQs::from_vec_unchecked(qs_pairs);
    let headers = HeaderMap::new();
    let mut body = Body::empty();
    let mut cx = sig_v2_test_context(&config, Some(&auth), &method, &uri, &mut body, Some(&qs), &headers, None);

    let cred = cx
        .v2_check()
        .await
        .expect("v2 presigned auth must be detected")
        .expect("the token-derived secret must verify the URL");
    assert_eq!(cred.session_token.as_deref(), Some(TOKEN));
}
