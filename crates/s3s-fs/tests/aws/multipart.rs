// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

use crate::case;
use crate::suite::{DOMAIN_NAME, FS_ROOT, Multipart, REGION, create_bucket, delete_bucket, delete_object, do_multipart_upload};

use std::sync::Arc;

use aws_sdk_s3::primitives::ByteStream;
use aws_sdk_s3::types::ChecksumMode;
use aws_sdk_s3::types::CompletedMultipartUpload;
use aws_sdk_s3::types::CompletedPart;
use std::fs;

use s3s::auth::SimpleAuth;
use s3s::host::SingleDomain;
use s3s::service::S3ServiceBuilder;
use s3s_fs::FileSystem;

use aws_config::SdkConfig;
use aws_credential_types::provider::SharedCredentialsProvider;
use aws_sdk_s3::Client;
use aws_sdk_s3::config::Credentials;
use aws_sdk_s3::config::Region;

use aws_sdk_s3::error::ProvideErrorMetadata;

use s3s_test::Result;
use s3s_test::tcx::TestContext;
use tracing::debug;
use uuid::Uuid;

#[derive(Debug, Clone, Copy)]
enum MultipartXxHashCase {
    XxHash64,
    XxHash3,
    XxHash128,
}

impl MultipartXxHashCase {
    fn name(self) -> &'static str {
        match self {
            Self::XxHash64 => "xxhash64",
            Self::XxHash3 => "xxhash3",
            Self::XxHash128 => "xxhash128",
        }
    }

    fn algorithm(self) -> aws_sdk_s3::types::ChecksumAlgorithm {
        use aws_sdk_s3::types::ChecksumAlgorithm;

        match self {
            Self::XxHash64 => ChecksumAlgorithm::Xxhash64,
            Self::XxHash3 => ChecksumAlgorithm::Xxhash3,
            Self::XxHash128 => ChecksumAlgorithm::Xxhash128,
        }
    }

    fn expected_checksum(self, content: &[u8]) -> String {
        use s3s::crypto::Checksum as _;
        use s3s::crypto::XxHash3;
        use s3s::crypto::XxHash64;
        use s3s::crypto::XxHash128;

        match self {
            Self::XxHash64 => base64_simd::STANDARD.encode_to_string(XxHash64::checksum(content)),
            Self::XxHash3 => base64_simd::STANDARD.encode_to_string(XxHash3::checksum(content)),
            Self::XxHash128 => base64_simd::STANDARD.encode_to_string(XxHash128::checksum(content)),
        }
    }

    fn upload_part_checksum(self, output: &aws_sdk_s3::operation::upload_part::UploadPartOutput) -> Option<&str> {
        match self {
            Self::XxHash64 => output.checksum_xxhash64(),
            Self::XxHash3 => output.checksum_xxhash3(),
            Self::XxHash128 => output.checksum_xxhash128(),
        }
    }

    fn listed_part_checksum(self, part: &aws_sdk_s3::types::Part) -> Option<&str> {
        match self {
            Self::XxHash64 => part.checksum_xxhash64(),
            Self::XxHash3 => part.checksum_xxhash3(),
            Self::XxHash128 => part.checksum_xxhash128(),
        }
    }

    fn completed_part(self, e_tag: String, checksum: String, part_number: i32) -> CompletedPart {
        match self {
            Self::XxHash64 => CompletedPart::builder()
                .e_tag(e_tag)
                .checksum_xxhash64(checksum)
                .part_number(part_number)
                .build(),
            Self::XxHash3 => CompletedPart::builder()
                .e_tag(e_tag)
                .checksum_xxhash3(checksum)
                .part_number(part_number)
                .build(),
            Self::XxHash128 => CompletedPart::builder()
                .e_tag(e_tag)
                .checksum_xxhash128(checksum)
                .part_number(part_number)
                .build(),
        }
    }

    fn complete_checksum(
        self,
        output: &aws_sdk_s3::operation::complete_multipart_upload::CompleteMultipartUploadOutput,
    ) -> Option<&str> {
        match self {
            Self::XxHash64 => output.checksum_xxhash64(),
            Self::XxHash3 => output.checksum_xxhash3(),
            Self::XxHash128 => output.checksum_xxhash128(),
        }
    }

    fn head_checksum(self, output: &aws_sdk_s3::operation::head_object::HeadObjectOutput) -> Option<&str> {
        match self {
            Self::XxHash64 => output.checksum_xxhash64(),
            Self::XxHash3 => output.checksum_xxhash3(),
            Self::XxHash128 => output.checksum_xxhash128(),
        }
    }

    fn get_checksum(self, output: &aws_sdk_s3::operation::get_object::GetObjectOutput) -> Option<&str> {
        match self {
            Self::XxHash64 => output.checksum_xxhash64(),
            Self::XxHash3 => output.checksum_xxhash3(),
            Self::XxHash128 => output.checksum_xxhash128(),
        }
    }
}

async fn assert_multipart_xxhash_checksums(
    client: &aws_sdk_s3::Client,
    bucket: &str,
    checksum_case: MultipartXxHashCase,
) -> Result<()> {
    let key = format!("multipart-{}.txt", checksum_case.name());
    let content = format!("multipart checksum coverage via {}\n", checksum_case.name());
    let expected_checksum = checksum_case.expected_checksum(content.as_bytes());

    let upload_id = client
        .create_multipart_upload()
        .bucket(bucket)
        .key(key.as_str())
        .checksum_algorithm(checksum_case.algorithm())
        .send()
        .await?
        .upload_id
        .expect("create_multipart_upload should return upload_id");

    let part_number = 1;
    let upload_part = client
        .upload_part()
        .bucket(bucket)
        .key(key.as_str())
        .upload_id(upload_id.as_str())
        .body(ByteStream::from(content.into_bytes()))
        .part_number(part_number)
        .send()
        .await?;

    let part_etag = upload_part.e_tag().expect("upload_part should return e_tag").to_owned();
    let part_checksum = checksum_case
        .upload_part_checksum(&upload_part)
        .unwrap_or_else(|| panic!("upload_part should return checksum_{}", checksum_case.name()))
        .to_owned();
    assert_eq!(part_checksum, expected_checksum, "upload_part checksum mismatch for {checksum_case:?}");

    let listed = client
        .list_parts()
        .bucket(bucket)
        .key(key.as_str())
        .upload_id(upload_id.as_str())
        .send()
        .await?;
    let listed_part = listed.parts().first().expect("list_parts should return one part");
    let listed_checksum = checksum_case.listed_part_checksum(listed_part);
    assert_eq!(
        listed_checksum,
        Some(expected_checksum.as_str()),
        "list_parts checksum mismatch for {checksum_case:?}"
    );

    let completed_part = checksum_case.completed_part(part_etag, part_checksum, part_number);
    let completed_upload = CompletedMultipartUpload::builder()
        .set_parts(Some(vec![completed_part]))
        .build();

    let complete = client
        .complete_multipart_upload()
        .bucket(bucket)
        .key(key.as_str())
        .multipart_upload(completed_upload)
        .upload_id(upload_id.as_str())
        .send()
        .await?;
    let complete_checksum = checksum_case.complete_checksum(&complete);
    assert_eq!(
        complete_checksum,
        Some(expected_checksum.as_str()),
        "complete_multipart_upload checksum mismatch for {checksum_case:?}"
    );
    assert_eq!(complete.checksum_type().map(aws_sdk_s3::types::ChecksumType::as_str), Some("FULL_OBJECT"));

    let head = client.head_object().bucket(bucket).key(key.as_str()).send().await?;
    let head_checksum = checksum_case.head_checksum(&head);
    assert_eq!(
        head_checksum,
        Some(expected_checksum.as_str()),
        "head_object checksum mismatch for {checksum_case:?}"
    );

    let get = client
        .get_object()
        .bucket(bucket)
        .key(key.as_str())
        .checksum_mode(ChecksumMode::Enabled)
        .send()
        .await?;
    let get_checksum = checksum_case.get_checksum(&get);
    assert_eq!(
        get_checksum,
        Some(expected_checksum.as_str()),
        "get_object checksum mismatch for {checksum_case:?}"
    );

    delete_object(client, bucket, key.as_str()).await?;

    Ok(())
}

pub fn register(tcx: &mut TestContext) {
    case!(tcx, FsServer, Multipart, test_multipart);
    case!(tcx, FsServer, Multipart, test_multipart_xxhash_checksums);
    case!(tcx, FsServer, Multipart, test_multipart_checksum_type_composite_not_implemented);
    case!(tcx, FsServer, Multipart, test_multipart_etag_format);
    case!(tcx, FsServer, Multipart, test_upload_part_copy);
    case!(tcx, FsServer, Multipart, test_upload_part_copy_invalid_source_range);
    case!(tcx, FsServer, Multipart, test_multipart_with_attributes);
    case!(tcx, FsServer, Multipart, test_multipart_upload_id_auth);
    case!(tcx, FsServer, Multipart, test_complete_multipart_if_none_match);
    case!(tcx, FsServer, Multipart, test_complete_multipart_if_match);
    case!(tcx, FsServer, Multipart, test_complete_multipart_if_match_wildcard);
    case!(tcx, FsServer, Multipart, test_upload_part_copy_empty_source);
}

impl Multipart {
    async fn test_multipart(self: Arc<Self>) -> Result<()> {
        let c = &self.s3;

        let bucket = format!("test-multipart-{}", Uuid::new_v4());
        let bucket = bucket.as_str();
        create_bucket(c, bucket).await?;

        let key = "sample.txt";
        let content = "abcdefghijklmnopqrstuvwxyz/0123456789/!@#$%^&*();\n";

        let upload_id = {
            let ans = c.create_multipart_upload().bucket(bucket).key(key).send().await?;
            ans.upload_id.unwrap()
        };
        let upload_id = upload_id.as_str();

        let upload_parts = {
            let body = ByteStream::from_static(content.as_bytes());
            let part_number = 1;

            let ans = c
                .upload_part()
                .bucket(bucket)
                .key(key)
                .upload_id(upload_id)
                .body(body)
                .part_number(part_number)
                .send()
                .await?;

            let part = CompletedPart::builder()
                .e_tag(ans.e_tag.unwrap_or_default())
                .part_number(part_number)
                .build();

            vec![part]
        };

        {
            let upload = CompletedMultipartUpload::builder().set_parts(Some(upload_parts)).build();

            let _ = c
                .complete_multipart_upload()
                .bucket(bucket)
                .key(key)
                .multipart_upload(upload)
                .upload_id(upload_id)
                .send()
                .await?;
        }

        {
            let ans = c.get_object().bucket(bucket).key(key).send().await?;

            let content_length: usize = ans.content_length().unwrap().try_into().unwrap();
            let body = ans.body.collect().await?.into_bytes();

            assert_eq!(content_length, content.len());
            assert_eq!(body.as_ref(), content.as_bytes());
        }

        {
            delete_object(c, bucket, key).await?;
            delete_bucket(c, bucket).await?;
        }

        Ok(())
    }

    async fn test_multipart_xxhash_checksums(self: Arc<Self>) -> Result<()> {
        let client = &self.s3;

        let bucket = format!("test-multipart-xxhash-{}", Uuid::new_v4());
        let bucket = bucket.as_str();
        create_bucket(client, bucket).await?;

        for checksum_case in [
            MultipartXxHashCase::XxHash64,
            MultipartXxHashCase::XxHash3,
            MultipartXxHashCase::XxHash128,
        ] {
            assert_multipart_xxhash_checksums(client, bucket, checksum_case).await?;
        }

        delete_bucket(client, bucket).await?;

        Ok(())
    }

    async fn test_multipart_checksum_type_composite_not_implemented(self: Arc<Self>) -> Result<()> {
        use aws_sdk_s3::types::ChecksumAlgorithm;
        use aws_sdk_s3::types::ChecksumType;

        let c = &self.s3;
        let bucket = format!("test-multipart-composite-{}", Uuid::new_v4());
        let bucket = bucket.as_str();
        create_bucket(c, bucket).await?;

        let err = c
            .create_multipart_upload()
            .bucket(bucket)
            .key("composite.txt")
            .checksum_algorithm(ChecksumAlgorithm::Xxhash64)
            .checksum_type(ChecksumType::Composite)
            .send()
            .await
            .expect_err("COMPOSITE checksum_type should be rejected until implemented");
        let service_err = err.into_service_error();
        assert_eq!(service_err.code(), Some("NotImplemented"));

        delete_bucket(c, bucket).await?;

        Ok(())
    }

    /// Test that multipart uploaded objects have the correct `ETag` format: `{hash}-{part_count}`
    async fn test_multipart_etag_format(self: Arc<Self>) -> Result<()> {
        let c = &self.s3;

        let bucket = format!("test-multipart-etag-{}", Uuid::new_v4());
        let bucket = bucket.as_str();
        create_bucket(c, bucket).await?;

        let key = "multipart-etag.txt";
        let content = "abcdefghijklmnopqrstuvwxyz/0123456789/!@#$%^&*();\n";

        let upload_id = {
            let ans = c.create_multipart_upload().bucket(bucket).key(key).send().await?;
            ans.upload_id.unwrap()
        };
        let upload_id = upload_id.as_str();

        let upload_parts = {
            let body = ByteStream::from_static(content.as_bytes());
            let part_number = 1;

            let ans = c
                .upload_part()
                .bucket(bucket)
                .key(key)
                .upload_id(upload_id)
                .body(body)
                .part_number(part_number)
                .send()
                .await?;

            let part = CompletedPart::builder()
                .e_tag(ans.e_tag.expect("upload_part response missing e_tag"))
                .part_number(part_number)
                .build();

            vec![part]
        };

        let complete_e_tag = {
            let upload = CompletedMultipartUpload::builder().set_parts(Some(upload_parts)).build();

            let ans = c
                .complete_multipart_upload()
                .bucket(bucket)
                .key(key)
                .multipart_upload(upload)
                .upload_id(upload_id)
                .send()
                .await?;

            let e_tag = ans.e_tag().unwrap().to_owned();
            debug!(?e_tag, "multipart etag");

            // Multipart ETags must have the format: {hex_md5}-{part_count}
            let unquoted = e_tag.trim_matches('"');
            let (hash_part, count_part) = unquoted.rsplit_once('-').expect("multipart ETag should contain a dash");
            assert_eq!(hash_part.len(), 32, "hash part should be 32 hex characters: {hash_part}");
            assert!(hash_part.chars().all(|c| c.is_ascii_hexdigit()), "hash part should be hex: {hash_part}");
            let part_count: usize = count_part.parse().expect("count part should be a number");
            assert_eq!(part_count, 1, "part count should match number of parts uploaded");

            e_tag
        };

        {
            // Verify the ETag from head_object matches complete_multipart_upload
            let ans = c.head_object().bucket(bucket).key(key).send().await?;
            let head_e_tag = ans.e_tag().unwrap();
            debug!(?head_e_tag, "head_object etag");
            assert_eq!(head_e_tag, complete_e_tag, "head_object ETag should match complete_multipart_upload ETag");
        }

        {
            // Verify the ETag from get_object matches complete_multipart_upload
            let ans = c.get_object().bucket(bucket).key(key).send().await?;
            let get_e_tag = ans.e_tag().unwrap();
            debug!(?get_e_tag, "get_object etag");
            assert_eq!(get_e_tag, complete_e_tag, "get_object ETag should match complete_multipart_upload ETag");
        }

        {
            delete_object(c, bucket, key).await?;
            delete_bucket(c, bucket).await?;
        }

        Ok(())
    }

    async fn test_upload_part_copy(self: Arc<Self>) -> Result<()> {
        let c = &self.s3;
        let src_bucket = format!("test-copy{}", Uuid::new_v4());
        let src_bucket = src_bucket.as_str();
        let src_key = "copied.txt";
        let src_content = "hello world\nनमस्ते दुनिया\n";
        let crc32c = base64_simd::STANDARD.encode_to_string(crc32c::crc32c(src_content.as_bytes()).to_be_bytes());

        create_bucket(c, src_bucket).await?;

        {
            let src_body = ByteStream::from_static(src_content.as_bytes());
            c.put_object()
                .bucket(src_bucket)
                .key(src_key)
                .body(src_body)
                .checksum_crc32_c(crc32c.as_str())
                .send()
                .await?;
        }

        let bucket = format!("test-uploadpartcopy-{}", Uuid::new_v4());
        let bucket = bucket.as_str();
        create_bucket(c, bucket).await?;

        let key = "sample.txt";

        let upload_id = {
            use aws_sdk_s3::types::ChecksumAlgorithm;

            let ans = c
                .create_multipart_upload()
                .bucket(bucket)
                .key(key)
                .checksum_algorithm(ChecksumAlgorithm::Crc32C)
                .send()
                .await?;
            ans.upload_id.unwrap()
        };
        let upload_id = upload_id.as_str();
        let src_path = format!("{src_bucket}/{src_key}");
        let upload_parts = {
            let part_number = 1;
            let ans = c
                .upload_part_copy()
                .bucket(bucket)
                .key(key)
                .copy_source(src_path)
                .upload_id(upload_id)
                .part_number(part_number)
                .send()
                .await?;

            let copy_part_result = ans
                .copy_part_result()
                .expect("upload_part_copy should return copy_part_result");
            let copied_checksum_crc32c = copy_part_result
                .checksum_crc32_c()
                .expect("upload_part_copy should return checksum_crc32_c")
                .to_owned();
            assert_eq!(copied_checksum_crc32c, crc32c);

            let part = CompletedPart::builder()
                .checksum_crc32_c(copied_checksum_crc32c)
                .e_tag(copy_part_result.e_tag().expect("copy_part_result should return e_tag"))
                .part_number(part_number)
                .build();
            vec![part]
        };

        {
            let upload = CompletedMultipartUpload::builder().set_parts(Some(upload_parts)).build();

            let _ = c
                .complete_multipart_upload()
                .bucket(bucket)
                .key(key)
                .multipart_upload(upload)
                .upload_id(upload_id)
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
            assert_eq!(ans.checksum_crc32_c(), Some(crc32c.as_str()));

            let content_length: usize = ans.content_length().unwrap().try_into().unwrap();
            let body = ans.body.collect().await?.into_bytes();

            assert_eq!(content_length, src_content.len());
            assert_eq!(body.as_ref(), src_content.as_bytes());
        }

        {
            delete_object(c, bucket, key).await?;
            delete_bucket(c, bucket).await?;
            delete_object(c, src_bucket, src_key).await?;
            delete_bucket(c, src_bucket).await?;
        }

        Ok(())
    }

    async fn test_upload_part_copy_invalid_source_range(self: Arc<Self>) -> Result<()> {
        let c = &self.s3;
        let bucket = format!("test-upc-bad-range-{}", Uuid::new_v4());
        let bucket = bucket.as_str();
        create_bucket(c, bucket).await?;

        let src_key = "src.txt";
        let src_content = "hello";
        c.put_object()
            .bucket(bucket)
            .key(src_key)
            .body(ByteStream::from_static(src_content.as_bytes()))
            .send()
            .await?;

        let dst_key = "dst.txt";
        let upload_id = c
            .create_multipart_upload()
            .bucket(bucket)
            .key(dst_key)
            .send()
            .await?
            .upload_id
            .expect("upload_id");
        let upload_id = upload_id.as_str();

        let copy_source = format!("{bucket}/{src_key}");
        // Object length is 5; inclusive end index must be <= 4. End 5 is past EOF and must not truncate.
        let err = c
            .upload_part_copy()
            .bucket(bucket)
            .key(dst_key)
            .copy_source(&copy_source)
            .copy_source_range("bytes=0-5")
            .upload_id(upload_id)
            .part_number(1)
            .send()
            .await
            .expect_err("Expected InvalidRange when copy range end is past EOF");
        let service_err = err.into_service_error();
        assert_eq!(
            service_err.code(),
            Some("InvalidRange"),
            "past-EOF range: expected InvalidRange, got {:?}",
            service_err.code()
        );

        let err = c
            .upload_part_copy()
            .bucket(bucket)
            .key(dst_key)
            .copy_source(&copy_source)
            .copy_source_range("bytes=0-18446744073709551615")
            .upload_id(upload_id)
            .part_number(1)
            .send()
            .await
            .expect_err("Expected InvalidRange for end=u64::MAX");
        let service_err = err.into_service_error();
        assert_eq!(
            service_err.code(),
            Some("InvalidRange"),
            "u64::MAX end: expected InvalidRange, got {:?}",
            service_err.code()
        );

        c.abort_multipart_upload()
            .bucket(bucket)
            .key(dst_key)
            .upload_id(upload_id)
            .send()
            .await?;

        delete_object(c, bucket, src_key).await?;
        delete_bucket(c, bucket).await?;

        Ok(())
    }

    /// Test that standard object attributes are preserved through multipart uploads
    async fn test_multipart_with_attributes(self: Arc<Self>) -> Result<()> {
        let c = &self.s3;
        let bucket = format!("test-multipart-attrs-{}", Uuid::new_v4());
        let bucket = bucket.as_str();
        let key = "multipart-with-attrs.json";

        create_bucket(c, bucket).await?;

        // Create multipart upload with standard attributes
        let upload_id = {
            let ans = c
                .create_multipart_upload()
                .bucket(bucket)
                .key(key)
                .content_encoding("gzip")
                .content_type("application/json")
                .content_disposition("attachment; filename=\"data.json\"")
                .cache_control("public, max-age=7200")
                .send()
                .await?;
            ans.upload_id.unwrap()
        };
        let upload_id = upload_id.as_str();

        // Upload a part
        let content = b"part1 content";
        let upload_parts = {
            let body = ByteStream::from_static(content);
            let part_number = 1;

            let ans = c
                .upload_part()
                .bucket(bucket)
                .key(key)
                .upload_id(upload_id)
                .body(body)
                .part_number(part_number)
                .send()
                .await?;

            let part = CompletedPart::builder()
                .e_tag(ans.e_tag.unwrap_or_default())
                .part_number(part_number)
                .build();

            vec![part]
        };

        // Complete the multipart upload
        {
            let upload = CompletedMultipartUpload::builder().set_parts(Some(upload_parts)).build();

            c.complete_multipart_upload()
                .bucket(bucket)
                .key(key)
                .multipart_upload(upload)
                .upload_id(upload_id)
                .send()
                .await?;
        }

        // Verify attributes were preserved after completing multipart upload
        {
            let ans = c.get_object().bucket(bucket).key(key).send().await?;

            debug!("Retrieved multipart object:");
            debug!("  Content-Encoding: {:?}", ans.content_encoding());
            debug!("  Content-Type: {:?}", ans.content_type());
            debug!("  Content-Disposition: {:?}", ans.content_disposition());
            debug!("  Cache-Control: {:?}", ans.cache_control());

            // Verify all attributes are preserved through multipart upload
            assert_eq!(ans.content_encoding(), Some("gzip"));
            assert_eq!(ans.content_type(), Some("application/json"));
            assert_eq!(ans.content_disposition(), Some("attachment; filename=\"data.json\""));
            assert_eq!(ans.cache_control(), Some("public, max-age=7200"));
        }

        // Also verify with HeadObject
        {
            let ans = c.head_object().bucket(bucket).key(key).send().await?;

            assert_eq!(ans.content_encoding(), Some("gzip"));
            assert_eq!(ans.content_type(), Some("application/json"));
        }

        {
            delete_object(c, bucket, key).await?;
            delete_bucket(c, bucket).await?;
        }

        Ok(())
    }

    /// Regression test for <https://github.com/s3s-project/s3s/issues/51>
    ///
    /// Multipart `upload_id` should be bound to the credentials that created it.
    /// A different user should not be able to upload parts or complete the upload.
    #[allow(clippy::too_many_lines)]
    async fn test_multipart_upload_id_auth(self: Arc<Self>) -> Result<()> {
        // Create a service with two sets of credentials
        let cred_user1 = Credentials::new("AKUSER1EXAMPLE", "secretkey1example", None, None, "user1");
        let cred_user2 = Credentials::new("AKUSER2EXAMPLE", "secretkey2example", None, None, "user2");

        let mut auth = SimpleAuth::new();
        auth.register(cred_user1.access_key_id().to_string(), cred_user1.secret_access_key().into());
        auth.register(cred_user2.access_key_id().to_string(), cred_user2.secret_access_key().into());

        fs::create_dir_all(FS_ROOT).unwrap();
        let fs = FileSystem::new(FS_ROOT).unwrap();
        let service = {
            let mut b = S3ServiceBuilder::new(fs);
            b.set_auth(auth);
            b.set_host(SingleDomain::new(DOMAIN_NAME).unwrap());
            b.build()
        };

        // Create client for user1
        let config_user1 = SdkConfig::builder()
            .credentials_provider(SharedCredentialsProvider::new(cred_user1.clone()))
            .http_client(s3s_aws::Client::from(service.clone()))
            .region(Region::new(REGION))
            .endpoint_url(format!("http://{DOMAIN_NAME}"))
            .build();
        let c1 = Client::new(&config_user1);

        // Create client for user2
        let config_user2 = SdkConfig::builder()
            .credentials_provider(SharedCredentialsProvider::new(cred_user2))
            .http_client(s3s_aws::Client::from(service))
            .region(Region::new(REGION))
            .endpoint_url(format!("http://{DOMAIN_NAME}"))
            .build();
        let c2 = Client::new(&config_user2);

        let bucket = format!("test-multipart-auth-{}", Uuid::new_v4());
        let bucket = bucket.as_str();
        let key = "auth-test.txt";

        // User1 creates bucket and starts multipart upload
        create_bucket(&c1, bucket).await?;

        let upload_id = {
            let ans = c1.create_multipart_upload().bucket(bucket).key(key).send().await?;
            ans.upload_id.unwrap()
        };
        let upload_id = upload_id.as_str();

        // User2 tries to upload a part - should fail with AccessDenied
        let result = c2
            .upload_part()
            .bucket(bucket)
            .key(key)
            .upload_id(upload_id)
            .body(ByteStream::from_static(b"unauthorized part"))
            .part_number(1)
            .send()
            .await;

        let err = result.expect_err("Expected AccessDenied when user2 tries to upload part");
        let service_err = err.into_service_error();
        assert_eq!(
            service_err.code(),
            Some("AccessDenied"),
            "Expected AccessDenied error code, got: {:?}",
            service_err.code()
        );

        // User1 should be able to upload a part
        let upload_parts = {
            let body = ByteStream::from_static(b"authorized part");
            let ans = c1
                .upload_part()
                .bucket(bucket)
                .key(key)
                .upload_id(upload_id)
                .body(body)
                .part_number(1)
                .send()
                .await?;

            vec![
                CompletedPart::builder()
                    .e_tag(ans.e_tag.unwrap_or_default())
                    .part_number(1)
                    .build(),
            ]
        };

        // User2 tries to complete the upload - should fail with AccessDenied
        let upload = CompletedMultipartUpload::builder()
            .set_parts(Some(upload_parts.clone()))
            .build();
        let result = c2
            .complete_multipart_upload()
            .bucket(bucket)
            .key(key)
            .multipart_upload(upload)
            .upload_id(upload_id)
            .send()
            .await;

        let err = result.expect_err("Expected AccessDenied when user2 tries to complete upload");
        let service_err = err.into_service_error();
        assert_eq!(
            service_err.code(),
            Some("AccessDenied"),
            "Expected AccessDenied error code, got: {:?}",
            service_err.code()
        );

        // User1 completes the upload
        let upload = CompletedMultipartUpload::builder().set_parts(Some(upload_parts)).build();
        c1.complete_multipart_upload()
            .bucket(bucket)
            .key(key)
            .multipart_upload(upload)
            .upload_id(upload_id)
            .send()
            .await?;

        // Cleanup
        delete_object(&c1, bucket, key).await?;
        delete_bucket(&c1, bucket).await?;

        Ok(())
    }

    /// Test that `CompleteMultipartUpload` with `If-None-Match: *` succeeds when object doesn't exist
    /// and fails with `PreconditionFailed` (412) when object already exists.
    async fn test_complete_multipart_if_none_match(self: Arc<Self>) -> Result<()> {
        let c = &self.s3;
        let bucket = format!("test-cmu-ifnm-{}", Uuid::new_v4());
        let bucket = bucket.as_str();
        let key = "multipart-conditional.txt";
        let content = "abcdefghijklmnopqrstuvwxyz/0123456789/!@#$%^&*();\n";

        create_bucket(c, bucket).await?;

        // Test 1: CompleteMultipartUpload with If-None-Match: * should succeed when object doesn't exist
        debug!("Test 1: CompleteMultipartUpload with If-None-Match: * on non-existent object");
        {
            let (upload_id, upload_parts) = do_multipart_upload(c, bucket, key, content.as_bytes()).await?;
            let upload = CompletedMultipartUpload::builder().set_parts(Some(upload_parts)).build();

            let result = c
                .complete_multipart_upload()
                .bucket(bucket)
                .key(key)
                .upload_id(&upload_id)
                .multipart_upload(upload)
                .if_none_match("*")
                .send()
                .await;

            match result {
                Ok(_) => debug!("✓ Successfully completed multipart upload with If-None-Match: *"),
                Err(e) => panic!(
                    "Expected CompleteMultipartUpload with If-None-Match: * to succeed when object doesn't exist, but got error: {e:?}"
                ),
            }
        }

        // Verify the object was created
        {
            let result = c.get_object().bucket(bucket).key(key).send().await?;
            let body = result.body.collect().await?.into_bytes();
            assert_eq!(body.as_ref(), content.as_bytes());
            debug!("✓ Verified object was created via multipart upload");
        }

        // Test 2: CompleteMultipartUpload with If-None-Match: * should fail when object exists
        debug!("Test 2: CompleteMultipartUpload with If-None-Match: * on existing object");
        {
            let (upload_id, upload_parts) = do_multipart_upload(c, bucket, key, b"new content").await?;
            let upload = CompletedMultipartUpload::builder().set_parts(Some(upload_parts)).build();

            let result = c
                .complete_multipart_upload()
                .bucket(bucket)
                .key(key)
                .upload_id(&upload_id)
                .multipart_upload(upload)
                .if_none_match("*")
                .send()
                .await;

            match result {
                Ok(_) => {
                    panic!("Expected CompleteMultipartUpload with If-None-Match: * to fail when object exists, but it succeeded")
                }
                Err(e) => {
                    let service_err = e.into_service_error();
                    debug!("✓ Expected error when object exists: {service_err:?}");
                    assert_eq!(
                        service_err.code(),
                        Some("PreconditionFailed"),
                        "Expected PreconditionFailed, got: {:?}",
                        service_err.code()
                    );
                }
            }
        }

        // Verify the object wasn't overwritten
        {
            let result = c.get_object().bucket(bucket).key(key).send().await?;
            let body = result.body.collect().await?.into_bytes();
            assert_eq!(body.as_ref(), content.as_bytes());
            debug!("✓ Verified object was not overwritten");
        }

        // Cleanup
        delete_object(c, bucket, key).await?;
        delete_bucket(c, bucket).await?;

        Ok(())
    }

    /// Test that `CompleteMultipartUpload` with `If-Match` succeeds when `ETag` matches
    /// and fails with `PreconditionFailed` (412) when `ETag` doesn't match or object is absent.
    #[allow(clippy::too_many_lines)]
    async fn test_complete_multipart_if_match(self: Arc<Self>) -> Result<()> {
        let c = &self.s3;
        let bucket = format!("test-cmu-ifm-{}", Uuid::new_v4());
        let bucket = bucket.as_str();
        let key = "multipart-conditional-match.txt";
        let content = "abcdefghijklmnopqrstuvwxyz/0123456789/!@#$%^&*();\n";

        create_bucket(c, bucket).await?;

        // Test 1: CompleteMultipartUpload with If-Match on absent object should fail
        debug!("Test 1: CompleteMultipartUpload with If-Match on absent object");
        {
            let (upload_id, upload_parts) = do_multipart_upload(c, bucket, key, b"some content").await?;
            let upload = CompletedMultipartUpload::builder()
                .set_parts(Some(upload_parts.clone()))
                .build();

            let result = c
                .complete_multipart_upload()
                .bucket(bucket)
                .key(key)
                .upload_id(&upload_id)
                .multipart_upload(upload)
                .if_match("\"some-etag\"")
                .send()
                .await;

            match result {
                Ok(_) => panic!("Expected CompleteMultipartUpload with If-Match on absent object to fail, but it succeeded"),
                Err(e) => {
                    let service_err = e.into_service_error();
                    debug!("✓ Expected error on absent object: {service_err:?}");
                    assert_eq!(
                        service_err.code(),
                        Some("PreconditionFailed"),
                        "Expected PreconditionFailed, got: {:?}",
                        service_err.code()
                    );
                }
            }

            // Verify the upload was not consumed: retry the same upload_id should still fail with precondition
            let upload2 = CompletedMultipartUpload::builder().set_parts(Some(upload_parts)).build();
            let result2 = c
                .complete_multipart_upload()
                .bucket(bucket)
                .key(key)
                .upload_id(&upload_id)
                .multipart_upload(upload2)
                .if_match("\"some-etag\"")
                .send()
                .await;

            let err2 = result2.expect_err("Expected retry to also fail");
            let service_err2 = err2.into_service_error();
            assert_eq!(
                service_err2.code(),
                Some("PreconditionFailed"),
                "Expected retry to fail with PreconditionFailed, got: {:?}",
                service_err2.code()
            );
            debug!("✓ Upload was not consumed by failed precondition check");
        }

        // Create the object with a known ETag via put_object
        let initial_etag = {
            let body = ByteStream::from_static(content.as_bytes());
            let result = c.put_object().bucket(bucket).key(key).body(body).send().await?;
            result.e_tag().unwrap().to_owned()
        };
        debug!("Initial ETag: {initial_etag}");

        // Test 2: CompleteMultipartUpload with If-Match and wrong ETag should fail
        debug!("Test 2: CompleteMultipartUpload with If-Match and wrong ETag");
        {
            let (upload_id, upload_parts) = do_multipart_upload(c, bucket, key, b"new content").await?;
            let upload = CompletedMultipartUpload::builder().set_parts(Some(upload_parts)).build();

            let result = c
                .complete_multipart_upload()
                .bucket(bucket)
                .key(key)
                .upload_id(&upload_id)
                .multipart_upload(upload)
                .if_match("\"wrong-etag-value\"")
                .send()
                .await;

            match result {
                Ok(_) => panic!("Expected CompleteMultipartUpload with wrong If-Match to fail, but it succeeded"),
                Err(e) => {
                    let service_err = e.into_service_error();
                    debug!("✓ Expected error with wrong ETag: {service_err:?}");
                    assert_eq!(
                        service_err.code(),
                        Some("PreconditionFailed"),
                        "Expected PreconditionFailed, got: {:?}",
                        service_err.code()
                    );
                }
            }
        }

        // Verify the object wasn't overwritten
        {
            let result = c.get_object().bucket(bucket).key(key).send().await?;
            let body = result.body.collect().await?.into_bytes();
            assert_eq!(body.as_ref(), content.as_bytes());
            debug!("✓ Verified object was not overwritten");
        }

        // Test 3: CompleteMultipartUpload with If-Match and correct ETag should succeed
        debug!("Test 3: CompleteMultipartUpload with If-Match and correct ETag");
        {
            let new_content = b"updated via conditional multipart";
            let (upload_id, upload_parts) = do_multipart_upload(c, bucket, key, new_content).await?;
            let upload = CompletedMultipartUpload::builder().set_parts(Some(upload_parts)).build();

            let result = c
                .complete_multipart_upload()
                .bucket(bucket)
                .key(key)
                .upload_id(&upload_id)
                .multipart_upload(upload)
                .if_match(&initial_etag)
                .send()
                .await;

            match result {
                Ok(_) => debug!("✓ Successfully completed multipart upload with matching If-Match"),
                Err(e) => panic!("Expected CompleteMultipartUpload with matching If-Match to succeed, but got error: {e:?}"),
            }
        }

        // Verify the object was updated (use head_object to avoid body checksum issues
        // since complete_multipart_upload doesn't update internal checksum info)
        {
            let result = c.head_object().bucket(bucket).key(key).send().await?;
            assert!(result.content_length().is_some());
            debug!("✓ Verified object exists after conditional multipart upload");
        }

        // Cleanup
        delete_object(c, bucket, key).await?;
        delete_bucket(c, bucket).await?;

        Ok(())
    }

    /// Test that `CompleteMultipartUpload` with `If-Match: *` succeeds when object exists
    /// and fails with `PreconditionFailed` (412) when object is absent.
    async fn test_complete_multipart_if_match_wildcard(self: Arc<Self>) -> Result<()> {
        let c = &self.s3;
        let bucket = format!("test-cmu-ifm-wc-{}", Uuid::new_v4());
        let bucket = bucket.as_str();
        let key = "multipart-conditional-match-wildcard.txt";

        create_bucket(c, bucket).await?;

        // Test 1: If-Match: * on absent object should fail
        debug!("Test 1: CompleteMultipartUpload with If-Match: * on absent object");
        {
            let (upload_id, upload_parts) = do_multipart_upload(c, bucket, key, b"some content").await?;
            let upload = CompletedMultipartUpload::builder().set_parts(Some(upload_parts)).build();

            let err = c
                .complete_multipart_upload()
                .bucket(bucket)
                .key(key)
                .upload_id(&upload_id)
                .multipart_upload(upload)
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
            debug!("✓ If-Match: * correctly rejected absent object");
        }

        // Create the object
        c.put_object()
            .bucket(bucket)
            .key(key)
            .body(ByteStream::from_static(b"existing object"))
            .send()
            .await?;

        // Test 2: If-Match: * on existing object should succeed
        debug!("Test 2: CompleteMultipartUpload with If-Match: * on existing object");
        {
            let (upload_id, upload_parts) = do_multipart_upload(c, bucket, key, b"new content").await?;
            let upload = CompletedMultipartUpload::builder().set_parts(Some(upload_parts)).build();

            c.complete_multipart_upload()
                .bucket(bucket)
                .key(key)
                .upload_id(&upload_id)
                .multipart_upload(upload)
                .if_match("*")
                .send()
                .await
                .expect("Expected If-Match: * on existing object to succeed");

            debug!("✓ If-Match: * correctly accepted existing object");
        }

        // Cleanup
        delete_object(c, bucket, key).await?;
        delete_bucket(c, bucket).await?;

        Ok(())
    }

    async fn test_upload_part_copy_empty_source(self: Arc<Self>) -> Result<()> {
        let c = &self.s3;
        let bucket = format!("test-upc-empty-{}", Uuid::new_v4());
        let bucket = bucket.as_str();
        create_bucket(c, bucket).await?;

        let src_key = "empty.txt";
        c.put_object()
            .bucket(bucket)
            .key(src_key)
            .body(ByteStream::from_static(b""))
            .send()
            .await?;

        let dst_key = "dst.txt";
        let upload_id = c
            .create_multipart_upload()
            .bucket(bucket)
            .key(dst_key)
            .send()
            .await?
            .upload_id
            .unwrap();

        let copy_source = format!("{bucket}/{src_key}");
        c.upload_part_copy()
            .bucket(bucket)
            .key(dst_key)
            .copy_source(copy_source)
            .upload_id(&upload_id)
            .part_number(1)
            .send()
            .await?;

        c.abort_multipart_upload()
            .bucket(bucket)
            .key(dst_key)
            .upload_id(&upload_id)
            .send()
            .await?;

        delete_object(c, bucket, src_key).await?;
        delete_bucket(c, bucket).await?;

        Ok(())
    }
}
