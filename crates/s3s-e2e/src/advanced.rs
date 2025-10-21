use crate::case;

use s3s_test::Result;
use s3s_test::TestFixture;
use s3s_test::TestSuite;
use s3s_test::tcx::TestContext;

use std::ops::Not;
use std::sync::Arc;

use aws_sdk_s3::primitives::ByteStream;
use tracing::debug;

pub fn register(tcx: &mut TestContext) {
    case!(tcx, Advanced, STS, test_assume_role);
    case!(tcx, Advanced, Multipart, test_multipart_upload);
    case!(tcx, Advanced, Tagging, test_object_tagging);
    case!(tcx, Advanced, ListPagination, test_list_objects_with_pagination);
}

struct Advanced {
    sts: aws_sdk_sts::Client,
    s3: aws_sdk_s3::Client,
}

impl TestSuite for Advanced {
    async fn setup() -> Result<Self> {
        let sdk_conf = aws_config::from_env().load().await;
        let sts = aws_sdk_sts::Client::new(&sdk_conf);
        let s3 = aws_sdk_s3::Client::from_conf(
            aws_sdk_s3::config::Builder::from(&sdk_conf)
                .force_path_style(true) // FIXME: remove force_path_style
                .build(),
        );

        Ok(Self { sts, s3 })
    }
}

#[allow(clippy::upper_case_acronyms)]
struct STS {
    sts: aws_sdk_sts::Client,
}

impl TestFixture<Advanced> for STS {
    async fn setup(suite: Arc<Advanced>) -> Result<Self> {
        Ok(Self { sts: suite.sts.clone() })
    }
}

impl STS {
    async fn test_assume_role(self: Arc<Self>) -> Result<()> {
        let sts = &self.sts;

        let result = sts.assume_role().role_arn("example").role_session_name("test").send().await;

        let resp = result?;

        let credentials = resp.credentials().unwrap();
        assert!(credentials.access_key_id().is_empty().not(), "Expected non-empty access key ID");
        assert!(credentials.secret_access_key().is_empty().not(), "Expected non-empty secret access key");
        assert!(credentials.session_token().is_empty().not(), "Expected session token in the response");

        debug!(ak=?credentials.access_key_id());
        debug!(sk=?credentials.secret_access_key());
        debug!(st=?credentials.session_token());
        debug!(exp=?credentials.expiration());

        Ok(())
    }
}

struct Multipart {
    s3: aws_sdk_s3::Client,
    bucket: String,
    key: String,
}

impl TestFixture<Advanced> for Multipart {
    async fn setup(suite: Arc<Advanced>) -> Result<Self> {
        use crate::utils::*;

        let s3 = &suite.s3;
        let bucket = "test-multipart";
        let key = "multipart-file";

        delete_object_loose(s3, bucket, key).await?;
        delete_bucket_loose(s3, bucket).await?;
        create_bucket(s3, bucket).await?;

        Ok(Self {
            s3: suite.s3.clone(),
            bucket: bucket.to_owned(),
            key: key.to_owned(),
        })
    }

    async fn teardown(self) -> Result {
        use crate::utils::*;

        let Self { s3, bucket, key } = &self;
        delete_object_loose(s3, bucket, key).await?;
        delete_bucket_loose(s3, bucket).await?;
        Ok(())
    }
}

impl Multipart {
    async fn test_multipart_upload(self: Arc<Self>) -> Result<()> {
        use aws_sdk_s3::primitives::ByteStream;

        let s3 = &self.s3;
        let bucket = self.bucket.as_str();
        let key = self.key.as_str();

        // Create multipart upload
        let create_resp = s3.create_multipart_upload().bucket(bucket).key(key).send().await?;

        let upload_id = create_resp.upload_id().unwrap();

        // Upload parts
        let part1_content = "a".repeat(5 * 1024 * 1024 + 1); // 5MB + 1 byte
        let part2_content = "b".repeat(1024); // 1KB

        let part1_resp = s3
            .upload_part()
            .bucket(bucket)
            .key(key)
            .upload_id(upload_id)
            .part_number(1)
            .body(ByteStream::from(part1_content.clone().into_bytes()))
            .send()
            .await?;

        let part2_resp = s3
            .upload_part()
            .bucket(bucket)
            .key(key)
            .upload_id(upload_id)
            .part_number(2)
            .body(ByteStream::from(part2_content.clone().into_bytes()))
            .send()
            .await?;

        // Complete multipart upload
        let completed_parts = vec![
            aws_sdk_s3::types::CompletedPart::builder()
                .part_number(1)
                .e_tag(part1_resp.e_tag().unwrap())
                .build(),
            aws_sdk_s3::types::CompletedPart::builder()
                .part_number(2)
                .e_tag(part2_resp.e_tag().unwrap())
                .build(),
        ];

        let completed_upload = aws_sdk_s3::types::CompletedMultipartUpload::builder()
            .set_parts(Some(completed_parts))
            .build();

        s3.complete_multipart_upload()
            .bucket(bucket)
            .key(key)
            .upload_id(upload_id)
            .multipart_upload(completed_upload)
            .send()
            .await?;

        // Verify the completed object
        let resp = s3.get_object().bucket(bucket).key(key).send().await?;
        let body = resp.body.collect().await?;
        let body = String::from_utf8(body.to_vec())?;

        let expected_content = format!("{part1_content}{part2_content}");
        assert_eq!(body, expected_content);

        Ok(())
    }
}

struct Tagging {
    s3: aws_sdk_s3::Client,
    bucket: String,
    key: String,
}

impl TestFixture<Advanced> for Tagging {
    async fn setup(suite: Arc<Advanced>) -> Result<Self> {
        use crate::utils::*;

        let s3 = &suite.s3;
        let bucket = "test-tagging";
        let key = "tagged-file";

        delete_object_loose(s3, bucket, key).await?;
        delete_bucket_loose(s3, bucket).await?;
        create_bucket(s3, bucket).await?;

        // Create an object to tag
        s3.put_object()
            .bucket(bucket)
            .key(key)
            .body(ByteStream::from_static(b"content for tagging"))
            .send()
            .await?;

        Ok(Self {
            s3: suite.s3.clone(),
            bucket: bucket.to_owned(),
            key: key.to_owned(),
        })
    }

    async fn teardown(self) -> Result {
        use crate::utils::*;

        let Self { s3, bucket, key } = &self;
        delete_object_loose(s3, bucket, key).await?;
        delete_bucket_loose(s3, bucket).await?;
        Ok(())
    }
}

impl Tagging {
    async fn test_object_tagging(self: Arc<Self>) -> Result<()> {
        let s3 = &self.s3;
        let bucket = self.bucket.as_str();
        let key = self.key.as_str();

        // Create tags
        let tag1 = aws_sdk_s3::types::Tag::builder()
            .key("Environment")
            .value("Test")
            .build()
            .unwrap();

        let tag2 = aws_sdk_s3::types::Tag::builder().key("Project").value("S3S").build().unwrap();

        let tag_set = aws_sdk_s3::types::Tagging::builder()
            .tag_set(tag1.clone())
            .tag_set(tag2.clone())
            .build()
            .unwrap();

        // Put object tagging
        s3.put_object_tagging()
            .bucket(bucket)
            .key(key)
            .tagging(tag_set)
            .send()
            .await?;

        // Get object tagging
        let resp = s3.get_object_tagging().bucket(bucket).key(key).send().await?;

        let tag_set = resp.tag_set();
        assert_eq!(tag_set.len(), 2);

        // Verify tags
        let tag_map: std::collections::HashMap<&str, &str> = tag_set.iter().map(|tag| (tag.key(), tag.value())).collect();

        assert_eq!(tag_map.get("Environment"), Some(&"Test"));
        assert_eq!(tag_map.get("Project"), Some(&"S3S"));

        Ok(())
    }
}

struct ListPagination {
    s3: aws_sdk_s3::Client,
    bucket: String,
}

impl TestFixture<Advanced> for ListPagination {
    async fn setup(suite: Arc<Advanced>) -> Result<Self> {
        use crate::utils::*;
        use aws_sdk_s3::primitives::ByteStream;

        let s3 = &suite.s3;
        let bucket = "test-list-pagination";

        // Clean up any existing objects
        for i in 0..10 {
            delete_object_loose(s3, bucket, &format!("file-{i:02}")).await?;
        }
        delete_bucket_loose(s3, bucket).await?;

        create_bucket(s3, bucket).await?;

        // Create multiple objects for pagination testing
        for i in 0..10 {
            s3.put_object()
                .bucket(bucket)
                .key(format!("file-{i:02}"))
                .body(ByteStream::from_static(b"test content"))
                .send()
                .await?;
        }

        Ok(Self {
            s3: suite.s3.clone(),
            bucket: bucket.to_owned(),
        })
    }

    async fn teardown(self) -> Result {
        use crate::utils::*;

        let Self { s3, bucket } = &self;

        // Clean up all objects
        for i in 0..10 {
            delete_object_loose(s3, bucket, &format!("file-{i:02}")).await?;
        }
        delete_bucket_loose(s3, bucket).await?;
        Ok(())
    }
}

impl ListPagination {
    async fn test_list_objects_with_pagination(self: Arc<Self>) -> Result<()> {
        let s3 = &self.s3;
        let bucket = self.bucket.as_str();

        // Test list objects with max-keys
        let resp = s3.list_objects_v2().bucket(bucket).max_keys(5).send().await?;

        let objects = resp.contents();
        assert_eq!(objects.len(), 5);
        assert!(resp.is_truncated().unwrap_or(false));

        // Test continuation
        if let Some(continuation_token) = resp.next_continuation_token() {
            let resp2 = s3
                .list_objects_v2()
                .bucket(bucket)
                .continuation_token(continuation_token)
                .send()
                .await?;

            let objects2 = resp2.contents();
            assert_eq!(objects2.len(), 5);
        }

        // Test prefix filtering
        let resp = s3.list_objects_v2().bucket(bucket).prefix("file-0").send().await?;

        let objects = resp.contents();
        // Should match file-00 through file-09
        assert_eq!(objects.len(), 10);

        for obj in objects {
            assert!(obj.key().unwrap().starts_with("file-0"));
        }

        Ok(())
    }
}
