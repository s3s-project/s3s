// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

use crate::case;
use crate::suite::{Copy, create_bucket, delete_bucket, delete_object};

use std::sync::Arc;

use aws_sdk_s3::primitives::ByteStream;
use aws_sdk_s3::types::CompletedMultipartUpload;
use aws_sdk_s3::types::CompletedPart;

use aws_sdk_s3::error::ProvideErrorMetadata;

use s3s_test::Result;
use s3s_test::tcx::TestContext;
use uuid::Uuid;

pub fn register(tcx: &mut TestContext) {
    case!(tcx, FsServer, Copy, test_copy_object_nested_dst);
    case!(tcx, FsServer, Copy, test_copy_object_self_replace_preserves_content);
    case!(tcx, FsServer, Copy, test_copy_object_metadata_directive_replace);
    case!(tcx, FsServer, Copy, test_copy_object_metadata_directive_default_copies_source);
    case!(tcx, FsServer, Copy, test_copy_object_metadata_directive_copy_ignores_request_fields);
    case!(tcx, FsServer, Copy, test_copy_object_if_match);
    case!(tcx, FsServer, Copy, test_copy_object_if_none_match);
    case!(tcx, FsServer, Copy, test_copy_object_if_modified_since);
    case!(tcx, FsServer, Copy, test_copy_object_conditional_with_multipart_source_etag);
}

impl Copy {
    /// Regression test for <https://github.com/s3s-project/s3s/issues/67>
    ///
    /// `copy_object` should create parent directories when the destination key contains "/"
    async fn test_copy_object_nested_dst(self: Arc<Self>) -> Result<()> {
        let c = &self.s3;
        let bucket = format!("test-copy-nested-{}", Uuid::new_v4());
        let bucket = bucket.as_str();

        create_bucket(c, bucket).await?;

        // Put a file at the root level
        let src_key = "source.txt";
        let content = "copy me into a nested directory";
        c.put_object()
            .bucket(bucket)
            .key(src_key)
            .body(ByteStream::from_static(content.as_bytes()))
            .send()
            .await?;

        // Copy to a nested destination with multiple levels of "/"
        let dst_key = "deep/nested/path/destination.txt";
        let copy_source = format!("{bucket}/{src_key}");
        c.copy_object()
            .bucket(bucket)
            .key(dst_key)
            .copy_source(copy_source)
            .send()
            .await?;

        // Verify the copied file exists and has the correct content
        let ans = c.get_object().bucket(bucket).key(dst_key).send().await?;
        let body = ans.body.collect().await?.into_bytes();
        assert_eq!(body.as_ref(), content.as_bytes());

        // Cleanup
        delete_object(c, bucket, src_key).await?;
        delete_object(c, bucket, dst_key).await?;
        delete_bucket(c, bucket).await?;

        Ok(())
    }

    /// Regression test: `CopyObject` with `src == dst` (self-replace) must
    /// preserve the on-disk content. AWS S3 supports this shape as the
    /// canonical way to update an object's metadata in place. Before the
    /// fix, `tokio::fs::copy(src, dst)` opened the destination with
    /// `O_TRUNC` before reading the source, zeroing the file.
    async fn test_copy_object_self_replace_preserves_content(self: Arc<Self>) -> Result<()> {
        let c = &self.s3;
        let bucket = format!("test-self-replace-{}", Uuid::new_v4());
        let bucket = bucket.as_str();

        create_bucket(c, bucket).await?;

        let key = "obj.bin";
        let content = "original content that must survive a self-replace";
        c.put_object()
            .bucket(bucket)
            .key(key)
            .body(ByteStream::from_static(content.as_bytes()))
            .send()
            .await?;

        let before_head = c.head_object().bucket(bucket).key(key).send().await?;
        let before_last_modified = before_head
            .last_modified()
            .expect("head_object should return last_modified")
            .to_owned();

        tokio::time::sleep(std::time::Duration::from_secs(1)).await;

        let copy_source = format!("{bucket}/{key}");
        c.copy_object()
            .bucket(bucket)
            .key(key)
            .copy_source(&copy_source)
            .send()
            .await?;

        let after_head = c.head_object().bucket(bucket).key(key).send().await?;
        let after_last_modified = after_head
            .last_modified()
            .expect("head_object should return last_modified")
            .to_owned();
        assert!(
            after_last_modified > before_last_modified,
            "CopyObject self-replace must update LastModified"
        );

        let got = c.get_object().bucket(bucket).key(key).send().await?;
        let body = got.body.collect().await?.into_bytes();
        assert_eq!(body.as_ref(), content.as_bytes(), "CopyObject self-replace must not zero the file");

        delete_object(c, bucket, key).await?;
        delete_bucket(c, bucket).await?;

        Ok(())
    }

    /// `MetadataDirective: REPLACE` must drop the source metadata and
    /// install the request's metadata + `content_type` on the destination.
    /// Before the fix, `copy_object` ignored both `metadata_directive`
    /// and `metadata` and unconditionally copied the source sidecar
    /// verbatim.
    async fn test_copy_object_metadata_directive_replace(self: Arc<Self>) -> Result<()> {
        use aws_sdk_s3::types::MetadataDirective;

        let c = &self.s3;
        let bucket = format!("test-meta-replace-{}", Uuid::new_v4());
        let bucket = bucket.as_str();

        create_bucket(c, bucket).await?;

        let src_key = "src.bin";
        c.put_object()
            .bucket(bucket)
            .key(src_key)
            .body(ByteStream::from_static(b"x"))
            .content_type("application/octet-stream")
            .metadata("origin", "v1")
            .metadata("rev", "1")
            .metadata("source-only", "keep-out")
            .send()
            .await?;

        let dst_key = "dst.bin";
        let copy_source = format!("{bucket}/{src_key}");
        c.copy_object()
            .bucket(bucket)
            .key(dst_key)
            .copy_source(&copy_source)
            .metadata_directive(MetadataDirective::Replace)
            .content_type("application/pdf")
            .metadata("origin", "v2")
            .metadata("rev", "2")
            .send()
            .await?;

        let head = c.head_object().bucket(bucket).key(dst_key).send().await?;
        assert_eq!(
            head.content_type().unwrap_or(""),
            "application/pdf",
            "REPLACE must install the request's content_type on the destination"
        );
        let dst_meta = head.metadata().cloned().unwrap_or_default();
        assert_eq!(dst_meta.get("origin").map(String::as_str), Some("v2"));
        assert_eq!(dst_meta.get("rev").map(String::as_str), Some("2"));
        assert_eq!(
            dst_meta.get("source-only"),
            None,
            "REPLACE must drop metadata that only exists on the source object"
        );

        delete_object(c, bucket, src_key).await?;
        delete_object(c, bucket, dst_key).await?;
        delete_bucket(c, bucket).await?;

        Ok(())
    }

    /// Omitting `MetadataDirective` must use S3's default `COPY` behavior:
    /// propagate source metadata and ignore replacement fields from the request.
    async fn test_copy_object_metadata_directive_default_copies_source(self: Arc<Self>) -> Result<()> {
        let c = &self.s3;
        let bucket = format!("test-meta-default-copy-{}", Uuid::new_v4());
        let bucket = bucket.as_str();

        create_bucket(c, bucket).await?;

        let src_key = "src.bin";
        c.put_object()
            .bucket(bucket)
            .key(src_key)
            .body(ByteStream::from_static(b"x"))
            .content_type("application/octet-stream")
            .metadata("origin", "v1")
            .send()
            .await?;

        let dst_key = "dst.bin";
        let copy_source = format!("{bucket}/{src_key}");
        c.copy_object()
            .bucket(bucket)
            .key(dst_key)
            .copy_source(&copy_source)
            .content_type("application/pdf")
            .metadata("origin", "v2")
            .send()
            .await?;

        let head = c.head_object().bucket(bucket).key(dst_key).send().await?;
        assert_eq!(
            head.content_type().unwrap_or(""),
            "application/octet-stream",
            "default MetadataDirective must propagate the source content_type"
        );
        let dst_meta = head.metadata().cloned().unwrap_or_default();
        assert_eq!(
            dst_meta.get("origin").map(String::as_str),
            Some("v1"),
            "default MetadataDirective must propagate the source metadata"
        );

        delete_object(c, bucket, src_key).await?;
        delete_object(c, bucket, dst_key).await?;
        delete_bucket(c, bucket).await?;

        Ok(())
    }

    async fn test_copy_object_metadata_directive_copy_ignores_request_fields(self: Arc<Self>) -> Result<()> {
        use aws_sdk_s3::types::MetadataDirective;

        let c = &self.s3;
        let bucket = format!("test-meta-copy-{}", Uuid::new_v4());
        let bucket = bucket.as_str();

        create_bucket(c, bucket).await?;

        let src_key = "src.bin";
        c.put_object()
            .bucket(bucket)
            .key(src_key)
            .body(ByteStream::from_static(b"x"))
            .content_type("application/octet-stream")
            .metadata("origin", "v1")
            .send()
            .await?;

        let dst_key = "dst.bin";
        let copy_source = format!("{bucket}/{src_key}");
        c.copy_object()
            .bucket(bucket)
            .key(dst_key)
            .copy_source(&copy_source)
            .metadata_directive(MetadataDirective::Copy)
            .content_type("application/pdf") // expected to be ignored under COPY
            .metadata("origin", "v2")
            .send()
            .await?;

        let head = c.head_object().bucket(bucket).key(dst_key).send().await?;
        assert_eq!(
            head.content_type().unwrap_or(""),
            "application/octet-stream",
            "COPY must propagate the source content_type and ignore the request override"
        );
        let dst_meta = head.metadata().cloned().unwrap_or_default();
        assert_eq!(
            dst_meta.get("origin").map(String::as_str),
            Some("v1"),
            "COPY must propagate the source metadata, not the request fields"
        );

        delete_object(c, bucket, src_key).await?;
        delete_object(c, bucket, dst_key).await?;
        delete_bucket(c, bucket).await?;

        Ok(())
    }

    /// Test conditional copy with `x-amz-copy-source-if-match`.
    async fn test_copy_object_if_match(self: Arc<Self>) -> Result<()> {
        use aws_sdk_s3::primitives::DateTime;
        use aws_sdk_s3::primitives::DateTimeFormat;

        let c = &self.s3;
        let bucket = format!("test-cond-copy-{}", Uuid::new_v4());
        let bucket = bucket.as_str();
        create_bucket(c, bucket).await?;

        let src_key = "source.txt";
        let content = "conditional copy content";
        c.put_object()
            .bucket(bucket)
            .key(src_key)
            .body(ByteStream::from_static(content.as_bytes()))
            .send()
            .await?;

        let get_result = c.get_object().bucket(bucket).key(src_key).send().await?;
        let e_tag = get_result.e_tag().expect("get_object should return e_tag").to_owned();
        let _ = get_result.body.collect().await?;

        let copy_source = format!("{bucket}/{src_key}");

        let dst_key = "dest-match.txt";
        c.copy_object()
            .bucket(bucket)
            .key(dst_key)
            .copy_source(&copy_source)
            .copy_source_if_match(&e_tag)
            .send()
            .await?;

        let ans = c.get_object().bucket(bucket).key(dst_key).send().await?;
        let body = ans.body.collect().await?.into_bytes();
        assert_eq!(body.as_ref(), content.as_bytes());

        let dst_key2 = "dest-match-wildcard.txt";
        let past = DateTime::from_str("Thu, 01 Jan 2000 00:00:00 GMT", DateTimeFormat::HttpDate)?;
        c.copy_object()
            .bucket(bucket)
            .key(dst_key2)
            .copy_source(&copy_source)
            .copy_source_if_match("*")
            .copy_source_if_unmodified_since(past)
            .send()
            .await?;

        let ans = c.get_object().bucket(bucket).key(dst_key2).send().await?;
        let body = ans.body.collect().await?.into_bytes();
        assert_eq!(body.as_ref(), content.as_bytes());

        let dst_key3 = "dest-nomatch.txt";
        let err = c
            .copy_object()
            .bucket(bucket)
            .key(dst_key3)
            .copy_source(&copy_source)
            .copy_source_if_match("\"nonexistent-etag\"")
            .send()
            .await
            .expect_err("Expected copy with non-matching If-Match to fail");
        let service_err = err.into_service_error();
        assert_eq!(service_err.code(), Some("PreconditionFailed"));

        delete_object(c, bucket, src_key).await?;
        delete_object(c, bucket, dst_key).await?;
        delete_object(c, bucket, dst_key2).await?;
        delete_bucket(c, bucket).await?;

        Ok(())
    }

    /// Test conditional copy with `x-amz-copy-source-if-none-match`.
    async fn test_copy_object_if_none_match(self: Arc<Self>) -> Result<()> {
        use aws_sdk_s3::primitives::DateTime;
        use aws_sdk_s3::primitives::DateTimeFormat;

        let c = &self.s3;
        let bucket = format!("test-cond-copy-nm-{}", Uuid::new_v4());
        let bucket = bucket.as_str();
        create_bucket(c, bucket).await?;

        let src_key = "source.txt";
        let content = "conditional copy none match";
        c.put_object()
            .bucket(bucket)
            .key(src_key)
            .body(ByteStream::from_static(content.as_bytes()))
            .send()
            .await?;

        let get_result = c.get_object().bucket(bucket).key(src_key).send().await?;
        let e_tag = get_result.e_tag().expect("get_object should return e_tag").to_owned();
        let _ = get_result.body.collect().await?;

        let copy_source = format!("{bucket}/{src_key}");

        let dst_key = "dest-none-match-ok.txt";
        c.copy_object()
            .bucket(bucket)
            .key(dst_key)
            .copy_source(&copy_source)
            .copy_source_if_none_match("\"different-etag\"")
            .send()
            .await?;

        let ans = c.get_object().bucket(bucket).key(dst_key).send().await?;
        let body = ans.body.collect().await?.into_bytes();
        assert_eq!(body.as_ref(), content.as_bytes());

        let dst_key2 = "dest-none-match-fail.txt";
        let err = c
            .copy_object()
            .bucket(bucket)
            .key(dst_key2)
            .copy_source(&copy_source)
            .copy_source_if_none_match(&e_tag)
            .send()
            .await
            .expect_err("Expected copy with matching If-None-Match to fail");
        let service_err = err.into_service_error();
        assert_eq!(service_err.code(), Some("PreconditionFailed"));

        let dst_key3 = "dest-none-match-wildcard.txt";
        let err = c
            .copy_object()
            .bucket(bucket)
            .key(dst_key3)
            .copy_source(&copy_source)
            .copy_source_if_none_match("*")
            .send()
            .await
            .expect_err("Expected copy with wildcard If-None-Match to fail for existing source");
        let service_err = err.into_service_error();
        assert_eq!(service_err.code(), Some("PreconditionFailed"));

        let dst_key4 = "dest-none-match-precedence.txt";
        let future = DateTime::from_str("Thu, 01 Jan 2099 00:00:00 GMT", DateTimeFormat::HttpDate)?;
        c.copy_object()
            .bucket(bucket)
            .key(dst_key4)
            .copy_source(&copy_source)
            .copy_source_if_none_match("\"different-etag\"")
            .copy_source_if_modified_since(future)
            .send()
            .await?;

        let ans = c.get_object().bucket(bucket).key(dst_key4).send().await?;
        let body = ans.body.collect().await?.into_bytes();
        assert_eq!(body.as_ref(), content.as_bytes());

        delete_object(c, bucket, src_key).await?;
        delete_object(c, bucket, dst_key).await?;
        delete_object(c, bucket, dst_key4).await?;
        delete_bucket(c, bucket).await?;

        Ok(())
    }

    /// Test conditional copy with `x-amz-copy-source-if-modified-since` and
    /// `x-amz-copy-source-if-unmodified-since`.
    async fn test_copy_object_if_modified_since(self: Arc<Self>) -> Result<()> {
        use aws_sdk_s3::primitives::DateTime;
        use aws_sdk_s3::primitives::DateTimeFormat;

        let c = &self.s3;
        let bucket = format!("test-cond-copy-ts-{}", Uuid::new_v4());
        let bucket = bucket.as_str();
        create_bucket(c, bucket).await?;

        let src_key = "source.txt";
        let content = "conditional copy timestamp";
        c.put_object()
            .bucket(bucket)
            .key(src_key)
            .body(ByteStream::from_static(content.as_bytes()))
            .send()
            .await?;

        let copy_source = format!("{bucket}/{src_key}");

        let dst_key = "dest-modified-ok.txt";
        let past = DateTime::from_str("Thu, 01 Jan 2000 00:00:00 GMT", DateTimeFormat::HttpDate)?;
        c.copy_object()
            .bucket(bucket)
            .key(dst_key)
            .copy_source(&copy_source)
            .copy_source_if_modified_since(past)
            .send()
            .await?;

        let ans = c.get_object().bucket(bucket).key(dst_key).send().await?;
        let body = ans.body.collect().await?.into_bytes();
        assert_eq!(body.as_ref(), content.as_bytes());

        let dst_key2 = "dest-modified-fail.txt";
        let future = DateTime::from_str("Thu, 01 Jan 2099 00:00:00 GMT", DateTimeFormat::HttpDate)?;
        let err = c
            .copy_object()
            .bucket(bucket)
            .key(dst_key2)
            .copy_source(&copy_source)
            .copy_source_if_modified_since(future)
            .send()
            .await
            .expect_err("Expected copy with future if-modified-since to fail");
        let service_err = err.into_service_error();
        assert_eq!(service_err.code(), Some("PreconditionFailed"));

        let dst_key3 = "dest-unmodified-ok.txt";
        let future = DateTime::from_str("Thu, 01 Jan 2099 00:00:00 GMT", DateTimeFormat::HttpDate)?;
        c.copy_object()
            .bucket(bucket)
            .key(dst_key3)
            .copy_source(&copy_source)
            .copy_source_if_unmodified_since(future)
            .send()
            .await?;

        let ans = c.get_object().bucket(bucket).key(dst_key3).send().await?;
        let body = ans.body.collect().await?.into_bytes();
        assert_eq!(body.as_ref(), content.as_bytes());

        let dst_key4 = "dest-unmodified-fail.txt";
        let past = DateTime::from_str("Thu, 01 Jan 2000 00:00:00 GMT", DateTimeFormat::HttpDate)?;
        let err = c
            .copy_object()
            .bucket(bucket)
            .key(dst_key4)
            .copy_source(&copy_source)
            .copy_source_if_unmodified_since(past)
            .send()
            .await
            .expect_err("Expected copy with past if-unmodified-since to fail");
        let service_err = err.into_service_error();
        assert_eq!(service_err.code(), Some("PreconditionFailed"));

        delete_object(c, bucket, src_key).await?;
        delete_object(c, bucket, dst_key).await?;
        delete_object(c, bucket, dst_key3).await?;
        delete_bucket(c, bucket).await?;

        Ok(())
    }

    /// Test conditional copy against a multipart source object's persisted `ETag`.
    async fn test_copy_object_conditional_with_multipart_source_etag(self: Arc<Self>) -> Result<()> {
        let c = &self.s3;
        let bucket = format!("test-cond-copy-multipart-{}", Uuid::new_v4());
        let bucket = bucket.as_str();
        create_bucket(c, bucket).await?;

        let src_key = "source-multipart.txt";
        let content = "multipart conditional copy content";

        let upload_id = c
            .create_multipart_upload()
            .bucket(bucket)
            .key(src_key)
            .send()
            .await?
            .upload_id
            .expect("create_multipart_upload should return upload_id");

        let upload_result = c
            .upload_part()
            .bucket(bucket)
            .key(src_key)
            .upload_id(&upload_id)
            .body(ByteStream::from_static(content.as_bytes()))
            .part_number(1)
            .send()
            .await?;

        let upload = CompletedMultipartUpload::builder()
            .set_parts(Some(vec![
                CompletedPart::builder()
                    .e_tag(upload_result.e_tag.expect("upload_part should return e_tag"))
                    .part_number(1)
                    .build(),
            ]))
            .build();

        let complete_result = c
            .complete_multipart_upload()
            .bucket(bucket)
            .key(src_key)
            .multipart_upload(upload)
            .upload_id(&upload_id)
            .send()
            .await?;
        let multipart_e_tag = complete_result
            .e_tag()
            .expect("complete_multipart_upload should return e_tag")
            .to_owned();

        let copy_source = format!("{bucket}/{src_key}");

        let dst_key = "dest-multipart-match.txt";
        c.copy_object()
            .bucket(bucket)
            .key(dst_key)
            .copy_source(&copy_source)
            .copy_source_if_match(&multipart_e_tag)
            .send()
            .await?;

        let ans = c.get_object().bucket(bucket).key(dst_key).send().await?;
        let body = ans.body.collect().await?.into_bytes();
        assert_eq!(body.as_ref(), content.as_bytes());

        // The destination ETag should match the source multipart ETag (format preserved during copy).
        let head = c.head_object().bucket(bucket).key(dst_key).send().await?;
        let dst_etag = head.e_tag().expect("head_object should return e_tag");
        assert_eq!(
            dst_etag, multipart_e_tag,
            "destination ETag should match source multipart ETag after copy"
        );

        let dst_key2 = "dest-multipart-none-match.txt";
        let err = c
            .copy_object()
            .bucket(bucket)
            .key(dst_key2)
            .copy_source(&copy_source)
            .copy_source_if_none_match(&multipart_e_tag)
            .send()
            .await
            .expect_err("Expected matching multipart ETag to fail If-None-Match");
        let service_err = err.into_service_error();
        assert_eq!(service_err.code(), Some("PreconditionFailed"));

        delete_object(c, bucket, src_key).await?;
        delete_object(c, bucket, dst_key).await?;
        delete_bucket(c, bucket).await?;

        Ok(())
    }
}
