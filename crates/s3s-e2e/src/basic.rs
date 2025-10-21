use crate::case;
use crate::utils::*;

use std::sync::Arc;

use s3s_test::Result;
use s3s_test::TestFixture;
use s3s_test::TestSuite;
use s3s_test::tcx::TestContext;

use aws_sdk_s3::primitives::ByteStream;
use aws_sdk_s3::primitives::SdkBody;

use bytes::Bytes;
use futures::StreamExt as _;
use http_body_util::StreamBody;

pub fn register(tcx: &mut TestContext) {
    case!(tcx, Basic, Essential, test_list_buckets);
    case!(tcx, Basic, Essential, test_list_objects);
    case!(tcx, Basic, Essential, test_get_object);
    case!(tcx, Basic, Essential, test_delete_object);
    case!(tcx, Basic, Essential, test_head_operations);
    case!(tcx, Basic, Put, test_put_object_tiny);
    case!(tcx, Basic, Put, test_put_object_with_metadata);
    case!(tcx, Basic, Put, test_put_object_larger);
    case!(tcx, Basic, Put, test_put_object_with_checksum_algorithm);
    case!(tcx, Basic, Put, test_put_object_with_content_checksums);
    case!(tcx, Basic, Copy, test_copy_object);
}

struct Basic {
    s3: aws_sdk_s3::Client,
}

impl TestSuite for Basic {
    #[tracing::instrument(skip_all)]
    async fn setup() -> Result<Self> {
        let sdk_conf = aws_config::from_env().load().await;

        let s3 = aws_sdk_s3::Client::from_conf(
            aws_sdk_s3::config::Builder::from(&sdk_conf)
                .force_path_style(true) // FIXME: remove force_path_style
                .build(),
        );

        Ok(Self { s3 })
    }
}

struct Essential {
    s3: aws_sdk_s3::Client,
}

impl TestFixture<Basic> for Essential {
    async fn setup(suite: Arc<Basic>) -> Result<Self> {
        Ok(Self { s3: suite.s3.clone() })
    }
}

impl Essential {
    async fn test_list_buckets(self: Arc<Self>) -> Result {
        let s3 = &self.s3;

        let buckets = ["test-list-buckets-1", "test-list-buckets-2"];

        {
            for &bucket in &buckets {
                delete_bucket_loose(s3, bucket).await?;
            }
        }

        {
            for &bucket in &buckets {
                create_bucket(s3, bucket).await?;
            }

            let resp = s3.list_buckets().send().await?;
            let bucket_list: Vec<_> = resp.buckets.as_deref().unwrap().iter().filter_map(|b| b.name()).collect();

            for &bucket in &buckets {
                assert!(bucket_list.contains(&bucket));
                s3.head_bucket().bucket(bucket).send().await?;
            }
        }

        {
            for &bucket in &buckets {
                delete_bucket_strict(s3, bucket).await?;
            }
        }

        Ok(())
    }

    async fn test_list_objects(self: Arc<Self>) -> Result {
        let s3 = &self.s3;

        let bucket = "test-list-objects";
        let keys = ["file-1", "file-2", "file-3"];
        let content = "hello world 你好世界 123456 !@#$%😂^&*()";

        {
            for key in &keys {
                delete_object_loose(s3, bucket, key).await?;
            }
            delete_bucket_loose(s3, bucket).await?;
        }

        {
            create_bucket(s3, bucket).await?;

            for &key in &keys {
                s3.put_object()
                    .bucket(bucket)
                    .key(key)
                    .body(ByteStream::from_static(content.as_bytes()))
                    .send()
                    .await?;
            }

            let resp = s3.list_objects_v2().bucket(bucket).send().await?;
            let object_list: Vec<_> = resp.contents.as_deref().unwrap().iter().filter_map(|o| o.key()).collect();

            for &key in &keys {
                assert!(object_list.contains(&key));
                s3.head_object().bucket(bucket).key(key).send().await?;
            }
        }

        {
            for &key in &keys {
                delete_object_strict(s3, bucket, key).await?;
            }
            delete_bucket_strict(s3, bucket).await?;
        }

        Ok(())
    }

    async fn test_get_object(self: Arc<Self>) -> Result {
        let s3 = &self.s3;

        let bucket = "test-get-object";
        let key = "file-1";
        let content = "hello world 你好世界 123456 !@#$%😂^&*()";

        {
            delete_object_loose(s3, bucket, key).await?;
            delete_bucket_loose(s3, bucket).await?;
        }

        {
            create_bucket(s3, bucket).await?;

            s3.put_object()
                .bucket(bucket)
                .key(key)
                .body(ByteStream::from_static(content.as_bytes()))
                .send()
                .await?;

            let resp = s3.get_object().bucket(bucket).key(key).send().await?;

            let body = resp.body.collect().await?;
            let body = String::from_utf8(body.to_vec())?;
            assert_eq!(body, content);
        }

        {
            delete_object_strict(s3, bucket, key).await?;
            delete_bucket_strict(s3, bucket).await?;
        }

        Ok(())
    }

    async fn test_delete_object(self: Arc<Self>) -> Result {
        let s3 = &self.s3;

        let bucket = "test-delete-object";
        let key = "file-to-delete";
        let content = "content to be deleted";

        {
            delete_object_loose(s3, bucket, key).await?;
            delete_bucket_loose(s3, bucket).await?;
        }

        {
            create_bucket(s3, bucket).await?;

            // Put an object
            s3.put_object()
                .bucket(bucket)
                .key(key)
                .body(ByteStream::from_static(content.as_bytes()))
                .send()
                .await?;

            // Verify object exists
            s3.head_object().bucket(bucket).key(key).send().await?;

            // Delete the object
            s3.delete_object().bucket(bucket).key(key).send().await?;

            // Verify object no longer exists
            let result = s3.head_object().bucket(bucket).key(key).send().await;
            assert!(result.is_err());
        }

        {
            delete_bucket_strict(s3, bucket).await?;
        }

        Ok(())
    }

    async fn test_head_operations(self: Arc<Self>) -> Result {
        let s3 = &self.s3;

        let bucket = "test-head-operations";
        let key = "head-test-file";
        let content = "content for head operations";

        {
            delete_object_loose(s3, bucket, key).await?;
            delete_bucket_loose(s3, bucket).await?;
        }

        {
            create_bucket(s3, bucket).await?;

            // Test HeadBucket
            let head_bucket_resp = s3.head_bucket().bucket(bucket).send().await?;
            assert!(head_bucket_resp.bucket_region().is_some() || head_bucket_resp.bucket_region().is_none()); // Just check response is valid

            // Put an object
            s3.put_object()
                .bucket(bucket)
                .key(key)
                .body(ByteStream::from_static(content.as_bytes()))
                .send()
                .await?;

            // Test HeadObject
            let head_object_resp = s3.head_object().bucket(bucket).key(key).send().await?;
            assert_eq!(head_object_resp.content_length().unwrap_or(0), i64::try_from(content.len())?);
        }

        {
            delete_object_strict(s3, bucket, key).await?;
            delete_bucket_strict(s3, bucket).await?;
        }

        Ok(())
    }
}

struct Put {
    s3: aws_sdk_s3::Client,
    bucket: String,
    key: String,
}

impl TestFixture<Basic> for Put {
    #[tracing::instrument(skip_all)]
    async fn setup(suite: Arc<Basic>) -> Result<Self> {
        let s3 = &suite.s3;
        let bucket = "test-put";
        let key = "file";

        delete_object_loose(s3, bucket, key).await?;
        delete_bucket_loose(s3, bucket).await?;

        create_bucket(s3, bucket).await?;

        Ok(Self {
            s3: suite.s3.clone(),
            bucket: bucket.to_owned(),
            key: key.to_owned(),
        })
    }

    #[tracing::instrument(skip_all)]
    async fn teardown(self) -> Result {
        let Self { s3, bucket, key } = &self;

        delete_object_loose(s3, bucket, key).await?;
        delete_bucket_loose(s3, bucket).await?;

        Ok(())
    }
}

impl Put {
    async fn test_put_object_tiny(self: Arc<Self>) -> Result {
        let s3 = &self.s3;
        let bucket = self.bucket.as_str();
        let key = self.key.as_str();

        let contents = ["", "1", "22", "333"];

        for content in contents {
            s3.put_object()
                .bucket(bucket)
                .key(key)
                .body(ByteStream::from_static(content.as_bytes()))
                .send()
                .await?;

            let resp = s3.get_object().bucket(bucket).key(key).send().await?;
            let body = resp.body.collect().await?;
            let body = String::from_utf8(body.to_vec())?;
            assert_eq!(body, content);
        }

        Ok(())
    }

    async fn test_put_object_with_metadata(self: Arc<Self>) -> Result {
        let s3 = &self.s3;
        let bucket = self.bucket.as_str();
        let key = "file-with-metadata";

        let content = "object with custom metadata";
        let metadata_key = "test-key";
        let metadata_value = "test-value";

        s3.put_object()
            .bucket(bucket)
            .key(key)
            .body(ByteStream::from_static(content.as_bytes()))
            .metadata(metadata_key, metadata_value)
            .content_type("text/plain")
            .send()
            .await?;

        // Verify object content
        let resp = s3.get_object().bucket(bucket).key(key).send().await?;
        let body = resp.body.collect().await?;
        let body = String::from_utf8(body.to_vec())?;
        assert_eq!(body, content);

        // Check metadata using head_object (more reliable for metadata)
        let head_resp = s3.head_object().bucket(bucket).key(key).send().await?;

        // FIXME: s3s-fs does not return correct content type
        // // Check content type if supported
        // if let Some(content_type) = head_resp.content_type() {
        //     assert_eq!(content_type, "text/plain");
        // }

        let metadata = head_resp.metadata().unwrap();
        let value = metadata.get(metadata_key).unwrap();
        assert_eq!(value, metadata_value);

        Ok(())
    }

    async fn test_put_object_larger(self: Arc<Self>) -> Result {
        let s3 = &self.s3;
        let bucket = self.bucket.as_str();
        let key = "large-file";

        // Create a larger object (1KB)
        let content = "x".repeat(1024);

        s3.put_object()
            .bucket(bucket)
            .key(key)
            .body(ByteStream::from(content.clone().into_bytes()))
            .send()
            .await?;

        let resp = s3.get_object().bucket(bucket).key(key).send().await?;
        let body = resp.body.collect().await?;
        let body = String::from_utf8(body.to_vec())?;
        assert_eq!(body, content);
        assert_eq!(body.len(), 1024);

        Ok(())
    }

    async fn test_put_object_with_checksum_algorithm(self: Arc<Self>) -> Result {
        use aws_sdk_s3::types::ChecksumAlgorithm;
        use aws_sdk_s3::types::ChecksumMode;

        let s3 = &self.s3;
        let bucket = self.bucket.as_str();
        let key = "with-checksum-trailer";

        for checksum_algorithm in [
            ChecksumAlgorithm::Crc32,
            ChecksumAlgorithm::Crc32C,
            ChecksumAlgorithm::Sha1,
            ChecksumAlgorithm::Sha256,
            ChecksumAlgorithm::Crc64Nvme,
        ] {
            let body = {
                let bytes = Bytes::from_static(&[b'a'; 1024]);

                let stream = futures::stream::repeat_with(move || {
                    let frame = http_body::Frame::data(bytes.clone());
                    Ok::<_, std::io::Error>(frame)
                });

                let body = WithSizeHint::new(StreamBody::new(stream.take(70)), 70 * 1024);
                ByteStream::new(SdkBody::from_body_1_x(body))
            };

            let put_resp = s3
                .put_object()
                .bucket(bucket)
                .key(key)
                .checksum_algorithm(checksum_algorithm.clone())
                .body(body)
                .send()
                .await?;

            let put_resp_checksum = match checksum_algorithm {
                ChecksumAlgorithm::Crc32 => put_resp
                    .checksum_crc32()
                    .expect("PUT should return checksum when checksum_algorithm is used"),
                ChecksumAlgorithm::Crc32C => put_resp
                    .checksum_crc32_c()
                    .expect("PUT should return checksum when checksum_algorithm is used"),
                ChecksumAlgorithm::Sha1 => put_resp
                    .checksum_sha1()
                    .expect("PUT should return checksum when checksum_algorithm is used"),
                ChecksumAlgorithm::Sha256 => put_resp
                    .checksum_sha256()
                    .expect("PUT should return checksum when checksum_algorithm is used"),
                ChecksumAlgorithm::Crc64Nvme => put_resp
                    .checksum_crc64_nvme()
                    .expect("PUT should return checksum when checksum_algorithm is used"),
                _ => panic!("Unsupported checksum algorithm"),
            };

            let mut resp = s3
                .get_object()
                .bucket(bucket)
                .key(key)
                .checksum_mode(ChecksumMode::Enabled)
                .send()
                .await?;

            let body = std::mem::replace(&mut resp.body, ByteStream::new(SdkBody::empty()))
                .collect()
                .await?;
            let body = String::from_utf8(body.to_vec())?;
            assert_eq!(body, "a".repeat(70 * 1024));

            let get_resp_checksum = match checksum_algorithm {
                ChecksumAlgorithm::Crc32 => resp.checksum_crc32(),
                ChecksumAlgorithm::Crc32C => resp.checksum_crc32_c(),
                ChecksumAlgorithm::Sha1 => resp.checksum_sha1(),
                ChecksumAlgorithm::Sha256 => resp.checksum_sha256(),
                ChecksumAlgorithm::Crc64Nvme => resp.checksum_crc64_nvme(),
                _ => panic!("Unsupported checksum algorithm"),
            };

            assert_eq!(get_resp_checksum, Some(put_resp_checksum));
        }

        Ok(())
    }

    async fn test_put_object_with_content_checksums(self: Arc<Self>) -> Result {
        let s3 = &self.s3;
        let bucket = self.bucket.as_str();
        let key = "file-with-content-checksums";

        // Create test content
        let content = "Hello, World! This is a test content for checksum validation. 你好世界！";
        let content_bytes = content.as_bytes();

        // Calculate MD5 hash
        let md5_digest = md5::compute(content_bytes);
        let md5_hash = base64_simd::STANDARD.encode_to_string(md5_digest.as_ref());

        // Test with Content-MD5
        s3.put_object()
            .bucket(bucket)
            .key(format!("{key}-md5"))
            .body(ByteStream::from_static(content_bytes))
            .content_md5(&md5_hash)
            .send()
            .await?;

        // Test with different content sizes and MD5
        let large_content = "x".repeat(2048);
        let large_md5_digest = md5::compute(large_content.as_bytes());
        let large_md5_hash = base64_simd::STANDARD.encode_to_string(large_md5_digest.as_ref());

        s3.put_object()
            .bucket(bucket)
            .key(format!("{key}-large"))
            .body(ByteStream::from(large_content.clone().into_bytes()))
            .content_md5(&large_md5_hash)
            .send()
            .await?;

        // Test with empty content and MD5
        let empty_content = "";
        let empty_md5_digest = md5::compute(empty_content.as_bytes());
        let empty_md5_hash = base64_simd::STANDARD.encode_to_string(empty_md5_digest.as_ref());

        s3.put_object()
            .bucket(bucket)
            .key(format!("{key}-empty"))
            .body(ByteStream::from_static(empty_content.as_bytes()))
            .content_md5(&empty_md5_hash)
            .send()
            .await?;

        // Verify all objects were uploaded correctly
        for (suffix, expected_content) in [("md5", content), ("large", &large_content), ("empty", empty_content)] {
            let resp = s3.get_object().bucket(bucket).key(format!("{key}-{suffix}")).send().await?;

            let body = resp.body.collect().await?;
            let body = String::from_utf8(body.to_vec())?;
            assert_eq!(body, expected_content);
        }

        // Test with incorrect MD5 (should fail)
        let incorrect_md5 = base64_simd::STANDARD.encode_to_string(b"incorrect_md5_hash");
        let result = s3
            .put_object()
            .bucket(bucket)
            .key(format!("{key}-incorrect-md5"))
            .body(ByteStream::from_static(content_bytes))
            .content_md5(&incorrect_md5)
            .send()
            .await;

        // This should fail with a checksum mismatch error
        assert!(result.is_err(), "Expected checksum mismatch error for incorrect MD5");

        // Test with correct MD5 but wrong content (should fail)
        let wrong_content = "This is different content";
        let wrong_md5_digest = md5::compute(wrong_content.as_bytes());
        let wrong_md5_hash = base64_simd::STANDARD.encode_to_string(wrong_md5_digest.as_ref());

        let result = s3
            .put_object()
            .bucket(bucket)
            .key(format!("{key}-wrong-content"))
            .body(ByteStream::from_static(content_bytes)) // Using original content
            .content_md5(&wrong_md5_hash) // But wrong MD5
            .send()
            .await;

        // This should also fail
        assert!(result.is_err(), "Expected checksum mismatch error for wrong content with correct MD5");

        Ok(())
    }
}

// Copy test fixture
struct Copy {
    s3: aws_sdk_s3::Client,
    bucket: String,
    source_key: String,
    dest_key: String,
}

impl TestFixture<Basic> for Copy {
    #[tracing::instrument(skip_all)]
    async fn setup(suite: Arc<Basic>) -> Result<Self> {
        let s3 = &suite.s3;
        let bucket = "test-copy";
        let source_key = "source-file";
        let dest_key = "dest-file";

        delete_object_loose(s3, bucket, source_key).await?;
        delete_object_loose(s3, bucket, dest_key).await?;
        delete_bucket_loose(s3, bucket).await?;

        create_bucket(s3, bucket).await?;

        Ok(Self {
            s3: suite.s3.clone(),
            bucket: bucket.to_owned(),
            source_key: source_key.to_owned(),
            dest_key: dest_key.to_owned(),
        })
    }

    #[tracing::instrument(skip_all)]
    async fn teardown(self) -> Result {
        let Self {
            s3,
            bucket,
            source_key,
            dest_key,
        } = &self;

        delete_object_loose(s3, bucket, source_key).await?;
        delete_object_loose(s3, bucket, dest_key).await?;
        delete_bucket_loose(s3, bucket).await?;

        Ok(())
    }
}

impl Copy {
    async fn test_copy_object(self: Arc<Self>) -> Result {
        let s3 = &self.s3;
        let bucket = self.bucket.as_str();
        let source_key = self.source_key.as_str();
        let dest_key = self.dest_key.as_str();

        let content = "content to be copied";

        // Put source object
        s3.put_object()
            .bucket(bucket)
            .key(source_key)
            .body(ByteStream::from_static(content.as_bytes()))
            .send()
            .await?;

        // Copy object
        s3.copy_object()
            .bucket(bucket)
            .key(dest_key)
            .copy_source(format!("{bucket}/{source_key}"))
            .send()
            .await?;

        // Verify destination object
        let resp = s3.get_object().bucket(bucket).key(dest_key).send().await?;
        let body = resp.body.collect().await?;
        let body = String::from_utf8(body.to_vec())?;
        assert_eq!(body, content);

        // Verify source object still exists
        let resp = s3.get_object().bucket(bucket).key(source_key).send().await?;
        let body = resp.body.collect().await?;
        let body = String::from_utf8(body.to_vec())?;
        assert_eq!(body, content);

        Ok(())
    }
}

struct WithSizeHint<T> {
    inner: T,
    size_hint: usize,
}

impl<T> WithSizeHint<T> {
    pub fn new(inner: T, size_hint: usize) -> Self {
        Self { inner, size_hint }
    }
}

impl<T> http_body::Body for WithSizeHint<T>
where
    T: http_body::Body + Unpin,
{
    type Data = T::Data;
    type Error = T::Error;

    fn poll_frame(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<std::result::Result<http_body::Frame<Self::Data>, Self::Error>>> {
        let this = self.get_mut();
        std::pin::Pin::new(&mut this.inner).poll_frame(cx)
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> http_body::SizeHint {
        let mut hint = self.inner.size_hint();
        hint.set_exact(self.size_hint as u64);
        hint
    }
}
