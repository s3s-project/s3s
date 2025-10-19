use s3s::auth::SimpleAuth;
use s3s::host::SingleDomain;
use s3s::service::S3ServiceBuilder;
use s3s::validation::NameValidation;
use s3s_fs::FileSystem;

use std::env;
use std::fs;

use aws_config::SdkConfig;
use aws_credential_types::provider::SharedCredentialsProvider;
use aws_sdk_s3::Client;
use aws_sdk_s3::config::Credentials;
use aws_sdk_s3::config::Region;
use aws_sdk_s3::primitives::ByteStream;

use aws_sdk_s3::types::BucketLocationConstraint;
use aws_sdk_s3::types::ChecksumMode;
use aws_sdk_s3::types::CompletedMultipartUpload;
use aws_sdk_s3::types::CompletedPart;
use aws_sdk_s3::types::CreateBucketConfiguration;

use anyhow::Result;
use tokio::sync::Mutex;
use tokio::sync::MutexGuard;
use tracing::{debug, error};
use uuid::Uuid;

const FS_ROOT: &str = concat!(env!("CARGO_TARGET_TMPDIR"), "/s3s-fs-tests-aws");
const DOMAIN_NAME: &str = "localhost:8014";
const REGION: &str = "us-west-2";

fn setup_tracing() {
    use tracing_subscriber::EnvFilter;

    // if env::var("RUST_LOG").is_err() {
    //     // TODO: Audit that the environment access only happens in single-threaded code.
    //     unsafe { env::set_var("RUST_LOG", "it_aws=debug,s3s_fs=debug,s3s=debug") };
    // }

    tracing_subscriber::fmt()
        .pretty()
        .with_env_filter(EnvFilter::from_default_env())
        .with_test_writer()
        .init();
}

fn config() -> &'static SdkConfig {
    use std::sync::LazyLock;
    static CONFIG: LazyLock<SdkConfig> = LazyLock::new(|| {
        setup_tracing();

        // Fake credentials
        let cred = Credentials::for_tests();

        // Setup S3 provider
        fs::create_dir_all(FS_ROOT).unwrap();
        let fs = FileSystem::new(FS_ROOT).unwrap();

        // Setup S3 service
        let service = {
            let mut b = S3ServiceBuilder::new(fs);
            b.set_auth(SimpleAuth::from_single(cred.access_key_id(), cred.secret_access_key()));
            b.set_host(SingleDomain::new(DOMAIN_NAME).unwrap());
            b.build()
        };

        // Convert to aws http client
        let client = s3s_aws::Client::from(service);

        // Setup aws sdk config
        SdkConfig::builder()
            .credentials_provider(SharedCredentialsProvider::new(cred))
            .http_client(client)
            .region(Region::new(REGION))
            .endpoint_url(format!("http://{DOMAIN_NAME}"))
            .build()
    });
    &CONFIG
}

async fn serial() -> MutexGuard<'static, ()> {
    use std::sync::LazyLock;
    static LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));
    LOCK.lock().await
}

fn create_client_with_validation(validation: impl NameValidation + 'static) -> Client {
    // Setup with custom validation
    let service = {
        let fs = FileSystem::new(FS_ROOT).unwrap();
        let mut b = S3ServiceBuilder::new(fs);
        let cred = Credentials::for_tests();
        b.set_auth(SimpleAuth::from_single(cred.access_key_id(), cred.secret_access_key()));
        b.set_host(SingleDomain::new(DOMAIN_NAME).unwrap());
        b.set_validation(validation);
        b.build()
    };

    // Convert to aws http client
    let client_inner = s3s_aws::Client::from(service);

    // Setup aws sdk config
    let cred = Credentials::for_tests();
    let config = SdkConfig::builder()
        .credentials_provider(SharedCredentialsProvider::new(cred))
        .http_client(client_inner)
        .region(Region::new(REGION))
        .endpoint_url(format!("http://{DOMAIN_NAME}"))
        .build();

    Client::new(&config)
}

async fn create_bucket(c: &Client, bucket: &str) -> Result<()> {
    let location = BucketLocationConstraint::from(REGION);
    let cfg = CreateBucketConfiguration::builder().location_constraint(location).build();

    c.create_bucket()
        .create_bucket_configuration(cfg)
        .bucket(bucket)
        .send()
        .await?;

    debug!("created bucket: {bucket:?}");
    Ok(())
}

async fn delete_object(c: &Client, bucket: &str, key: &str) -> Result<()> {
    c.delete_object().bucket(bucket).key(key).send().await?;
    Ok(())
}

async fn delete_bucket(c: &Client, bucket: &str) -> Result<()> {
    c.delete_bucket().bucket(bucket).send().await?;
    Ok(())
}

macro_rules! log_and_unwrap {
    ($result:expr) => {
        match $result {
            Ok(ans) => {
                debug!(?ans);
                ans
            }
            Err(err) => {
                error!(?err);
                return Err(err.into());
            }
        }
    };
}

#[tokio::test]
#[tracing::instrument]
async fn test_list_buckets() -> Result<()> {
    let _guard = serial().await;

    let c = Client::new(config());
    let response1 = log_and_unwrap!(c.list_buckets().send().await);
    drop(response1);

    let bucket1 = format!("test-list-buckets-1-{}", Uuid::new_v4());
    let bucket1_str = bucket1.as_str();
    let bucket2 = format!("test-list-buckets-2-{}", Uuid::new_v4());
    let bucket2_str = bucket2.as_str();

    create_bucket(&c, bucket1_str).await?;
    create_bucket(&c, bucket2_str).await?;

    let response2 = log_and_unwrap!(c.list_buckets().send().await);
    let bucket_names: Vec<_> = response2.buckets().iter().filter_map(|bucket| bucket.name()).collect();
    assert!(bucket_names.contains(&bucket1_str));
    assert!(bucket_names.contains(&bucket2_str));

    Ok(())
}

#[tokio::test]
#[tracing::instrument]
async fn test_list_objects_v2() -> Result<()> {
    let _guard = serial().await;

    let c = Client::new(config());
    let bucket = format!("test-list-objects-v2-{}", Uuid::new_v4());
    let bucket_str = bucket.as_str();
    create_bucket(&c, bucket_str).await?;

    let test_prefix = "/this/is/a/test/";
    let key1 = "this/is/a/test/path/file1.txt";
    let key2 = "this/is/a/test/path/file2.txt";
    {
        let content = "hello world\nनमस्ते दुनिया\n";
        let crc32c = base64_simd::STANDARD.encode_to_string(crc32c::crc32c(content.as_bytes()).to_be_bytes());
        c.put_object()
            .bucket(bucket_str)
            .key(key1)
            .body(ByteStream::from_static(content.as_bytes()))
            .checksum_crc32_c(crc32c.as_str())
            .send()
            .await?;
        c.put_object()
            .bucket(bucket_str)
            .key(key2)
            .body(ByteStream::from_static(content.as_bytes()))
            .checksum_crc32_c(crc32c.as_str())
            .send()
            .await?;
    }

    let result = c.list_objects_v2().bucket(bucket_str).prefix(test_prefix).send().await;

    let response = log_and_unwrap!(result);

    let contents: Vec<_> = response.contents().iter().filter_map(|obj| obj.key()).collect();
    assert!(!contents.is_empty());
    assert!(contents.contains(&key1));
    assert!(contents.contains(&key2));

    Ok(())
}

#[tokio::test]
#[tracing::instrument]
async fn test_list_objects_v2_with_prefixes() -> Result<()> {
    let _guard = serial().await;

    let c = Client::new(config());
    let bucket = format!("test-list-prefixes-{}", Uuid::new_v4());
    let bucket_str = bucket.as_str();
    create_bucket(&c, bucket_str).await?;

    // Create files in nested directory structure
    let content = "hello world\n";
    let files = [
        "README.md",                   // Root level file
        "test/subdirectory/README.md", // Nested file
        "test/file.txt",               // File in test/ directory
        "other/dir/file.txt",          // File in other/dir/ directory
    ];

    for key in &files {
        c.put_object()
            .bucket(bucket_str)
            .key(*key)
            .body(ByteStream::from_static(content.as_bytes()))
            .send()
            .await?;
    }

    // List without delimiter - should return all files recursively
    let result = c.list_objects_v2().bucket(bucket_str).send().await;

    let response = log_and_unwrap!(result);
    let contents: Vec<_> = response.contents().iter().filter_map(|obj| obj.key()).collect();

    debug!("List without delimiter - objects: {:?}", contents);
    assert_eq!(contents.len(), 4);
    for key in &files {
        assert!(contents.contains(key), "Missing key: {key}");
    }

    // List with delimiter "/" - should return root files and common prefixes
    let result = c.list_objects_v2().bucket(bucket_str).delimiter("/").send().await;

    let response = log_and_unwrap!(result);

    // Should have one file at root level
    let contents: Vec<_> = response.contents().iter().filter_map(|obj| obj.key()).collect();
    debug!("List with delimiter - objects: {:?}", contents);
    assert_eq!(contents.len(), 1);
    assert!(contents.contains(&"README.md"));

    // Should have two common prefixes: "test/" and "other/"
    let prefixes: Vec<_> = response.common_prefixes().iter().filter_map(|cp| cp.prefix()).collect();
    debug!("List with delimiter - prefixes: {:?}", prefixes);
    assert_eq!(prefixes.len(), 2);
    assert!(prefixes.contains(&"test/"));
    assert!(prefixes.contains(&"other/"));

    // List with prefix "test/" and delimiter "/" - should return files in test/ and subdirectories
    let result = c
        .list_objects_v2()
        .bucket(bucket_str)
        .prefix("test/")
        .delimiter("/")
        .send()
        .await;

    let response = log_and_unwrap!(result);

    // Should have one file in test/ directory
    let contents: Vec<_> = response.contents().iter().filter_map(|obj| obj.key()).collect();
    debug!("List with prefix test/ - objects: {:?}", contents);
    assert_eq!(contents.len(), 1);
    assert!(contents.contains(&"test/file.txt"));

    // Should have one common prefix: "test/subdirectory/"
    let prefixes: Vec<_> = response.common_prefixes().iter().filter_map(|cp| cp.prefix()).collect();
    debug!("List with prefix test/ - prefixes: {:?}", prefixes);
    assert_eq!(prefixes.len(), 1);
    assert!(prefixes.contains(&"test/subdirectory/"));

    Ok(())
}

#[tokio::test]
#[tracing::instrument]
async fn test_list_objects_v1_with_prefixes() -> Result<()> {
    let _guard = serial().await;

    let c = Client::new(config());
    let bucket = format!("test-list-v1-prefixes-{}", Uuid::new_v4());
    let bucket_str = bucket.as_str();
    create_bucket(&c, bucket_str).await?;

    // Create a simple structure
    let content = "hello world\n";
    let files = ["README.md", "dir/file.txt"];

    for key in &files {
        c.put_object()
            .bucket(bucket_str)
            .key(*key)
            .body(ByteStream::from_static(content.as_bytes()))
            .send()
            .await?;
    }

    // Test list_objects (v1) with delimiter
    let result = c.list_objects().bucket(bucket_str).delimiter("/").send().await;

    let response = log_and_unwrap!(result);

    // Should have one file at root level
    let contents: Vec<_> = response.contents().iter().filter_map(|obj| obj.key()).collect();
    debug!("ListObjects v1 with delimiter - objects: {:?}", contents);
    assert_eq!(contents.len(), 1);
    assert!(contents.contains(&"README.md"));

    // Should have one common prefix: "dir/"
    let prefixes: Vec<_> = response.common_prefixes().iter().filter_map(|cp| cp.prefix()).collect();
    debug!("ListObjects v1 with delimiter - prefixes: {:?}", prefixes);
    assert_eq!(prefixes.len(), 1);
    assert!(prefixes.contains(&"dir/"));

    Ok(())
}

#[tokio::test]
#[tracing::instrument]
async fn test_list_objects_v2_max_keys() -> Result<()> {
    let _guard = serial().await;

    let c = Client::new(config());
    let bucket = format!("test-max-keys-{}", Uuid::new_v4());
    let bucket_str = bucket.as_str();
    create_bucket(&c, bucket_str).await?;

    // Create 10 files
    let content = "test";
    for i in 0..10 {
        let key = format!("file{i:02}.txt");
        c.put_object()
            .bucket(bucket_str)
            .key(key)
            .body(ByteStream::from_static(content.as_bytes()))
            .send()
            .await?;
    }

    // Test max_keys=5
    let result = c.list_objects_v2().bucket(bucket_str).max_keys(5).send().await;
    let response = log_and_unwrap!(result);

    // Should return exactly 5 objects
    let contents: Vec<_> = response.contents().iter().filter_map(|obj| obj.key()).collect();
    assert_eq!(contents.len(), 5, "Expected 5 objects, got {}", contents.len());
    assert_eq!(response.key_count(), Some(5));
    assert_eq!(response.max_keys(), Some(5));
    assert_eq!(response.is_truncated(), Some(true), "Should be truncated");

    // Test max_keys=20 (more than available)
    let result = c.list_objects_v2().bucket(bucket_str).max_keys(20).send().await;
    let response = log_and_unwrap!(result);

    let contents: Vec<_> = response.contents().iter().filter_map(|obj| obj.key()).collect();
    assert_eq!(contents.len(), 10, "Expected 10 objects, got {}", contents.len());
    assert_eq!(response.key_count(), Some(10));
    assert_eq!(response.max_keys(), Some(20));
    assert_eq!(response.is_truncated(), Some(false), "Should not be truncated");

    Ok(())
}

#[tokio::test]
#[tracing::instrument]
async fn test_single_object() -> Result<()> {
    let _guard = serial().await;

    let c = Client::new(config());
    let bucket = format!("test-single-object-{}", Uuid::new_v4());
    let bucket = bucket.as_str();
    let key = "sample.txt";
    let content = "hello world\n你好世界\n";
    let crc32c = base64_simd::STANDARD.encode_to_string(crc32c::crc32c(content.as_bytes()).to_be_bytes());

    create_bucket(&c, bucket).await?;

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
        delete_object(&c, bucket, key).await?;
        delete_bucket(&c, bucket).await?;
    }

    Ok(())
}

#[tokio::test]
#[tracing::instrument]
async fn test_multipart() -> Result<()> {
    let _guard = serial().await;

    let c = Client::new(config());

    let bucket = format!("test-multipart-{}", Uuid::new_v4());
    let bucket = bucket.as_str();
    create_bucket(&c, bucket).await?;

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
        delete_object(&c, bucket, key).await?;
        delete_bucket(&c, bucket).await?;
    }

    Ok(())
}

#[tokio::test]
#[tracing::instrument]
async fn test_upload_part_copy() -> Result<()> {
    let _guard = serial().await;

    let c = Client::new(config());
    let src_bucket = format!("test-copy{}", Uuid::new_v4());
    let src_bucket = src_bucket.as_str();
    let src_key = "copied.txt";
    let src_content = "hello world\nनमस्ते दुनिया\n";
    let crc32c = base64_simd::STANDARD.encode_to_string(crc32c::crc32c(src_content.as_bytes()).to_be_bytes());

    create_bucket(&c, src_bucket).await?;

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
    create_bucket(&c, bucket).await?;

    let key = "sample.txt";

    let upload_id = {
        let ans = c.create_multipart_upload().bucket(bucket).key(key).send().await?;
        ans.upload_id.unwrap()
    };
    let upload_id = upload_id.as_str();
    let src_path = format!("{src_bucket}/{src_key}");
    let upload_parts = {
        let part_number = 1;
        let _ans = c
            .upload_part_copy()
            .bucket(bucket)
            .key(key)
            .copy_source(src_path)
            .upload_id(upload_id)
            .part_number(part_number)
            .send()
            .await?;
        let part = CompletedPart::builder().part_number(part_number).build();
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

        assert_eq!(content_length, src_content.len());
        assert_eq!(body.as_ref(), src_content.as_bytes());
    }
    println!("{key} CK3");
    {
        delete_object(&c, bucket, key).await?;
        delete_bucket(&c, bucket).await?;
        delete_object(&c, src_bucket, src_key).await?;
        delete_bucket(&c, src_bucket).await?;
    }

    Ok(())
}

#[tokio::test]
#[tracing::instrument]
async fn test_single_object_get_range() -> Result<()> {
    let _guard = serial().await;

    let c = Client::new(config());
    let bucket = format!("test-single-object-{}", Uuid::new_v4());
    let bucket = bucket.as_str();
    let key = "sample.txt";
    let content = "hello world\n你好世界\n";
    let crc32c = base64_simd::STANDARD.encode_to_string(crc32c::crc32c(content.as_bytes()).to_be_bytes());

    create_bucket(&c, bucket).await?;

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
        delete_object(&c, bucket, key).await?;
        delete_bucket(&c, bucket).await?;
    }

    Ok(())
}

#[tokio::test]
#[tracing::instrument]
async fn test_relaxed_bucket_validation() -> Result<()> {
    struct RelaxedNameValidation;

    impl NameValidation for RelaxedNameValidation {
        fn validate_bucket_name(&self, name: &str) -> bool {
            !name.is_empty()
        }
    }

    let _guard = serial().await;

    let c = create_client_with_validation(RelaxedNameValidation);

    // Test with bucket names that should pass with relaxed validation
    let relaxed_bucket_names = [
        "UPPERCASE-BUCKET",       // Uppercase not normally allowed
        "bucket_with_underscore", // Underscores not allowed
    ];

    for bucket_name in relaxed_bucket_names {
        let location = BucketLocationConstraint::from(REGION);
        let cfg = CreateBucketConfiguration::builder().location_constraint(location).build();

        let result = c
            .create_bucket()
            .create_bucket_configuration(cfg)
            .bucket(bucket_name)
            .send()
            .await;

        // Should not fail due to bucket name validation
        match result {
            Ok(_) => {
                debug!("Successfully created bucket with relaxed validation: {bucket_name}");

                // Verify the bucket was actually created by checking bucket existence
                let head_result = c.head_bucket().bucket(bucket_name).send().await;
                assert!(head_result.is_ok(), "Failed to head bucket {bucket_name} after creation");

                // Clean up the bucket
                let delete_result = delete_bucket(&c, bucket_name).await;
                assert!(delete_result.is_ok(), "Failed to delete bucket {bucket_name}");
            }
            Err(e) => {
                let error_str = format!("{e:?}");
                debug!("Bucket creation failed for other reasons (expected): {bucket_name} - {error_str}");
                // Verify it's not a bucket name validation error
                assert!(!error_str.contains("InvalidBucketName") && !error_str.contains("bucket name"));
            }
        }
    }

    Ok(())
}

#[tokio::test]
#[tracing::instrument]
async fn test_default_bucket_validation() -> Result<()> {
    let _guard = serial().await;

    let c = Client::new(config()); // Uses default validation

    // Test with invalid bucket names that should be rejected by AWS rules
    let invalid_bucket_names = [
        "UPPERCASE-BUCKET",       // Uppercase not allowed
        "bucket_with_underscore", // Underscores not allowed
        "bucket..double.dots",    // Consecutive dots not allowed
    ];

    for bucket_name in invalid_bucket_names {
        // Try to create bucket with invalid name - should fail with default validation
        let location = BucketLocationConstraint::from(REGION);
        let cfg = CreateBucketConfiguration::builder().location_constraint(location).build();

        let result = c
            .create_bucket()
            .create_bucket_configuration(cfg)
            .bucket(bucket_name)
            .send()
            .await;

        // Should fail due to bucket name validation
        assert!(result.is_err(), "Expected error for invalid bucket name: {bucket_name}");

        let error_str = format!("{:?}", result.unwrap_err());
        debug!("Default validation rejected bucket name {bucket_name}: {error_str}");
    }

    Ok(())
}
