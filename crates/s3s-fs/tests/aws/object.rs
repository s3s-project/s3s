// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

use crate::case;
use crate::suite::{FS_ROOT, Object, create_bucket, delete_bucket, delete_object};

use std::sync::Arc;

use std::fs;

use aws_sdk_s3::primitives::ByteStream;
use aws_sdk_s3::types::ChecksumMode;

use aws_sdk_s3::error::ProvideErrorMetadata;

use s3s_test::Result;
use s3s_test::tcx::TestContext;
use tracing::debug;
use uuid::Uuid;

pub fn register(tcx: &mut TestContext) {
    case!(tcx, FsServer, Object, test_single_object);
    case!(tcx, FsServer, Object, test_single_object_get_range);
    case!(tcx, FsServer, Object, test_content_encoding_preservation);
    case!(tcx, FsServer, Object, test_put_object_atomic_write);
    case!(tcx, FsServer, Object, test_head_object_no_such_key);
    case!(tcx, FsServer, Object, test_head_object_directory_prefix_returns_no_such_key);
    case!(tcx, FsServer, Object, test_head_object_no_such_bucket);
    case!(tcx, FsServer, Object, test_head_object_etag_and_checksum);
}

impl Object {
    async fn test_single_object(self: Arc<Self>) -> Result<()> {
        let c = &self.s3;
        let bucket = format!("test-single-object-{}", Uuid::new_v4());
        let bucket = bucket.as_str();
        let key = "sample.txt";
        let content = "hello world\n你好世界\n";
        let crc32c = base64_simd::STANDARD.encode_to_string(crc32c::crc32c(content.as_bytes()).to_be_bytes());

        create_bucket(c, bucket).await?;

        {
            let body = ByteStream::from_static(content.as_bytes());
            c.put_object()
                .bucket(bucket)
                .key(key)
                .body(body)
                .checksum_crc32_c(crc32c.as_str())
                .send()
                .await?;
        }

        {
            let ans = c
                .get_object()
                .bucket(bucket)
                .key(key)
                .checksum_mode(ChecksumMode::Enabled)
                .send()
                .await?;

            let content_length: usize = ans.content_length().unwrap().try_into().unwrap();
            let checksum_crc32c = ans.checksum_crc32_c.unwrap();
            let body = ans.body.collect().await?.into_bytes();

            assert_eq!(content_length, content.len());
            assert_eq!(checksum_crc32c, crc32c);
            assert_eq!(body.as_ref(), content.as_bytes());
        }

        {
            delete_object(c, bucket, key).await?;
            delete_bucket(c, bucket).await?;
        }

        Ok(())
    }

    async fn test_single_object_get_range(self: Arc<Self>) -> Result<()> {
        let c = &self.s3;
        let bucket = format!("test-single-object-{}", Uuid::new_v4());
        let bucket = bucket.as_str();
        let key = "sample.txt";
        let content = "hello world\n你好世界\n";
        let crc32c = base64_simd::STANDARD.encode_to_string(crc32c::crc32c(content.as_bytes()).to_be_bytes());

        create_bucket(c, bucket).await?;

        {
            let body = ByteStream::from_static(content.as_bytes());
            c.put_object()
                .bucket(bucket)
                .key(key)
                .body(body)
                .checksum_crc32_c(crc32c.as_str())
                .send()
                .await?;
        }

        {
            let ans = c
                .get_object()
                .bucket(bucket)
                .key(key)
                .range("bytes=0-4")
                .checksum_mode(ChecksumMode::Enabled)
                .send()
                .await?;

            // S3 doesn't return checksums when a range is specified
            assert!(&ans.checksum_crc32().is_none());
            assert!(&ans.checksum_crc32_c().is_none());

            let content_length: usize = ans.content_length().unwrap().try_into().unwrap();
            let body = ans.body.collect().await?.into_bytes();

            assert_eq!(content_length, 5);
            assert_eq!(body.as_ref(), &content.as_bytes()[0..=4]);
        }

        {
            let ans = c
                .get_object()
                .bucket(bucket)
                .key(key)
                .range("bytes=0-1000")
                .checksum_mode(ChecksumMode::Enabled)
                .send()
                .await?;

            let content_length: usize = ans.content_length().unwrap().try_into().unwrap();
            let checksum_crc32c = ans.checksum_crc32_c.unwrap();
            let body = ans.body.collect().await?.into_bytes();

            assert_eq!(content_length, content.len());
            assert_eq!(checksum_crc32c, crc32c);
            assert_eq!(body.as_ref(), content.as_bytes());
        }

        {
            delete_object(c, bucket, key).await?;
            delete_bucket(c, bucket).await?;
        }

        Ok(())
    }

    /// Test that demonstrates the Content-Encoding preservation issue
    /// Related: <https://github.com/rustfs/rustfs/issues/1062>
    async fn test_content_encoding_preservation(self: Arc<Self>) -> Result<()> {
        let c = &self.s3;
        let bucket = format!("test-content-encoding-{}", Uuid::new_v4());
        let bucket = bucket.as_str();
        let key = "compressed.json";

        // Simulated Brotli-compressed JSON content
        let content = b"compressed data here";

        create_bucket(c, bucket).await?;

        // Upload object with Content-Encoding header
        {
            let body = ByteStream::from_static(content);
            c.put_object()
                .bucket(bucket)
                .key(key)
                .body(body)
                .content_encoding("br") // Brotli compression
                .content_type("application/json")
                .content_disposition("attachment; filename=\"data.json\"")
                .cache_control("max-age=3600")
                .send()
                .await?;

            debug!("Uploaded object with Content-Encoding: br");
        }

        // Retrieve object and verify headers are preserved
        {
            let ans = c.get_object().bucket(bucket).key(key).send().await?;

            // Verify that standard object attributes are now preserved by s3s-fs
            debug!("Retrieved object:");
            debug!("  Content-Encoding: {:?}", ans.content_encoding());
            debug!("  Content-Type: {:?}", ans.content_type());
            debug!("  Content-Disposition: {:?}", ans.content_disposition());
            debug!("  Cache-Control: {:?}", ans.cache_control());

            // All standard attributes should be preserved
            assert_eq!(ans.content_encoding(), Some("br"));
            assert_eq!(ans.content_type(), Some("application/json"));
            assert_eq!(ans.content_disposition(), Some("attachment; filename=\"data.json\""));
            assert_eq!(ans.cache_control(), Some("max-age=3600"));
        }

        // Also test HeadObject
        {
            let ans = c.head_object().bucket(bucket).key(key).send().await?;

            debug!("HeadObject result:");
            debug!("  Content-Encoding: {:?}", ans.content_encoding());
            debug!("  Content-Type: {:?}", ans.content_type());

            // Verify HeadObject also returns the stored attributes
            assert_eq!(ans.content_encoding(), Some("br"));
            assert_eq!(ans.content_type(), Some("application/json"));
        }

        {
            delete_object(c, bucket, key).await?;
            delete_bucket(c, bucket).await?;
        }

        Ok(())
    }

    /// Regression test for <https://github.com/s3s-project/s3s/issues/116>
    ///
    /// `put_object` should write atomically via a temp file to prevent incomplete writes.
    /// Verify that the file is fully written and readable after `put_object` completes.
    async fn test_put_object_atomic_write(self: Arc<Self>) -> Result<()> {
        let c = &self.s3;
        let bucket = format!("test-atomic-write-{}", Uuid::new_v4());
        let bucket = bucket.as_str();
        create_bucket(c, bucket).await?;

        // Write a reasonably sized object
        let content = "x".repeat(1024 * 64); // 64 KB
        let key = "atomic-test.bin";

        c.put_object()
            .bucket(bucket)
            .key(key)
            .body(ByteStream::from(content.clone().into_bytes()))
            .send()
            .await?;

        // Read it back immediately and verify full content
        let ans = c.get_object().bucket(bucket).key(key).send().await?;
        let body = ans.body.collect().await?.into_bytes();
        assert_eq!(body.len(), content.len(), "Content length mismatch");
        assert_eq!(body.as_ref(), content.as_bytes(), "Content mismatch");

        // Verify no temp files remain in the FS root
        let entries: Vec<_> = fs::read_dir(FS_ROOT)?
            .filter_map(Result::ok)
            .filter(|e| {
                let name = e.file_name();
                let name = name.to_str().unwrap_or("");
                name.starts_with(".tmp.") && name.ends_with(".internal.part")
            })
            .collect();
        assert!(entries.is_empty(), "Leftover temp files found: {entries:?}");

        // Cleanup
        delete_object(c, bucket, key).await?;
        delete_bucket(c, bucket).await?;

        Ok(())
    }

    async fn test_head_object_no_such_key(self: Arc<Self>) -> Result<()> {
        let c = &self.s3;
        let bucket = format!("test-head-no-such-key-{}", Uuid::new_v4());
        let bucket = bucket.as_str();
        create_bucket(c, bucket).await?;

        let result = c.head_object().bucket(bucket).key("nonexistent-object").send().await;
        let err = result.expect_err("Expected NoSuchKey for missing object");
        let service_err = err.into_service_error();
        assert_eq!(service_err.code(), Some("NoSuchKey"), "Expected NoSuchKey, got: {:?}", service_err.code());

        delete_bucket(c, bucket).await?;
        Ok(())
    }

    async fn test_head_object_directory_prefix_returns_no_such_key(self: Arc<Self>) -> Result<()> {
        let c = &self.s3;
        let bucket = format!("test-head-directory-prefix-{}", Uuid::new_v4());
        let bucket = bucket.as_str();
        let key = "prefix/object.txt";
        create_bucket(c, bucket).await?;
        c.put_object()
            .bucket(bucket)
            .key(key)
            .body(ByteStream::from_static(b"content"))
            .send()
            .await?;

        let result = c.head_object().bucket(bucket).key("prefix").send().await;
        let err = result.expect_err("Expected NoSuchKey for a directory-like prefix");
        let service_err = err.into_service_error();
        assert_eq!(service_err.code(), Some("NoSuchKey"), "Expected NoSuchKey, got: {:?}", service_err.code());

        delete_object(c, bucket, key).await?;
        delete_bucket(c, bucket).await?;
        Ok(())
    }

    async fn test_head_object_no_such_bucket(self: Arc<Self>) -> Result<()> {
        let c = &self.s3;
        let bucket = format!("test-head-no-such-bucket-{}", Uuid::new_v4());
        let bucket = bucket.as_str();

        let result = c.head_object().bucket(bucket).key("some-key").send().await;
        let err = result.expect_err("Expected NoSuchBucket for missing bucket");
        let service_err = err.into_service_error();
        assert_eq!(
            service_err.code(),
            Some("NoSuchBucket"),
            "Expected NoSuchBucket, got: {:?}",
            service_err.code()
        );

        Ok(())
    }

    async fn test_head_object_etag_and_checksum(self: Arc<Self>) -> Result<()> {
        let c = &self.s3;
        let bucket = format!("test-head-etag-{}", Uuid::new_v4());
        let bucket = bucket.as_str();
        let key = "sample.txt";
        let content = "hello world\n";
        let crc32c = base64_simd::STANDARD.encode_to_string(crc32c::crc32c(content.as_bytes()).to_be_bytes());

        create_bucket(c, bucket).await?;

        // Put object with checksum
        let put_result = c
            .put_object()
            .bucket(bucket)
            .key(key)
            .body(ByteStream::from_static(content.as_bytes()))
            .checksum_crc32_c(crc32c.as_str())
            .send()
            .await?;
        let put_e_tag = put_result.e_tag().unwrap().to_owned();

        // Head object and verify e_tag is present and matches put_object
        let head_result = c
            .head_object()
            .bucket(bucket)
            .key(key)
            .checksum_mode(ChecksumMode::Enabled)
            .send()
            .await?;
        let head_e_tag = head_result.e_tag().expect("head_object should return e_tag").to_owned();
        assert_eq!(head_e_tag, put_e_tag, "head_object e_tag should match put_object e_tag");

        // Verify checksum is returned
        let head_crc32c = head_result
            .checksum_crc32_c()
            .expect("head_object should return checksum_crc32c");
        assert_eq!(head_crc32c, crc32c);

        // Get object and verify e_tag matches
        let get_result = c
            .get_object()
            .bucket(bucket)
            .key(key)
            .checksum_mode(ChecksumMode::Enabled)
            .send()
            .await?;
        let get_e_tag = get_result.e_tag().expect("get_object should return e_tag").to_owned();
        assert_eq!(head_e_tag, get_e_tag, "head_object e_tag should match get_object e_tag");

        // Cleanup
        delete_object(c, bucket, key).await?;
        delete_bucket(c, bucket).await?;

        Ok(())
    }
}
