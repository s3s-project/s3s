use s3s::auth::SimpleAuth;
use s3s::header::CONTENT_TYPE;
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

use aws_sdk_s3::error::ProvideErrorMetadata;

use anyhow::Result;
use hyper::Method;
use tokio::sync::Mutex;
use tokio::sync::MutexGuard;
use tracing::{debug, error};
use uuid::Uuid;

const FS_ROOT: &str = concat!(env!("CARGO_TARGET_TMPDIR"), "/s3s-fs-tests-aws");
const DOMAIN_NAME: &str = "localhost:8014";
const REGION: &str = "us-west-2";

// STS AssumeRole route that returns NotImplemented
struct AssumeRoleRoute;

#[async_trait::async_trait]
impl s3s::route::S3Route for AssumeRoleRoute {
    fn is_match(&self, method: &Method, uri: &hyper::Uri, headers: &hyper::HeaderMap, _: &mut hyper::http::Extensions) -> bool {
        if method == Method::POST
            && uri.path() == "/"
            && let Some(val) = headers.get(CONTENT_TYPE)
            && val.as_bytes() == b"application/x-www-form-urlencoded"
        {
            return true;
        }
        false
    }

    async fn call(&self, _req: s3s::S3Request<s3s::Body>) -> s3s::S3Result<s3s::S3Response<s3s::Body>> {
        debug!("AssumeRole called - returning NotImplemented");
        Err(s3s::s3_error!(NotImplemented, "STS operations are not supported by s3s-fs"))
    }
}

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
            b.set_route(AssumeRoleRoute);
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

/// Test that multipart uploaded objects have the correct `ETag` format: `{hash}-{part_count}`
#[tokio::test]
#[tracing::instrument]
async fn test_multipart_etag_format() -> Result<()> {
    let _guard = serial().await;

    let c = Client::new(config());

    let bucket = format!("test-multipart-etag-{}", Uuid::new_v4());
    let bucket = bucket.as_str();
    create_bucket(&c, bucket).await?;

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
async fn test_upload_part_copy_invalid_source_range() -> Result<()> {
    let _guard = serial().await;

    let c = Client::new(config());
    let bucket = format!("test-upc-bad-range-{}", Uuid::new_v4());
    let bucket = bucket.as_str();
    create_bucket(&c, bucket).await?;

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

    delete_object(&c, bucket, src_key).await?;
    delete_bucket(&c, bucket).await?;

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

/// Test that demonstrates the Content-Encoding preservation issue
/// Related: <https://github.com/rustfs/rustfs/issues/1062>
#[tokio::test]
#[tracing::instrument]
async fn test_content_encoding_preservation() -> Result<()> {
    let _guard = serial().await;

    let c = Client::new(config());
    let bucket = format!("test-content-encoding-{}", Uuid::new_v4());
    let bucket = bucket.as_str();
    let key = "compressed.json";

    // Simulated Brotli-compressed JSON content
    let content = b"compressed data here";

    create_bucket(&c, bucket).await?;

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
        delete_object(&c, bucket, key).await?;
        delete_bucket(&c, bucket).await?;
    }

    Ok(())
}

/// Test that standard object attributes are preserved through multipart uploads
#[tokio::test]
#[tracing::instrument]
async fn test_multipart_with_attributes() -> Result<()> {
    let _guard = serial().await;

    let c = Client::new(config());
    let bucket = format!("test-multipart-attrs-{}", Uuid::new_v4());
    let bucket = bucket.as_str();
    let key = "multipart-with-attrs.json";

    create_bucket(&c, bucket).await?;

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
        delete_object(&c, bucket, key).await?;
        delete_bucket(&c, bucket).await?;
    }

    Ok(())
}

#[tokio::test]
#[tracing::instrument]
async fn test_sts_assume_role_not_implemented() -> Result<()> {
    let _guard = serial().await;

    // Create STS client using the same config as S3
    let sdk_config = config();
    let sts_client = aws_sdk_sts::Client::new(sdk_config);

    // Attempt to call AssumeRole - should fail with NotImplemented
    let result = sts_client
        .assume_role()
        .role_arn("arn:aws:iam::123456789012:role/test-role")
        .role_session_name("test-session")
        .send()
        .await;

    // Verify the operation returned an error
    assert!(result.is_err(), "Expected AssumeRole to fail with NotImplemented error");

    // Check that the error is NotImplemented
    let error = result.unwrap_err();
    let error_str = format!("{error:?}");
    debug!("AssumeRole error (expected): {error_str}");

    // The error should contain "NotImplemented" or similar indication
    assert!(
        error_str.contains("NotImplemented") || error_str.contains("not implemented"),
        "Expected NotImplemented error, got: {error_str}"
    );

    Ok(())
}

#[tokio::test]
#[tracing::instrument]
async fn test_if_none_match_wildcard() -> Result<()> {
    let _guard = serial().await;

    let c = Client::new(config());
    let bucket = format!("if-none-match-{}", Uuid::new_v4());
    let bucket = bucket.as_str();
    let key = "test-file.txt";
    let content1 = "initial content";
    let content2 = "updated content";

    create_bucket(&c, bucket).await?;

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
    delete_object(&c, bucket, key).await?;
    delete_bucket(&c, bucket).await?;

    Ok(())
}

/// Regression test for <https://github.com/s3s-project/s3s/issues/67>
///
/// `copy_object` should create parent directories when the destination key contains "/"
#[tokio::test]
#[tracing::instrument]
async fn test_copy_object_nested_dst() -> Result<()> {
    let _guard = serial().await;

    let c = Client::new(config());
    let bucket = format!("test-copy-nested-{}", Uuid::new_v4());
    let bucket = bucket.as_str();

    create_bucket(&c, bucket).await?;

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
    delete_object(&c, bucket, src_key).await?;
    delete_object(&c, bucket, dst_key).await?;
    delete_bucket(&c, bucket).await?;

    Ok(())
}

/// Regression test for <https://github.com/s3s-project/s3s/issues/112>
///
/// `list_objects_v2` prefix matching should use string-based matching (not `Path::starts_with`)
/// and `start_after` should work correctly
#[tokio::test]
#[tracing::instrument]
async fn test_list_objects_v2_start_after() -> Result<()> {
    let _guard = serial().await;

    let c = Client::new(config());
    let bucket = format!("test-start-after-{}", Uuid::new_v4());
    let bucket = bucket.as_str();
    create_bucket(&c, bucket).await?;

    let content = "test content";
    let keys = ["aaa.txt", "bbb.txt", "ccc.txt", "ddd.txt"];
    for key in &keys {
        c.put_object()
            .bucket(bucket)
            .key(*key)
            .body(ByteStream::from_static(content.as_bytes()))
            .send()
            .await?;
    }

    // start_after="bbb.txt" should return only ccc.txt and ddd.txt
    let result = c.list_objects_v2().bucket(bucket).start_after("bbb.txt").send().await?;

    let contents: Vec<_> = result.contents().iter().filter_map(|obj| obj.key()).collect();
    assert_eq!(contents, vec!["ccc.txt", "ddd.txt"]);

    // Cleanup
    for key in &keys {
        delete_object(&c, bucket, key).await?;
    }
    delete_bucket(&c, bucket).await?;

    Ok(())
}

/// Regression test for <https://github.com/s3s-project/s3s/issues/112>
///
/// Prefix matching must use string comparison, not `Path::starts_with` which is stricter.
/// For example, prefix "dir/sub" should match key "dir/subdir/file.txt".
#[tokio::test]
#[tracing::instrument]
async fn test_list_objects_v2_prefix_string_matching() -> Result<()> {
    let _guard = serial().await;

    let c = Client::new(config());
    let bucket = format!("test-prefix-match-{}", Uuid::new_v4());
    let bucket = bucket.as_str();
    create_bucket(&c, bucket).await?;

    let content = "test";
    let keys = ["dir/subdir/file1.txt", "dir/subother/file2.txt", "dir/other/file3.txt"];
    for key in &keys {
        c.put_object()
            .bucket(bucket)
            .key(*key)
            .body(ByteStream::from_static(content.as_bytes()))
            .send()
            .await?;
    }

    // Prefix "dir/sub" should match "dir/subdir/..." and "dir/subother/..."
    // but NOT "dir/other/..."
    // Path::starts_with would fail here because it requires component boundaries
    let result = c.list_objects_v2().bucket(bucket).prefix("dir/sub").send().await?;

    let contents: Vec<_> = result.contents().iter().filter_map(|obj| obj.key()).collect();
    assert_eq!(contents.len(), 2, "Expected 2 objects matching prefix 'dir/sub', got {contents:?}");
    assert!(contents.contains(&"dir/subdir/file1.txt"));
    assert!(contents.contains(&"dir/subother/file2.txt"));

    // Cleanup
    for key in &keys {
        delete_object(&c, bucket, key).await?;
    }
    delete_bucket(&c, bucket).await?;

    Ok(())
}

/// Regression test for <https://github.com/s3s-project/s3s/issues/116>
///
/// `put_object` should write atomically via a temp file to prevent incomplete writes.
/// Verify that the file is fully written and readable after `put_object` completes.
#[tokio::test]
#[tracing::instrument]
async fn test_put_object_atomic_write() -> Result<()> {
    let _guard = serial().await;

    let c = Client::new(config());
    let bucket = format!("test-atomic-write-{}", Uuid::new_v4());
    let bucket = bucket.as_str();
    create_bucket(&c, bucket).await?;

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
    delete_object(&c, bucket, key).await?;
    delete_bucket(&c, bucket).await?;

    Ok(())
}

/// Regression test for <https://github.com/s3s-project/s3s/issues/51>
///
/// Multipart `upload_id` should be bound to the credentials that created it.
/// A different user should not be able to upload parts or complete the upload.
#[tokio::test]
#[tracing::instrument]
#[allow(clippy::too_many_lines)]
async fn test_multipart_upload_id_auth() -> Result<()> {
    let _guard = serial().await;

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

#[tokio::test]
#[tracing::instrument]
async fn test_list_objects_v2_continuation_token_pagination() -> Result<()> {
    let _guard = serial().await;

    let c = Client::new(config());
    let bucket = format!("test-continuation-{}", Uuid::new_v4());
    let bucket = bucket.as_str();
    create_bucket(&c, bucket).await?;

    let keys = ["a.txt", "b.txt", "c.txt", "d.txt", "e.txt"];
    for key in &keys {
        c.put_object()
            .bucket(bucket)
            .key(*key)
            .body(ByteStream::from_static(b"x"))
            .send()
            .await?;
    }

    // Walk all pages with max_keys=2
    let mut all_keys: Vec<String> = Vec::new();
    let mut token: Option<String> = None;
    loop {
        let mut req = c.list_objects_v2().bucket(bucket).max_keys(2);
        if let Some(t) = &token {
            req = req.continuation_token(t.clone());
        }
        let page = req.send().await?;

        all_keys.extend(page.contents().iter().filter_map(|o| o.key().map(String::from)));

        if page.is_truncated() != Some(true) {
            break;
        }
        token = page.next_continuation_token().map(String::from);
        assert!(token.is_some(), "is_truncated is true but next_continuation_token is missing");
    }

    assert_eq!(all_keys, vec!["a.txt", "b.txt", "c.txt", "d.txt", "e.txt"]);

    // Cleanup
    for key in &keys {
        delete_object(&c, bucket, key).await?;
    }
    delete_bucket(&c, bucket).await?;

    Ok(())
}

/// When both `continuation_token` and `start_after` are present, the stricter
/// (larger) bound wins so we never re-list keys the caller already skipped.
#[tokio::test]
#[tracing::instrument]
async fn test_list_objects_v2_continuation_token_and_start_after_uses_max() -> Result<()> {
    let _guard = serial().await;

    let c = Client::new(config());
    let bucket = format!("test-ct-max-{}", Uuid::new_v4());
    let bucket = bucket.as_str();
    create_bucket(&c, bucket).await?;

    let keys = ["a.txt", "b.txt", "c.txt", "d.txt", "e.txt"];
    for key in &keys {
        c.put_object()
            .bucket(bucket)
            .key(*key)
            .body(ByteStream::from_static(b"x"))
            .send()
            .await?;
    }

    // start_after="d.txt" is larger than continuation_token="b.txt", so we resume after d.txt
    let result = c
        .list_objects_v2()
        .bucket(bucket)
        .continuation_token("b.txt")
        .start_after("d.txt")
        .send()
        .await?;
    let result_keys: Vec<_> = result.contents().iter().filter_map(|o| o.key()).collect();
    assert_eq!(result_keys, vec!["e.txt"], "should resume after the larger value (d.txt)");

    // continuation_token="c.txt" is larger than start_after="a.txt", so we resume after c.txt
    let result = c
        .list_objects_v2()
        .bucket(bucket)
        .continuation_token("c.txt")
        .start_after("a.txt")
        .send()
        .await?;
    let result_keys: Vec<_> = result.contents().iter().filter_map(|o| o.key()).collect();
    assert_eq!(result_keys, vec!["d.txt", "e.txt"], "should resume after the larger value (c.txt)");

    // Cleanup
    for key in &keys {
        delete_object(&c, bucket, key).await?;
    }

    delete_bucket(&c, bucket).await?;

    Ok(())
}

/// `max_keys=0` return `is_truncated=false` with no continuation token
#[tokio::test]
#[tracing::instrument]
async fn test_list_objects_v2_max_keys_zero() -> Result<()> {
    let _guard = serial().await;

    let c = Client::new(config());
    let bucket = format!("test-max-keys-zero-{}", Uuid::new_v4());
    let bucket = bucket.as_str();
    create_bucket(&c, bucket).await?;

    let keys = ["a.txt", "b.txt", "c.txt"];
    for key in &keys {
        c.put_object()
            .bucket(bucket)
            .key(*key)
            .body(ByteStream::from_static(b"x"))
            .send()
            .await?;
    }

    let result = c.list_objects_v2().bucket(bucket).max_keys(0).send().await?;

    assert_eq!(result.is_truncated(), Some(false), "max_keys=0 should not be truncated");
    assert!(
        result.next_continuation_token().is_none(),
        "max_keys=0 should not return a continuation token"
    );
    assert!(result.contents().is_empty(), "max_keys=0 should return no objects");

    // Cleanup
    for key in &keys {
        delete_object(&c, bucket, key).await?;
    }
    delete_bucket(&c, bucket).await?;

    Ok(())
}

#[tokio::test]
#[tracing::instrument]
async fn test_head_object_no_such_key() -> Result<()> {
    let _guard = serial().await;

    let c = Client::new(config());
    let bucket = format!("test-head-no-such-key-{}", Uuid::new_v4());
    let bucket = bucket.as_str();
    create_bucket(&c, bucket).await?;

    let result = c.head_object().bucket(bucket).key("nonexistent-object").send().await;
    let err = result.expect_err("Expected NoSuchKey for missing object");
    let service_err = err.into_service_error();
    assert_eq!(service_err.code(), Some("NoSuchKey"), "Expected NoSuchKey, got: {:?}", service_err.code());

    delete_bucket(&c, bucket).await?;
    Ok(())
}

#[tokio::test]
#[tracing::instrument]
async fn test_head_object_no_such_bucket() -> Result<()> {
    let _guard = serial().await;

    let c = Client::new(config());
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

#[tokio::test]
#[tracing::instrument]
async fn test_upload_part_copy_empty_source() -> Result<()> {
    let _guard = serial().await;

    let c = Client::new(config());
    let bucket = format!("test-upc-empty-{}", Uuid::new_v4());
    let bucket = bucket.as_str();
    create_bucket(&c, bucket).await?;

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

    delete_object(&c, bucket, src_key).await?;
    delete_bucket(&c, bucket).await?;

    Ok(())
}

#[tokio::test]
#[tracing::instrument]
async fn test_head_object_etag_and_checksum() -> Result<()> {
    let _guard = serial().await;

    let c = Client::new(config());
    let bucket = format!("test-head-etag-{}", Uuid::new_v4());
    let bucket = bucket.as_str();
    let key = "sample.txt";
    let content = "hello world\n";
    let crc32c = base64_simd::STANDARD.encode_to_string(crc32c::crc32c(content.as_bytes()).to_be_bytes());

    create_bucket(&c, bucket).await?;

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
    delete_object(&c, bucket, key).await?;
    delete_bucket(&c, bucket).await?;

    Ok(())
}

/// Test conditional copy with `x-amz-copy-source-if-match`
#[tokio::test]
#[tracing::instrument]
async fn test_copy_object_if_match() -> Result<()> {
    use aws_sdk_s3::primitives::DateTime;
    use aws_sdk_s3::primitives::DateTimeFormat;

    let _guard = serial().await;

    let c = Client::new(config());
    let bucket = format!("test-cond-copy-{}", Uuid::new_v4());
    let bucket = bucket.as_str();
    create_bucket(&c, bucket).await?;

    let src_key = "source.txt";
    let content = "conditional copy content";
    c.put_object()
        .bucket(bucket)
        .key(src_key)
        .body(ByteStream::from_static(content.as_bytes()))
        .send()
        .await?;

    let get_result = c.get_object().bucket(bucket).key(src_key).send().await?;
    let etag = get_result.e_tag().expect("get_object should return e_tag").to_owned();
    let _ = get_result.body.collect().await?;

    let copy_source = format!("{bucket}/{src_key}");

    let dst_key = "dest-match.txt";
    c.copy_object()
        .bucket(bucket)
        .key(dst_key)
        .copy_source(&copy_source)
        .copy_source_if_match(&etag)
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

    delete_object(&c, bucket, src_key).await?;
    delete_object(&c, bucket, dst_key).await?;
    delete_object(&c, bucket, dst_key2).await?;
    delete_bucket(&c, bucket).await?;

    Ok(())
}

/// Test conditional copy with `x-amz-copy-source-if-none-match`
#[tokio::test]
#[tracing::instrument]
async fn test_copy_object_if_none_match() -> Result<()> {
    use aws_sdk_s3::primitives::DateTime;
    use aws_sdk_s3::primitives::DateTimeFormat;

    let _guard = serial().await;

    let c = Client::new(config());
    let bucket = format!("test-cond-copy-nm-{}", Uuid::new_v4());
    let bucket = bucket.as_str();
    create_bucket(&c, bucket).await?;

    let src_key = "source.txt";
    let content = "conditional copy none match";
    c.put_object()
        .bucket(bucket)
        .key(src_key)
        .body(ByteStream::from_static(content.as_bytes()))
        .send()
        .await?;

    let get_result = c.get_object().bucket(bucket).key(src_key).send().await?;
    let etag = get_result.e_tag().expect("get_object should return e_tag").to_owned();
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
        .copy_source_if_none_match(&etag)
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

    delete_object(&c, bucket, src_key).await?;
    delete_object(&c, bucket, dst_key).await?;
    delete_object(&c, bucket, dst_key4).await?;
    delete_bucket(&c, bucket).await?;

    Ok(())
}

/// Test conditional copy with `x-amz-copy-source-if-modified-since` and `x-amz-copy-source-if-unmodified-since`
#[tokio::test]
#[tracing::instrument]
async fn test_copy_object_if_modified_since() -> Result<()> {
    use aws_sdk_s3::primitives::DateTime;
    use aws_sdk_s3::primitives::DateTimeFormat;

    let _guard = serial().await;

    let c = Client::new(config());
    let bucket = format!("test-cond-copy-ts-{}", Uuid::new_v4());
    let bucket = bucket.as_str();
    create_bucket(&c, bucket).await?;

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

    delete_object(&c, bucket, src_key).await?;
    delete_object(&c, bucket, dst_key).await?;
    delete_object(&c, bucket, dst_key3).await?;
    delete_bucket(&c, bucket).await?;

    Ok(())
}

/// Test conditional copy against a multipart source object's persisted `ETag`.
#[tokio::test]
#[tracing::instrument]
async fn test_copy_object_conditional_with_multipart_source_etag() -> Result<()> {
    let _guard = serial().await;

    let c = Client::new(config());
    let bucket = format!("test-cond-copy-multipart-{}", Uuid::new_v4());
    let bucket = bucket.as_str();
    create_bucket(&c, bucket).await?;

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
    let multipart_etag = complete_result
        .e_tag()
        .expect("complete_multipart_upload should return e_tag")
        .to_owned();

    let copy_source = format!("{bucket}/{src_key}");

    let dst_key = "dest-multipart-match.txt";
    c.copy_object()
        .bucket(bucket)
        .key(dst_key)
        .copy_source(&copy_source)
        .copy_source_if_match(&multipart_etag)
        .send()
        .await?;

    let ans = c.get_object().bucket(bucket).key(dst_key).send().await?;
    let body = ans.body.collect().await?.into_bytes();
    assert_eq!(body.as_ref(), content.as_bytes());

    let dst_key2 = "dest-multipart-none-match.txt";
    let err = c
        .copy_object()
        .bucket(bucket)
        .key(dst_key2)
        .copy_source(&copy_source)
        .copy_source_if_none_match(&multipart_etag)
        .send()
        .await
        .expect_err("Expected matching multipart ETag to fail If-None-Match");
    let service_err = err.into_service_error();
    assert_eq!(service_err.code(), Some("PreconditionFailed"));

    delete_object(&c, bucket, src_key).await?;
    delete_object(&c, bucket, dst_key).await?;
    delete_bucket(&c, bucket).await?;

    Ok(())
}
