// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

use crate::case;
use crate::suite::{DOMAIN_NAME, FS_ROOT, Object, REGION, create_bucket, delete_bucket, delete_object};

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use aws_sdk_s3::Client;
use aws_sdk_s3::config::Credentials;
use aws_sdk_s3::config::Region;
use aws_sdk_s3::error::ProvideErrorMetadata;
use aws_sdk_s3::primitives::ByteStream;
use aws_sdk_s3::types::ChecksumMode;

use aws_config::SdkConfig;
use aws_credential_types::provider::SharedCredentialsProvider;

use s3s::auth::SimpleAuth;
use s3s::host::SingleDomain;
use s3s::service::S3ServiceBuilder;
use s3s_fs::FileSystem;

use s3s::crypto::Checksum as _;
use s3s::crypto::Crc32;
use s3s::crypto::Md5;

use s3s_test::Result;
use s3s_test::tcx::TestContext;
use uuid::Uuid;

pub fn register(tcx: &mut TestContext) {
    case!(tcx, FsServer, Object, test_missing_etag_is_hashed_and_remembered);
    case!(tcx, FsServer, Object, test_remembered_etag_is_served_without_hashing_again);
    case!(tcx, FsServer, Object, test_backfill_keeps_stored_checksums);
    case!(tcx, FsServer, Object, test_copy_source_without_etag_is_remembered);
    case!(tcx, FsServer, Object, test_conditional_put_uses_computed_etag);
    #[cfg(unix)]
    case!(tcx, FsServer, Object, test_backfill_write_failure_leaves_the_response_alone);
}

/// Put an object into the served root the way an import does: content, no metadata sidecar.
fn import_object(bucket: &str, key: &str, content: &[u8]) -> Result<()> {
    let path = object_path(bucket, key);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, content)?;
    assert!(
        !internal_info_path(bucket, key).exists(),
        "an imported object must start without an internal.json"
    );
    Ok(())
}

fn object_path(bucket: &str, key: &str) -> std::path::PathBuf {
    Path::new(FS_ROOT).join(bucket).join(key)
}

fn internal_info_path(bucket: &str, key: &str) -> std::path::PathBuf {
    let encode = |s: &str| base64_simd::URL_SAFE_NO_PAD.encode_to_string(s);
    Path::new(FS_ROOT).join(format!(".bucket-{}.object-{}.internal.json", encode(bucket), encode(key)))
}

fn read_internal_info(bucket: &str, key: &str) -> Result<serde_json::Map<String, serde_json::Value>> {
    let content = std::fs::read(internal_info_path(bucket, key))?;
    Ok(serde_json::from_slice(&content)?)
}

fn write_internal_info(bucket: &str, key: &str, info: &serde_json::Map<String, serde_json::Value>) -> Result<()> {
    std::fs::write(internal_info_path(bucket, key), serde_json::to_vec(info)?)?;
    Ok(())
}

fn stored_e_tag(bucket: &str, key: &str) -> Result<Option<String>> {
    let info = read_internal_info(bucket, key)?;
    Ok(info.get("e_tag").and_then(|value| value.as_str()).map(str::to_owned))
}

/// The `ETag` header carries the value quoted, so responses have to be compared in that form.
fn quoted(etag: &str) -> String {
    format!("\"{etag}\"")
}

fn md5_hex(content: &[u8]) -> String {
    hex_simd::encode_to_string(Md5::checksum(content), hex_simd::AsciiCase::Lower)
}

impl Object {
    async fn test_missing_etag_is_hashed_and_remembered(self: Arc<Self>) -> Result<()> {
        let c = &self.s3;
        let bucket = format!("test-etag-remember-{}", Uuid::new_v4());
        let bucket = bucket.as_str();
        let key = "imported.bin";
        let content = b"imported body without any metadata sidecar";

        create_bucket(c, bucket).await?;
        import_object(bucket, key, content)?;
        let expected = md5_hex(content);

        let head = c.head_object().bucket(bucket).key(key).send().await?;
        assert_eq!(head.e_tag(), Some(quoted(&expected).as_str()), "HEAD must hash the imported object");

        assert_eq!(
            stored_e_tag(bucket, key)?.as_deref(),
            Some(expected.as_str()),
            "the computed ETag must be remembered in internal.json"
        );

        let get = c.get_object().bucket(bucket).key(key).send().await?;
        assert_eq!(get.e_tag(), Some(quoted(&expected).as_str()), "GET must serve the remembered ETag");
        assert_eq!(get.body.collect().await?.into_bytes().as_ref(), content);

        delete_object(c, bucket, key).await?;
        delete_bucket(c, bucket).await?;

        Ok(())
    }

    async fn test_remembered_etag_is_served_without_hashing_again(self: Arc<Self>) -> Result<()> {
        let c = &self.s3;
        let bucket = format!("test-etag-reuse-{}", Uuid::new_v4());
        let bucket = bucket.as_str();
        let key = "imported-reuse.bin";
        let content = b"content that gets remembered";
        let other_content = b"different content written behind the server's back";

        create_bucket(c, bucket).await?;
        import_object(bucket, key, content)?;
        let remembered = md5_hex(content);

        let first = c.head_object().bucket(bucket).key(key).send().await?;
        assert_eq!(first.e_tag(), Some(quoted(&remembered).as_str()));

        // Rewriting the sidecar goes through a temporary file and a rename, so its modification time
        // moves. Waiting first keeps a same-tick rewrite from hiding behind an equal timestamp.
        tokio::time::sleep(Duration::from_millis(20)).await;
        let sidecar = internal_info_path(bucket, key);
        let first_modified = std::fs::metadata(&sidecar)?.modified()?;

        let second = c.head_object().bucket(bucket).key(key).send().await?;
        assert_eq!(second.e_tag(), Some(quoted(&remembered).as_str()));
        assert_eq!(
            std::fs::metadata(&sidecar)?.modified()?,
            first_modified,
            "a later request must not rewrite the sidecar"
        );

        // The remembered ETag answers for the object from here on, so no request hashes the body
        // again: replacing the file behind the server's back cannot change what is served. The two
        // bodies hash differently, so serving the old ETag proves the bytes were never re-read.
        std::fs::write(object_path(bucket, key), other_content)?;
        assert_ne!(remembered, md5_hex(other_content), "the check below needs two different digests");

        let third = c.head_object().bucket(bucket).key(key).send().await?;
        assert_eq!(
            third.e_tag(),
            Some(quoted(&remembered).as_str()),
            "the remembered ETag must be served without reading the body again"
        );
        assert_eq!(
            std::fs::metadata(&sidecar)?.modified()?,
            first_modified,
            "serving a remembered ETag must not rewrite the sidecar"
        );

        delete_object(c, bucket, key).await?;
        delete_bucket(c, bucket).await?;

        Ok(())
    }

    async fn test_backfill_keeps_stored_checksums(self: Arc<Self>) -> Result<()> {
        let c = &self.s3;
        let bucket = format!("test-etag-checksum-{}", Uuid::new_v4());
        let bucket = bucket.as_str();
        let key = "imported-with-checksum.bin";
        let content = b"imported body that already has a stored checksum";

        create_bucket(c, bucket).await?;
        import_object(bucket, key, content)?;

        let crc32 = base64_simd::STANDARD.encode_to_string(Crc32::checksum(content));
        let mut info = serde_json::Map::new();
        info.insert("checksum_crc32".to_owned(), serde_json::Value::String(crc32.clone()));
        write_internal_info(bucket, key, &info)?;

        let expected = md5_hex(content);
        let head = c
            .head_object()
            .bucket(bucket)
            .key(key)
            .checksum_mode(ChecksumMode::Enabled)
            .send()
            .await?;
        assert_eq!(head.e_tag(), Some(quoted(&expected).as_str()));
        assert_eq!(
            head.checksum_crc32(),
            Some(crc32.as_str()),
            "the stored checksum must still be served after the backfill"
        );

        let stored = read_internal_info(bucket, key)?;
        assert_eq!(
            stored.get("e_tag").and_then(|value| value.as_str()),
            Some(expected.as_str()),
            "the backfill must add the ETag"
        );
        assert_eq!(
            stored.get("checksum_crc32").and_then(|value| value.as_str()),
            Some(crc32.as_str()),
            "the backfill must keep the checksum that was already stored"
        );

        delete_object(c, bucket, key).await?;
        delete_bucket(c, bucket).await?;

        Ok(())
    }

    async fn test_copy_source_without_etag_is_remembered(self: Arc<Self>) -> Result<()> {
        let c = &self.s3;
        let bucket = format!("test-etag-copy-{}", Uuid::new_v4());
        let bucket = bucket.as_str();
        let src_key = "imported-source.bin";
        let dst_key = "copied.bin";
        let content = b"imported source content for a copy";

        create_bucket(c, bucket).await?;
        import_object(bucket, src_key, content)?;
        let expected = md5_hex(content);

        // The copy source condition forces the copy path to derive the source ETag.
        let copy_source = format!("{bucket}/{src_key}");
        let ans = c
            .copy_object()
            .bucket(bucket)
            .key(dst_key)
            .copy_source(copy_source)
            .copy_source_if_match(quoted(&expected))
            .send()
            .await?;
        assert_eq!(
            ans.copy_object_result().and_then(|result| result.e_tag()),
            Some(quoted(&expected).as_str())
        );

        assert_eq!(
            stored_e_tag(bucket, src_key)?.as_deref(),
            Some(expected.as_str()),
            "the copy path must remember the source ETag it computed"
        );
        assert_eq!(
            stored_e_tag(bucket, dst_key)?.as_deref(),
            Some(expected.as_str()),
            "the copy must store the destination ETag"
        );

        let get = c.get_object().bucket(bucket).key(dst_key).send().await?;
        assert_eq!(get.body.collect().await?.into_bytes().as_ref(), content);

        delete_object(c, bucket, dst_key).await?;
        delete_object(c, bucket, src_key).await?;
        delete_bucket(c, bucket).await?;

        Ok(())
    }

    async fn test_conditional_put_uses_computed_etag(self: Arc<Self>) -> Result<()> {
        let c = &self.s3;
        let bucket = format!("test-etag-conditional-{}", Uuid::new_v4());
        let bucket = bucket.as_str();
        let key = "imported-conditional.bin";
        let original = b"original content";
        let replacement = b"replacement content";

        create_bucket(c, bucket).await?;
        import_object(bucket, key, original)?;
        let expected = md5_hex(original);

        // The rejected write leaves the object untouched, so the ETag the check computed is visible.
        let err = c
            .put_object()
            .bucket(bucket)
            .key(key)
            .if_match("00000000000000000000000000000000")
            .body(ByteStream::from_static(replacement))
            .send()
            .await
            .expect_err("a mismatching If-Match must reject the write");
        assert_eq!(err.into_service_error().code(), Some("PreconditionFailed"));
        assert_eq!(
            std::fs::read(object_path(bucket, key))?,
            original,
            "a rejected conditional write must leave the object alone"
        );
        assert_eq!(
            stored_e_tag(bucket, key)?.as_deref(),
            Some(expected.as_str()),
            "the conditional path must remember the ETag it computed"
        );

        let put = c
            .put_object()
            .bucket(bucket)
            .key(key)
            .if_match(quoted(&expected))
            .body(ByteStream::from_static(replacement))
            .send()
            .await?;
        assert_eq!(put.e_tag(), Some(quoted(&md5_hex(replacement)).as_str()));

        let get = c.get_object().bucket(bucket).key(key).send().await?;
        assert_eq!(get.body.collect().await?.into_bytes().as_ref(), replacement);

        delete_object(c, bucket, key).await?;
        delete_bucket(c, bucket).await?;

        Ok(())
    }

    /// A backfill that cannot write must not change the response.
    #[cfg(unix)]
    async fn test_backfill_write_failure_leaves_the_response_alone(self: Arc<Self>) -> Result<()> {
        use std::os::unix::fs::PermissionsExt;

        // The served root is shared by every case, so this one serves its own root and can make it
        // read-only without disturbing the others.
        let root = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("s3s-fs-etag-failure-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&root)?;
        let file_system = FileSystem::new(&root).expect("the test root must be usable");

        let cred = Credentials::for_tests();
        let service = {
            let mut b = S3ServiceBuilder::new(file_system);
            b.set_auth(SimpleAuth::from_single(cred.access_key_id(), cred.secret_access_key()));
            b.set_host(SingleDomain::new(DOMAIN_NAME).unwrap());
            b.build()
        };
        let config = SdkConfig::builder()
            .credentials_provider(SharedCredentialsProvider::new(cred))
            .http_client(s3s_aws::Client::from(service))
            .region(Region::new(REGION))
            .endpoint_url(format!("http://{DOMAIN_NAME}"))
            .build();
        let c = Client::new(&config);

        let bucket = format!("test-etag-failure-{}", Uuid::new_v4());
        let bucket = bucket.as_str();
        let key = "imported-blocked.bin";
        let content = b"imported body whose sidecar cannot be written";

        create_bucket(&c, bucket).await?;
        std::fs::write(root.join(bucket).join(key), content)?;
        let sidecar = root.join(format!(
            ".bucket-{}.object-{}.internal.json",
            base64_simd::URL_SAFE_NO_PAD.encode_to_string(bucket),
            base64_simd::URL_SAFE_NO_PAD.encode_to_string(key)
        ));

        // A read-only root still serves reads but refuses the temporary file the atomic write needs.
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o555))?;
        let probe = root.join(".permission-probe");
        if std::fs::File::create(&probe).is_ok() {
            // This user bypasses file permissions, so the write cannot be made to fail.
            std::fs::remove_file(&probe)?;
            std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o755))?;
            std::fs::remove_dir_all(&root)?;
            return Ok(());
        }

        let expected = md5_hex(content);
        let head = c.head_object().bucket(bucket).key(key).send().await?;
        assert_eq!(
            head.e_tag(),
            Some(quoted(&expected).as_str()),
            "a backfill that cannot write must not change the ETag"
        );
        assert!(!sidecar.exists(), "a failed backfill must not leave a sidecar behind");

        let get = c.get_object().bucket(bucket).key(key).send().await?;
        assert_eq!(get.body.collect().await?.into_bytes().as_ref(), content);

        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o755))?;
        std::fs::remove_dir_all(&root)?;

        Ok(())
    }
}
