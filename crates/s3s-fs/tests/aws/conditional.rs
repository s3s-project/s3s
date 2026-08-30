// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

use crate::case;
use crate::suite::{Conditional, FS_ROOT, create_bucket, delete_bucket, delete_object, do_multipart_upload};

use std::sync::Arc;

use std::fs;

use aws_sdk_s3::primitives::ByteStream;
use aws_sdk_s3::types::CompletedMultipartUpload;

use aws_sdk_s3::error::ProvideErrorMetadata;

use s3s_test::Result;
use s3s_test::tcx::TestContext;
use tracing::debug;
use uuid::Uuid;

pub fn register(tcx: &mut TestContext) {
    case!(tcx, FsServer, Conditional, test_if_none_match_wildcard);
    case!(tcx, FsServer, Conditional, test_put_object_if_match_wildcard);
    case!(tcx, FsServer, Conditional, test_put_object_if_match_etag);
    case!(tcx, FsServer, Conditional, test_put_object_if_match_multipart_etag);
    case!(tcx, FsServer, Conditional, test_put_object_if_match_legacy_md5_fallback);
    case!(tcx, FsServer, Conditional, test_put_object_if_match_rejects_weak_etag);
}

impl Conditional {
    async fn test_if_none_match_wildcard(self: Arc<Self>) -> Result<()> {
        let c = &self.s3;
        let bucket = format!("if-none-match-{}", Uuid::new_v4());
        let bucket = bucket.as_str();
        let key = "test-file.txt";
        let content1 = "initial content";
        let content2 = "updated content";

        create_bucket(c, bucket).await?;

        // Test 1: PUT with If-None-Match: * should succeed when object doesn't exist
        debug!("Test 1: PUT with If-None-Match: * on non-existent object");
        {
            let body = ByteStream::from_static(content1.as_bytes());
            let result = c
                .put_object()
                .bucket(bucket)
                .key(key)
                .body(body)
                .if_none_match("*")
                .send()
                .await;

            match result {
                Ok(_) => debug!("✓ Successfully created object with If-None-Match: *"),
                Err(e) => panic!("Expected PUT with If-None-Match: * to succeed when object doesn't exist, but got error: {e:?}"),
            }
        }

        // Verify the object was created
        {
            let result = c.get_object().bucket(bucket).key(key).send().await?;
            let body = result.body.collect().await?.into_bytes();
            assert_eq!(body.as_ref(), content1.as_bytes());
            debug!("✓ Verified object was created");
        }

        // Test 2: PUT with If-None-Match: * should fail when object exists
        debug!("Test 2: PUT with If-None-Match: * on existing object");
        {
            let body = ByteStream::from_static(content2.as_bytes());
            let result = c
                .put_object()
                .bucket(bucket)
                .key(key)
                .body(body)
                .if_none_match("*")
                .send()
                .await;

            match result {
                Ok(_) => panic!("Expected PUT with If-None-Match: * to fail when object exists, but it succeeded"),
                Err(e) => {
                    let error_str = format!("{e:?}");
                    debug!("✓ Expected error when object exists: {error_str}");
                    // The error should be a PreconditionFailed (412)
                    assert!(
                        error_str.contains("PreconditionFailed") || error_str.contains("412"),
                        "Expected PreconditionFailed error, got: {error_str}"
                    );
                }
            }
        }

        // Verify the object wasn't overwritten
        {
            let result = c.get_object().bucket(bucket).key(key).send().await?;
            let body = result.body.collect().await?.into_bytes();
            assert_eq!(body.as_ref(), content1.as_bytes());
            debug!("✓ Verified object was not overwritten");
        }

        // Cleanup
        delete_object(c, bucket, key).await?;
        delete_bucket(c, bucket).await?;

        Ok(())
    }

    /// Test that `PutObject` with `If-Match: *` succeeds when the object exists
    /// and fails with `PreconditionFailed` (412) when the object is absent.
    async fn test_put_object_if_match_wildcard(self: Arc<Self>) -> Result<()> {
        let c = &self.s3;
        let bucket = format!("if-match-wc-{}", Uuid::new_v4());
        let bucket = bucket.as_str();
        let key = "test-file.txt";
        let initial = "initial content";
        let updated = "updated content";

        create_bucket(c, bucket).await?;

        // Test 1: PUT with If-Match: * should fail when the object doesn't exist
        {
            let body = ByteStream::from_static(initial.as_bytes());
            let err = c
                .put_object()
                .bucket(bucket)
                .key(key)
                .body(body)
                .if_match("*")
                .send()
                .await
                .expect_err("Expected If-Match: * on absent object to fail");

            let service_err = err.into_service_error();
            assert_eq!(
                service_err.code(),
                Some("PreconditionFailed"),
                "Expected PreconditionFailed, got: {:?}",
                service_err.code()
            );
        }

        // Seed the object so the next steps have something to match against.
        c.put_object()
            .bucket(bucket)
            .key(key)
            .body(ByteStream::from_static(initial.as_bytes()))
            .send()
            .await?;

        // Test 2: PUT with If-Match: * should succeed when the object exists
        {
            let body = ByteStream::from_static(updated.as_bytes());
            c.put_object()
                .bucket(bucket)
                .key(key)
                .body(body)
                .if_match("*")
                .send()
                .await
                .expect("Expected If-Match: * on existing object to succeed");
        }

        {
            let result = c.get_object().bucket(bucket).key(key).send().await?;
            let body = result.body.collect().await?.into_bytes();
            assert_eq!(body.as_ref(), updated.as_bytes());
        }

        delete_object(c, bucket, key).await?;
        delete_bucket(c, bucket).await?;

        Ok(())
    }

    /// Test that `PutObject` with `If-Match: <etag>` overwrites only when the
    /// stored `ETag` matches and returns `PreconditionFailed` (412) otherwise.
    async fn test_put_object_if_match_etag(self: Arc<Self>) -> Result<()> {
        let c = &self.s3;
        let bucket = format!("if-match-etag-{}", Uuid::new_v4());
        let bucket = bucket.as_str();
        let key = "test-file.txt";
        let initial = "initial content";
        let updated = "updated content";

        create_bucket(c, bucket).await?;

        // Test 1: PUT with If-Match: <etag> should fail when the object doesn't exist
        {
            let body = ByteStream::from_static(initial.as_bytes());
            let err = c
                .put_object()
                .bucket(bucket)
                .key(key)
                .body(body)
                .if_match("\"some-etag\"")
                .send()
                .await
                .expect_err("Expected If-Match on absent object to fail");

            let service_err = err.into_service_error();
            assert_eq!(
                service_err.code(),
                Some("PreconditionFailed"),
                "Expected PreconditionFailed, got: {:?}",
                service_err.code()
            );
        }

        // Seed the object and capture the real ETag.
        let initial_etag = {
            let body = ByteStream::from_static(initial.as_bytes());
            let result = c.put_object().bucket(bucket).key(key).body(body).send().await?;
            result.e_tag().expect("put_object should return e_tag").to_owned()
        };

        // Test 2: PUT with a wrong ETag should fail and not overwrite
        {
            let body = ByteStream::from_static(updated.as_bytes());
            let err = c
                .put_object()
                .bucket(bucket)
                .key(key)
                .body(body)
                .if_match("\"wrong-etag-value\"")
                .send()
                .await
                .expect_err("Expected If-Match with wrong ETag to fail");

            let service_err = err.into_service_error();
            assert_eq!(
                service_err.code(),
                Some("PreconditionFailed"),
                "Expected PreconditionFailed, got: {:?}",
                service_err.code()
            );

            let result = c.get_object().bucket(bucket).key(key).send().await?;
            let body = result.body.collect().await?.into_bytes();
            assert_eq!(body.as_ref(), initial.as_bytes(), "Object should not be overwritten");
        }

        // Test 3: PUT with the matching ETag should succeed and replace the body
        {
            let body = ByteStream::from_static(updated.as_bytes());
            c.put_object()
                .bucket(bucket)
                .key(key)
                .body(body)
                .if_match(&initial_etag)
                .send()
                .await
                .expect("Expected If-Match with matching ETag to succeed");

            let result = c.get_object().bucket(bucket).key(key).send().await?;
            let body = result.body.collect().await?.into_bytes();
            assert_eq!(body.as_ref(), updated.as_bytes());
        }

        delete_object(c, bucket, key).await?;
        delete_bucket(c, bucket).await?;

        Ok(())
    }

    async fn test_put_object_if_match_multipart_etag(self: Arc<Self>) -> Result<()> {
        let c = &self.s3;
        let bucket = format!("if-match-multipart-{}", Uuid::new_v4());
        let bucket = bucket.as_str();
        let key = "multipart-source.txt";
        let initial = b"multipart initial content";
        let updated = b"updated through put object";

        create_bucket(c, bucket).await?;

        let multipart_etag = {
            let (upload_id, upload_parts) = do_multipart_upload(c, bucket, key, initial).await?;
            let upload = CompletedMultipartUpload::builder().set_parts(Some(upload_parts)).build();
            let result = c
                .complete_multipart_upload()
                .bucket(bucket)
                .key(key)
                .upload_id(&upload_id)
                .multipart_upload(upload)
                .send()
                .await?;
            result
                .e_tag()
                .expect("complete_multipart_upload should return e_tag")
                .to_owned()
        };
        assert!(multipart_etag.contains('-'), "expected multipart ETag format");

        let err = c
            .put_object()
            .bucket(bucket)
            .key(key)
            .body(ByteStream::from_static(b"wrong overwrite"))
            .if_match("\"wrong-etag-value\"")
            .send()
            .await
            .expect_err("wrong ETag should fail");
        assert_eq!(err.into_service_error().code(), Some("PreconditionFailed"));

        let result = c.get_object().bucket(bucket).key(key).send().await?;
        let body = result.body.collect().await?.into_bytes();
        assert_eq!(body.as_ref(), initial);

        c.put_object()
            .bucket(bucket)
            .key(key)
            .body(ByteStream::from_static(updated))
            .if_match(&multipart_etag)
            .send()
            .await?;

        let result = c.get_object().bucket(bucket).key(key).send().await?;
        let body = result.body.collect().await?.into_bytes();
        assert_eq!(body.as_ref(), updated);

        delete_object(c, bucket, key).await?;
        delete_bucket(c, bucket).await?;

        Ok(())
    }

    async fn test_put_object_if_match_legacy_md5_fallback(self: Arc<Self>) -> Result<()> {
        let c = &self.s3;
        let bucket = format!("if-match-legacy-{}", Uuid::new_v4());
        let bucket = bucket.as_str();
        let key = "legacy-object.txt";
        let initial = b"legacy initial content";
        let updated = b"legacy updated content";

        create_bucket(c, bucket).await?;

        let initial_etag = {
            let result = c
                .put_object()
                .bucket(bucket)
                .key(key)
                .body(ByteStream::from_static(initial))
                .send()
                .await?;
            result.e_tag().expect("put_object should return e_tag").to_owned()
        };

        let encode = |s: &str| base64_simd::URL_SAFE_NO_PAD.encode_to_string(s);
        let internal_info_path =
            std::path::Path::new(FS_ROOT).join(format!(".bucket-{}.object-{}.internal.json", encode(bucket), encode(key)));
        fs::remove_file(internal_info_path)?;

        let err = c
            .put_object()
            .bucket(bucket)
            .key(key)
            .body(ByteStream::from_static(b"wrong overwrite"))
            .if_match("\"wrong-etag-value\"")
            .send()
            .await
            .expect_err("wrong ETag should fail through MD5 fallback");
        assert_eq!(err.into_service_error().code(), Some("PreconditionFailed"));

        let result = c.get_object().bucket(bucket).key(key).send().await?;
        let body = result.body.collect().await?.into_bytes();
        assert_eq!(body.as_ref(), initial);

        c.put_object()
            .bucket(bucket)
            .key(key)
            .body(ByteStream::from_static(updated))
            .if_match(&initial_etag)
            .send()
            .await?;

        let result = c.get_object().bucket(bucket).key(key).send().await?;
        let body = result.body.collect().await?.into_bytes();
        assert_eq!(body.as_ref(), updated);

        delete_object(c, bucket, key).await?;
        delete_bucket(c, bucket).await?;

        Ok(())
    }

    async fn test_put_object_if_match_rejects_weak_etag(self: Arc<Self>) -> Result<()> {
        let c = &self.s3;
        let bucket = format!("if-match-weak-{}", Uuid::new_v4());
        let bucket = bucket.as_str();
        let key = "weak-etag.txt";
        let initial = b"weak etag initial content";

        create_bucket(c, bucket).await?;

        let initial_etag = {
            let result = c
                .put_object()
                .bucket(bucket)
                .key(key)
                .body(ByteStream::from_static(initial))
                .send()
                .await?;
            result.e_tag().expect("put_object should return e_tag").to_owned()
        };
        let weak_etag = format!("W/{initial_etag}");

        let err = c
            .put_object()
            .bucket(bucket)
            .key(key)
            .body(ByteStream::from_static(b"weak overwrite"))
            .if_match(weak_etag)
            .send()
            .await
            .expect_err("weak ETag must not satisfy If-Match");
        assert_eq!(err.into_service_error().code(), Some("PreconditionFailed"));

        let result = c.get_object().bucket(bucket).key(key).send().await?;
        let body = result.body.collect().await?.into_bytes();
        assert_eq!(body.as_ref(), initial);

        delete_object(c, bucket, key).await?;
        delete_bucket(c, bucket).await?;

        Ok(())
    }
}
