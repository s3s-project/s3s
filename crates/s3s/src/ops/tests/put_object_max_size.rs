// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

//! `S3Config::put_object_max_size`: which operations it covers, when it rejects before
//! dispatch, and how it interacts with `Content-Length` and aws-chunked bodies.

use super::common::*;
use super::*;

mod put_object_max_size_tests {
    use super::*;

    use crate::auth::{SecretKey, SimpleAuth};
    use crate::config::{S3Config, S3ConfigProvider, StaticConfigProvider};
    use crate::error::StdError;
    use crate::s3_trait::S3;
    use bytes::Bytes;
    use futures::StreamExt;
    use std::sync::Arc;

    /// The streaming-body predicate is model-driven: exactly the operations
    /// whose input carries a `StreamingBlob` payload (`PutObject`,
    /// `UploadPart`, `WriteGetObjectResponse`). `PostObject` is excluded —
    /// its body is the multipart file stream governed by
    /// `post_object_max_file_size`.
    #[test]
    fn has_streaming_body_matches_streaming_payload_operations() {
        assert!(PutObject.has_streaming_body());
        assert!(UploadPart.has_streaming_body());
        assert!(WriteGetObjectResponse.has_streaming_body());
        assert!(!PostObject.has_streaming_body());
        assert!(!GetObject.has_streaming_body());
        assert!(!DeleteObjects.has_streaming_body());
        assert!(!PutBucketPolicy.has_streaming_body());
    }

    fn test_config(put_object_max_size: Option<u64>) -> Arc<dyn S3ConfigProvider> {
        Arc::new(StaticConfigProvider::new(Arc::new(S3Config {
            put_object_max_size,
            presigned_url_max_skew_time_secs: u32::MAX,
            ..Default::default()
        })))
    }

    fn test_context<'a>(
        s3: &'a Arc<dyn S3>,
        config: &'a Arc<dyn S3ConfigProvider>,
        auth: Option<&'a dyn crate::auth::S3Auth>,
    ) -> CallContext<'a> {
        CallContext {
            s3,
            config,
            host: None,
            auth,
            access: None,
            route: None,
            validation: None,
        }
    }

    fn plain_put_request(body: Bytes) -> Request {
        Request::from(
            hyper::Request::builder()
                .method(Method::PUT)
                .uri("http://localhost/test-bucket/test-key")
                .header(crate::header::HOST, "localhost")
                .header(hyper::header::CONTENT_LENGTH, body.len())
                .body(Body::from(body))
                .unwrap(),
        )
    }

    fn upload_part_request(body: Bytes) -> Request {
        Request::from(
            hyper::Request::builder()
                .method(Method::PUT)
                .uri("http://localhost/test-bucket/test-key?partNumber=1&uploadId=test-upload")
                .header(crate::header::HOST, "localhost")
                .header(hyper::header::CONTENT_LENGTH, body.len())
                .body(Body::from(body))
                .unwrap(),
        )
    }

    async fn collect_stream<S>(mut stream: S) -> Result<Bytes, StdError>
    where
        S: futures::Stream<Item = Result<Bytes, StdError>> + Unpin,
    {
        let mut collected = Vec::new();
        while let Some(chunk) = stream.next().await {
            collected.extend_from_slice(&chunk?);
        }
        Ok(Bytes::from(collected))
    }

    fn signed_aws_chunked_request(chunk_data: &Bytes, access_key: &str, secret_key: &SecretKey, upload_part: bool) -> Request {
        let method = Method::PUT;
        let uri_path = "/test-bucket/test-key";
        let query: &[(&str, &str)] = if upload_part {
            &[("partNumber", "1"), ("uploadId", "test-upload")]
        } else {
            &[]
        };
        let amz_date = s3s_sigv4::AmzDate::parse("20130524T000000Z").unwrap();
        let decoded_content_length = chunk_data.len().to_string();
        let headers_for_signing = [
            ("host", "s3.amazonaws.com"),
            ("x-amz-content-sha256", "STREAMING-AWS4-HMAC-SHA256-PAYLOAD"),
            ("x-amz-date", "20130524T000000Z"),
            ("x-amz-decoded-content-length", decoded_content_length.as_str()),
        ];
        let canonical_request = s3s_sigv4::create_canonical_request(
            method.as_str(),
            uri_path,
            query,
            headers_for_signing,
            s3s_sigv4::Payload::MultipleChunks,
        );
        let seed_string_to_sign = s3s_sigv4::create_string_to_sign(&canonical_request, &amz_date, "us-east-1", "s3");
        let seed_signature =
            s3s_sigv4::calculate_signature(&seed_string_to_sign, secret_key.expose(), &amz_date, "us-east-1", "s3");

        let chunk_string_to_sign = s3s_sigv4::create_chunk_string_to_sign(
            &amz_date,
            "us-east-1",
            "s3",
            seed_signature.as_str(),
            std::slice::from_ref(chunk_data),
        );
        let chunk_signature =
            s3s_sigv4::calculate_signature(&chunk_string_to_sign, secret_key.expose(), &amz_date, "us-east-1", "s3");
        let final_string_to_sign =
            s3s_sigv4::create_chunk_string_to_sign(&amz_date, "us-east-1", "s3", chunk_signature.as_str(), &[] as &[Vec<u8>]);
        let final_signature =
            s3s_sigv4::calculate_signature(&final_string_to_sign, secret_key.expose(), &amz_date, "us-east-1", "s3");

        let mut streaming_body = Vec::new();
        streaming_body
            .extend_from_slice(format!("{:x};chunk-signature={}\r\n", chunk_data.len(), chunk_signature.as_str()).as_bytes());
        streaming_body.extend_from_slice(chunk_data);
        streaming_body.extend_from_slice(b"\r\n");
        streaming_body.extend_from_slice(format!("0;chunk-signature={}\r\n\r\n", final_signature.as_str()).as_bytes());

        let authorization = format!(
            "AWS4-HMAC-SHA256 Credential={access_key}/20130524/us-east-1/s3/aws4_request, SignedHeaders=host;x-amz-content-sha256;x-amz-date;x-amz-decoded-content-length, Signature={}",
            seed_signature.as_str(),
        );

        Request::from(
            hyper::Request::builder()
                .method(method)
                .uri(format!(
                    "https://s3.amazonaws.com{uri_path}?{}",
                    serde_urlencoded::to_string(query).unwrap()
                ))
                .header(crate::header::HOST, "s3.amazonaws.com")
                .header(hyper::header::CONTENT_LENGTH, streaming_body.len())
                .header("content-encoding", "aws-chunked")
                .header(crate::header::AUTHORIZATION, authorization)
                .header(crate::header::X_AMZ_CONTENT_SHA256, "STREAMING-AWS4-HMAC-SHA256-PAYLOAD")
                .header(crate::header::X_AMZ_DATE, "20130524T000000Z")
                .header(crate::header::X_AMZ_DECODED_CONTENT_LENGTH, decoded_content_length)
                .body(Body::from(Bytes::from(streaming_body)))
                .unwrap(),
        )
    }

    #[tokio::test]
    async fn none_leaves_plain_put_stream_unchanged() {
        let s3: Arc<dyn S3> = Arc::new(post_policy_test_helpers::TestS3NoOp);
        let config = test_config(None);
        let ccx = test_context(&s3, &config, None);
        let expected_body = Bytes::from_static(b"hello");
        let mut req = plain_put_request(expected_body.clone());

        let Prepare::S3(op) = super::prepare(&mut req, &ccx).await.expect("prepare should succeed") else {
            panic!("plain PUT should resolve to an S3 operation");
        };
        assert_eq!(op.name(), "PutObject");

        let input = generated::PutObject::deserialize_http(&mut req).expect("deserialize should succeed");
        let body = input.body.expect("put object input should carry a body");
        let collected = collect_stream(body).await.expect("unlimited stream should be readable");
        assert_eq!(collected, expected_body);
    }

    #[tokio::test]
    async fn rejects_oversized_plain_put_before_dispatch() {
        let s3: Arc<dyn S3> = Arc::new(post_policy_test_helpers::TestS3NoOp);
        let config = test_config(Some(4));
        let ccx = test_context(&s3, &config, None);
        let mut req = plain_put_request(Bytes::from_static(b"hello"));

        assert_oversized_before_dispatch(&mut req, &ccx).await;
    }

    #[tokio::test]
    async fn rejects_oversized_upload_part_before_dispatch() {
        let s3: Arc<dyn S3> = Arc::new(post_policy_test_helpers::TestS3NoOp);
        let config = test_config(Some(4));
        let ccx = test_context(&s3, &config, None);
        let mut req = upload_part_request(Bytes::from_static(b"hello"));

        assert_oversized_before_dispatch(&mut req, &ccx).await;
    }

    async fn assert_oversized_before_dispatch(req: &mut Request, ccx: &CallContext<'_>) {
        // The declared length alone must suffice; reading the body is a failure.
        let stream = futures::stream::poll_fn(|_| -> std::task::Poll<Option<Result<http_body::Frame<Bytes>, StdError>>> {
            panic!("oversized request body must not be polled");
        });
        req.body = Body::http_body(http_body_util::StreamBody::new(stream));
        let response = super::call(req, ccx).await.expect("size error should serialize");
        assert_eq!(response.status, StatusCode::BAD_REQUEST);
        let body = response.body.bytes().expect("error response should be buffered");
        let body = std::str::from_utf8(&body).unwrap();
        assert!(body.contains("<Code>EntityTooLarge</Code>"), "unexpected error response: {body}");
    }

    #[tokio::test]
    async fn rejects_oversized_write_get_object_response_before_dispatch() {
        let s3: Arc<dyn S3> = Arc::new(post_policy_test_helpers::TestS3NoOp);
        let config = test_config(Some(4));
        let ccx = test_context(&s3, &config, None);
        let mut req = Request::from(
            hyper::Request::builder()
                .method(Method::POST)
                .uri("http://localhost/test-bucket")
                .header(crate::header::HOST, "localhost")
                .header("x-amz-request-route", "test-route")
                .header("x-amz-request-token", "test-token")
                .header(hyper::header::CONTENT_LENGTH, 5)
                .body(Body::empty())
                .unwrap(),
        );
        assert_oversized_before_dispatch(&mut req, &ccx).await;
    }

    #[tokio::test]
    async fn exact_body_length_is_enforced_without_header_normalization() {
        let s3: Arc<dyn S3> = Arc::new(post_policy_test_helpers::TestS3NoOp);
        for normalize_content_length in [false, true] {
            let config: Arc<dyn S3ConfigProvider> = Arc::new(StaticConfigProvider::new(Arc::new(S3Config {
                put_object_max_size: Some(4),
                normalize_content_length,
                ..Default::default()
            })));
            let ccx = test_context(&s3, &config, None);
            for mut req in [
                plain_put_request(Bytes::from_static(b"hello")),
                upload_part_request(Bytes::from_static(b"hello")),
            ] {
                req.headers.remove(hyper::header::CONTENT_LENGTH);
                let err = super::prepare(&mut req, &ccx)
                    .await
                    .err()
                    .expect("known oversized body must fail");
                assert_eq!(err.code(), &S3ErrorCode::EntityTooLarge);
            }
        }
    }

    #[tokio::test]
    async fn plain_uploads_ignore_unrelated_decoded_length_headers() {
        let s3: Arc<dyn S3> = Arc::new(post_policy_test_helpers::TestS3NoOp);
        let config = test_config(Some(4));
        let ccx = test_context(&s3, &config, None);
        for has_content_length in [false, true] {
            for decoded_length in ["1", "999"] {
                let mut req = plain_put_request(Bytes::from_static(b"data"));
                if !has_content_length {
                    req.headers.remove(hyper::header::CONTENT_LENGTH);
                }
                req.headers.insert(
                    crate::header::X_AMZ_DECODED_CONTENT_LENGTH,
                    hyper::header::HeaderValue::from_static(decoded_length),
                );
                super::prepare(&mut req, &ccx)
                    .await
                    .expect("unrelated decoded length must not reject a valid body");
                let input = generated::PutObject::deserialize_http(&mut req).unwrap();
                assert_eq!(input.content_length, Some(4));
                assert_eq!(collect_stream(input.body.unwrap()).await.unwrap(), Bytes::from_static(b"data"));
            }
        }

        let mut req = plain_put_request(Bytes::from_static(b"hello"));
        req.headers
            .insert(crate::header::X_AMZ_DECODED_CONTENT_LENGTH, hyper::header::HeaderValue::from_static("1"));
        assert_oversized_before_dispatch(&mut req, &ccx).await;
    }

    #[tokio::test]
    async fn exact_limit_and_empty_uploads_are_accepted() {
        let s3: Arc<dyn S3> = Arc::new(post_policy_test_helpers::TestS3NoOp);
        for expected in [Bytes::new(), Bytes::from_static(b"hello")] {
            let config = test_config(Some(expected.len() as u64));
            let ccx = test_context(&s3, &config, None);
            for mut req in [plain_put_request(expected.clone()), upload_part_request(expected.clone())] {
                super::prepare(&mut req, &ccx)
                    .await
                    .expect("exact-limit upload should be accepted");
                assert_eq!(collect_stream(req.body).await.unwrap(), expected);
            }
        }
    }

    #[tokio::test]
    async fn unknown_and_underdeclared_uploads_keep_the_read_time_limit() {
        let s3: Arc<dyn S3> = Arc::new(post_policy_test_helpers::TestS3NoOp);
        let config = test_config(Some(4));
        let ccx = test_context(&s3, &config, None);
        for declared_length in [None, Some(4)] {
            for mut req in [plain_put_request(Bytes::new()), upload_part_request(Bytes::new())] {
                req.headers.remove(hyper::header::CONTENT_LENGTH);
                if let Some(length) = declared_length {
                    req.headers
                        .insert(hyper::header::CONTENT_LENGTH, hyper::header::HeaderValue::from(length));
                } else {
                    req.headers
                        .insert(hyper::header::TRANSFER_ENCODING, hyper::header::HeaderValue::from_static("chunked"));
                }
                let mut chunks = std::collections::VecDeque::from([Bytes::from_static(b"he"), Bytes::from_static(b"llo")]);
                let mut pending = false;
                let stream = futures::stream::poll_fn(move |cx| {
                    pending = !pending;
                    if pending {
                        cx.waker().wake_by_ref();
                        return std::task::Poll::Pending;
                    }
                    std::task::Poll::Ready(
                        chunks
                            .pop_front()
                            .map(|chunk| Ok::<_, StdError>(http_body::Frame::data(chunk))),
                    )
                });
                req.body = Body::http_body(http_body_util::StreamBody::new(stream));
                super::prepare(&mut req, &ccx)
                    .await
                    .expect("unknown or underdeclared stream must retain read-time enforcement");
                assert_eq!(req.body.next().await.unwrap().unwrap(), Bytes::from_static(b"he"));
                let err = req
                    .body
                    .next()
                    .await
                    .unwrap()
                    .expect_err("second chunk exceeds the remaining allowance");
                let err = err
                    .downcast_ref::<crate::BodySizeLimitExceeded>()
                    .expect("limit error must retain its type");
                assert_eq!(err.size, 3);
                assert_eq!(err.limit, 2);
            }
        }
    }

    #[tokio::test]
    async fn upload_parts_do_not_share_a_cumulative_limit() {
        let s3: Arc<dyn S3> = Arc::new(post_policy_test_helpers::TestS3NoOp);
        let config = test_config(Some(4));
        let ccx = test_context(&s3, &config, None);
        for part_number in [1, 2] {
            let mut req = upload_part_request(Bytes::from_static(b"data"));
            req.uri = format!("http://localhost/test-bucket/test-key?partNumber={part_number}&uploadId=test-upload")
                .parse()
                .unwrap();
            super::prepare(&mut req, &ccx)
                .await
                .expect("each part should receive its own limit");
            let input = generated::UploadPart::deserialize_http(&mut req).unwrap();
            assert_eq!(collect_stream(input.body.unwrap()).await.unwrap(), Bytes::from_static(b"data"));
        }
    }

    #[tokio::test]
    async fn empty_body_operations_are_unaffected_when_limit_is_set() {
        let s3: Arc<dyn S3> = Arc::new(post_policy_test_helpers::TestS3NoOp);
        let config = test_config(Some(0));
        let ccx = test_context(&s3, &config, None);
        let mut req = Request::from(
            hyper::Request::builder()
                .method(Method::GET)
                .uri("http://localhost/test-bucket/test-key")
                .header(crate::header::HOST, "localhost")
                .body(Body::empty())
                .unwrap(),
        );

        let Prepare::S3(op) = super::prepare(&mut req, &ccx).await.expect("prepare should succeed") else {
            panic!("GET object should resolve to an S3 operation");
        };
        assert_eq!(op.name(), "GetObject");

        let input = generated::GetObject::deserialize_http(&mut req).expect("deserialize should succeed");
        assert_eq!(input.bucket, "test-bucket");
        assert_eq!(input.key, "test-key");
        assert!(
            req.body
                .store_all_limited(0)
                .await
                .expect("empty limited body should be readable")
                .is_empty()
        );
    }

    /// A `ListObjects` response that echoes a request-controlled object key
    /// containing a control character must not produce an invalid XML body:
    /// the serialization layer rejects it with an internal error.
    #[tokio::test]
    async fn list_objects_with_control_character_key_rejected_at_serialization() {
        use crate::config::{S3ConfigProvider, StaticConfigProvider};
        use crate::http::{Body, Request};

        struct ControlCharS3;
        #[async_trait::async_trait]
        impl crate::s3_trait::S3 for ControlCharS3 {
            async fn list_objects(
                &self,
                _req: crate::S3Request<crate::dto::ListObjectsInput>,
            ) -> crate::error::S3Result<crate::protocol::S3Response<crate::dto::ListObjectsOutput>> {
                let output = crate::dto::ListObjectsOutput {
                    name: Some("bucket".into()),
                    contents: Some(vec![crate::dto::Object {
                        key: Some("a\x01b".into()),
                        ..Default::default()
                    }]),
                    ..Default::default()
                };
                Ok(crate::protocol::S3Response::new(output))
            }
        }

        let s3: Arc<dyn crate::s3_trait::S3> = Arc::new(ControlCharS3);
        let config: Arc<dyn S3ConfigProvider> = Arc::new(StaticConfigProvider::default());
        let ccx = CallContext {
            s3: &s3,
            config: &config,
            host: None,
            auth: None,
            access: None,
            route: None,
            validation: None,
        };

        let mut req = Request::from(
            hyper::Request::builder()
                .method(Method::GET)
                .uri("http://localhost/bucket")
                .header(crate::header::HOST, "localhost")
                .body(Body::empty())
                .unwrap(),
        );

        let resp = super::call(&mut req, &ccx).await.unwrap();
        assert_eq!(resp.status, hyper::StatusCode::INTERNAL_SERVER_ERROR);
    }

    #[tokio::test]
    async fn post_object_keeps_post_object_max_file_size_when_put_limit_is_set() {
        let s3: Arc<dyn S3> = Arc::new(post_policy_test_helpers::TestS3NoOp);
        let config: Arc<dyn S3ConfigProvider> = Arc::new(StaticConfigProvider::new(Arc::new(S3Config {
            post_object_max_file_size: 1024,
            put_object_max_size: Some(1),
            expected_region: Some("us-east-1".parse().expect("valid test region")),
            presigned_url_max_skew_time_secs: u32::MAX,
            ..Default::default()
        })));
        let auth = post_policy_test_helpers::create_test_auth();
        let ccx = test_context(&s3, &config, Some(&auth));
        let secret_key: SecretKey = "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY".into();
        let policy_json = &format!(
            r#"{{"expiration":"2030-01-01T00:00:00.000Z","conditions":[{}]}}"#,
            post_policy_test_helpers::BASE_CONDITIONS,
        );
        let file_content = "hello";
        let mut req = post_policy_test_helpers::build_post_object_request(policy_json, file_content, &secret_key, false);

        let Prepare::S3(op) = super::prepare(&mut req, &ccx).await.expect("prepare should succeed") else {
            panic!("POST Object should resolve to an S3 operation");
        };
        assert_eq!(op.name(), "PostObject");

        let stream = req
            .s3ext
            .post_object_stream
            .take()
            .expect("post object stream should be prepared");
        let collected = collect_stream(stream)
            .await
            .expect("POST Object stream should use its own limit");
        assert_eq!(collected, Bytes::from_static(b"hello"));
    }

    #[tokio::test]
    async fn aws_chunked_limit_uses_decoded_length() {
        let s3: Arc<dyn S3> = Arc::new(post_policy_test_helpers::TestS3NoOp);
        let access_key = "AKIAIOSFODNN7EXAMPLE";
        let secret_key: SecretKey = "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY".into();
        let auth = SimpleAuth::from_single(access_key, secret_key.clone());
        let decoded = Bytes::from_static(b"hello");

        for upload_part in [false, true] {
            for has_content_length in [false, true] {
                for normalize_content_length in [false, true] {
                    for limit in [4, 5] {
                        let config: Arc<dyn S3ConfigProvider> = Arc::new(StaticConfigProvider::new(Arc::new(S3Config {
                            put_object_max_size: Some(limit),
                            normalize_content_length,
                            presigned_url_max_skew_time_secs: u32::MAX,
                            ..Default::default()
                        })));
                        let ccx = test_context(&s3, &config, Some(&auth));
                        let mut req = signed_aws_chunked_request(&decoded, access_key, &secret_key, upload_part);
                        assert!(extract_content_length(&req).unwrap().unwrap() > limit);
                        if !has_content_length {
                            req.headers.remove(hyper::header::CONTENT_LENGTH);
                            req.headers
                                .insert(hyper::header::TRANSFER_ENCODING, hyper::header::HeaderValue::from_static("chunked"));
                        }
                        if limit < decoded.len() as u64 {
                            assert_oversized_before_dispatch(&mut req, &ccx).await;
                            continue;
                        }
                        let Prepare::S3(op) = super::prepare(&mut req, &ccx)
                            .await
                            .expect("decoded body at the limit should pass")
                        else {
                            panic!("upload should resolve to an S3 operation");
                        };
                        assert_eq!(op.name(), if upload_part { "UploadPart" } else { "PutObject" });
                        let (length, body) = if upload_part {
                            let input = generated::UploadPart::deserialize_http(&mut req).unwrap();
                            (input.content_length, input.body.unwrap())
                        } else {
                            let input = generated::PutObject::deserialize_http(&mut req).unwrap();
                            (input.content_length, input.body.unwrap())
                        };
                        assert_eq!(length, (has_content_length || normalize_content_length).then_some(5));
                        assert_eq!(collect_stream(body).await.unwrap(), decoded);
                    }
                }
            }
        }
    }

    #[tokio::test]
    async fn authentication_errors_take_precedence_over_upload_size() {
        let s3: Arc<dyn S3> = Arc::new(post_policy_test_helpers::TestS3NoOp);
        let access_key = "AKIAIOSFODNN7EXAMPLE";
        let auth = SimpleAuth::from_single(access_key, "correct-secret");
        let config = test_config(Some(4));
        let ccx = test_context(&s3, &config, Some(&auth));
        let mut req = signed_aws_chunked_request(&Bytes::from_static(b"hello"), access_key, &"wrong-secret".into(), false);
        let err = super::prepare(&mut req, &ccx)
            .await
            .err()
            .expect("bad signature should be rejected");
        assert_eq!(err.code(), &S3ErrorCode::SignatureDoesNotMatch);
    }
}
