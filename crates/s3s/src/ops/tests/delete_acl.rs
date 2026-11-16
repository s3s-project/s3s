// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

use super::common::{ACCESS_KEY, SECRET_KEY, ccx, ctx, signed_request, test_config};
use super::*;
use crate::auth::SimpleAuth;
use crate::config::StaticConfigProvider;
use crate::dto::{DeleteBucketInput, DeleteBucketOutput, DeleteObjectInput, DeleteObjectOutput};
use crate::protocol::S3Response;
use hyper::{StatusCode, Version};
use std::sync::atomic::{AtomicUsize, Ordering};

#[derive(Default)]
struct DeleteRecorder {
    buckets: AtomicUsize,
    objects: AtomicUsize,
}

#[async_trait::async_trait]
impl S3 for DeleteRecorder {
    async fn delete_bucket(&self, _req: S3Request<DeleteBucketInput>) -> S3Result<S3Response<DeleteBucketOutput>> {
        self.buckets.fetch_add(1, Ordering::SeqCst);
        Ok(S3Response::new(DeleteBucketOutput::default()))
    }

    async fn delete_object(&self, _req: S3Request<DeleteObjectInput>) -> S3Result<S3Response<DeleteObjectOutput>> {
        self.objects.fetch_add(1, Ordering::SeqCst);
        Ok(S3Response::new(DeleteObjectOutput::default()))
    }
}

fn request(method: Method, path: &str) -> Request {
    signed_request(
        method,
        Version::HTTP_11,
        &format!("http://localhost{path}"),
        s3s_sigv4::EMPTY_STRING_SHA256_HASH,
        &[("content-length", "0")],
    )
}

#[tokio::test]
async fn delete_acl_never_calls_delete_handlers() {
    for operation_id_routing in [false, true] {
        let recorder = Arc::new(DeleteRecorder::default());
        let mut parts = ctx();
        parts.s3 = recorder.clone();
        parts.auth = Some(SimpleAuth::from_single(ACCESS_KEY, SECRET_KEY));
        parts.config = Arc::new(StaticConfigProvider::new(Arc::new(S3Config {
            operation_id_routing,
            ..test_config().snapshot().as_ref().clone()
        })));
        let ccx = ccx(&parts, true, false, None);

        for (path, operation) in [("/bucket", "DeleteBucket"), ("/bucket/key", "DeleteObject")] {
            for query in [
                "acl",
                "acl=",
                "%61cl",
                "acl=value",
                "acl&acl",
                "acl&versionId=version",
                "tagging&acl",
                "uploadId=upload&acl",
                "policy&acl",
            ] {
                for declaration in [String::new(), format!("&x-id={operation}")] {
                    let path = format!("{path}?{query}{declaration}");
                    let mut req = request(Method::DELETE, &path);
                    let resp = call(&mut req, &ccx).await.unwrap();
                    assert_eq!(resp.status, StatusCode::METHOD_NOT_ALLOWED, "{path}, OIR={operation_id_routing}");
                    let body = resp.body.bytes().unwrap();
                    assert!(String::from_utf8_lossy(&body).contains("<Code>MethodNotAllowed</Code>"));
                }
            }
        }
        assert_eq!(recorder.buckets.load(Ordering::SeqCst), 0);
        assert_eq!(recorder.objects.load(Ordering::SeqCst), 0);

        for path in ["/bucket", "/bucket/key", "/bucket/key?versionId=version"] {
            let mut req = request(Method::DELETE, path);
            assert_eq!(call(&mut req, &ccx).await.unwrap().status, StatusCode::NO_CONTENT);
        }
        assert_eq!(recorder.buckets.load(Ordering::SeqCst), 1);
        assert_eq!(recorder.objects.load(Ordering::SeqCst), 2);
    }
}

#[tokio::test]
async fn delete_acl_preserves_authentication_and_operation_id_errors() {
    let mut parts = ctx();
    parts.config = test_config();
    parts.auth = Some(SimpleAuth::from_single(ACCESS_KEY, SECRET_KEY));
    let ccx = ccx(&parts, true, false, None);

    for path in ["/bucket?acl", "/bucket/key?acl", "/bucket/key?acl&x-id=DeleteObject"] {
        let mut req = request(Method::DELETE, path);
        // Alter the signed host to invalidate the signature without changing the query.
        req.headers.insert(header::HOST, "other-host".parse().unwrap());
        let err = prepare(&mut req, &ccx).await.err().unwrap();
        assert_eq!(err.code(), &S3ErrorCode::SignatureDoesNotMatch, "{path}");
    }

    let mut req = request(Method::DELETE, "/bucket/key?acl&x-id=GetObject");
    let err = prepare(&mut req, &ccx).await.err().unwrap();
    assert_eq!(err.code(), &S3ErrorCode::InvalidRequest);
}

#[tokio::test]
async fn acl_reads_writes_and_other_delete_routes_remain_available() {
    let parts = ctx();
    let ccx = ccx(&parts, false, false, None);
    for (method, path, expected) in [
        (Method::GET, "/bucket?acl", "GetBucketAcl"),
        (Method::PUT, "/bucket?acl", "PutBucketAcl"),
        (Method::GET, "/bucket/key?acl", "GetObjectAcl"),
        (Method::PUT, "/bucket/key?acl", "PutObjectAcl"),
        (Method::DELETE, "/bucket?policy", "DeleteBucketPolicy"),
        (Method::DELETE, "/bucket?tagging", "DeleteBucketTagging"),
        (Method::DELETE, "/bucket/key?tagging", "DeleteObjectTagging"),
        (Method::DELETE, "/bucket/key?uploadId=upload", "AbortMultipartUpload"),
        (Method::DELETE, "/bucket/key?unknown=acl", "DeleteObject"),
        (Method::DELETE, "/bucket/key?ACL", "DeleteObject"),
    ] {
        let mut req = Request::from(
            hyper::Request::builder()
                .method(method)
                .uri(format!("http://localhost{path}"))
                .header(header::CONTENT_LENGTH, "0")
                .body(Body::empty())
                .unwrap(),
        );
        let Prepare::S3(op) = prepare(&mut req, &ccx).await.unwrap() else {
            panic!("unexpected custom route for {path}");
        };
        assert_eq!(op.name(), expected, "{path}");
    }
}
