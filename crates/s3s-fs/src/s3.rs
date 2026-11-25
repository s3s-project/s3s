// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

use crate::fs::FileSystem;
use crate::fs::InternalInfo;
use crate::utils::*;

use http::StatusCode;
use s3s::S3;
use s3s::S3Result;
use s3s::crypto::Checksum;
use s3s::crypto::Md5;
use s3s::dto::*;
use s3s::s3_error;
use s3s::{S3Request, S3Response};

#[cfg(test)]
use std::collections::VecDeque;
use std::fs::FileTimes;
use std::io;
use std::ops::Neg;
use std::ops::Not;
#[cfg(test)]
use std::path::Component;
use std::path::Path;
#[cfg(test)]
use std::path::PathBuf;
use std::time::Duration;
use std::time::SystemTime;

use tokio::fs;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncSeekExt;
use tokio::io::AsyncWriteExt;
use tokio_util::io::ReaderStream;

use futures::TryStreamExt;
use numeric_cast::NumericCast;
use stdx::default::default;
use tracing::{debug, warn};
use uuid::Uuid;

/// Read chunk size for streaming an object body out of the file system.
///
/// Larger chunks amortize the per-chunk syscall, poll and framing overhead over more bytes. The
/// buffer is held per in-flight request, so peak memory is roughly concurrent requests × this
/// size; 64 KiB is a stable point to pair with connection limiting later on.
const READ_CHUNK_SIZE: usize = 64 * 1024;

/// Copy chunk size for assembling a multipart object out of its parts.
///
/// One buffer is reused for every read from a part and every write to the temporary object, so
/// copying an object of `n` bytes costs roughly `2n / COPY_CHUNK_SIZE` syscalls and the buffer is
/// held per in-flight `CompleteMultipartUpload`. 256 KiB is four times fewer syscalls than
/// [`READ_CHUNK_SIZE`] while keeping that per-request footprint small. The two are separate
/// constants because the read path streams an object to a client and this one copies between files.
const COPY_CHUNK_SIZE: usize = 256 * 1024;

/// Maps a path that no longer exists to `None`, leaving every other error intact.
///
/// A listing walks the tree entry by entry, so an object deleted while the walk runs can be gone
/// by the time the walk reaches it. S3 omits such an object from the listing instead of failing
/// the request.
///
/// The walkers keep the bucket root out of this treatment: a bucket that no longer exists is
/// reported as `NoSuchBucket`, not as an empty listing.
fn skip_vanished<T>(result: io::Result<T>) -> io::Result<Option<T>> {
    match result {
        Ok(value) => Ok(Some(value)),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err),
    }
}

/// How many times a failed call is re-probed before its error is kept.
const VANISHED_PROBE_ATTEMPTS: u32 = 4;

/// Wait between two existence probes of a path.
const VANISHED_PROBE_DELAY: Duration = Duration::from_millis(5);

/// Whether `path` is gone, waiting while its delete is still pending.
///
/// Windows answers `PermissionDenied` instead of `NotFound` while a file or a
/// directory is being deleted, and keeps answering it until the delete completes,
/// so the probe is repeated a few times before the caller keeps its error.
async fn path_is_gone(path: &Path) -> bool {
    for _ in 0..VANISHED_PROBE_ATTEMPTS {
        match fs::symlink_metadata(path).await {
            // The path is gone: the call raced with the delete.
            Err(err) if err.kind() == io::ErrorKind::NotFound => return true,
            // The path is still there: the caller may not read it, keep the error.
            Ok(_) => return false,
            // A delete-pending path answers `PermissionDenied`; wait and probe again.
            Err(_) => tokio::time::sleep(VANISHED_PROBE_DELAY).await,
        }
    }
    false
}

/// Maps a failed directory enumeration step to the end of that directory.
///
/// Windows answers `PermissionDenied` from an enumeration whose directory is
/// being deleted, and the open handle that is being enumerated keeps the
/// directory delete-pending, so a probe cannot tell "gone" from "still there".
/// Ending that directory keeps the listing working without dropping the error of
/// any other call.
fn skip_vanished_iter<T>(result: io::Result<Option<T>>) -> io::Result<Option<T>> {
    match result {
        Ok(value) => Ok(value),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(err) if err.kind() == io::ErrorKind::PermissionDenied => Ok(None),
        Err(err) => Err(err),
    }
}

/// Maps a failed stat of one directory entry to `None` when that entry vanished.
async fn skip_vanished_entry<T>(entry: &fs::DirEntry, result: io::Result<T>) -> io::Result<Option<T>> {
    match result {
        Ok(value) => Ok(Some(value)),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(err) if err.kind() != io::ErrorKind::PermissionDenied => Err(err),
        // The path is built here so that the happy path does not pay for it.
        Err(err) => {
            if path_is_gone(&entry.path()).await {
                Ok(None)
            } else {
                Err(err)
            }
        }
    }
}

/// Maps a failed directory read to `None` when that directory vanished.
///
/// The directory counterpart of [`skip_vanished_entry`]: a directory that is
/// being removed answers `PermissionDenied` on Windows for as long as the
/// delete is pending.
async fn skip_vanished_dir<T>(dir: &Path, result: io::Result<T>) -> io::Result<Option<T>> {
    match result {
        Ok(value) => Ok(Some(value)),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(err) if err.kind() != io::ErrorKind::PermissionDenied => Err(err),
        Err(err) => {
            if path_is_gone(dir).await {
                Ok(None)
            } else {
                Err(err)
            }
        }
    }
}

/// Build the key of a path below the bucket root, or `None` when it cannot become a key.
///
/// The full scan the ordered walk replaced still uses it in tests.
#[cfg(test)]
fn normalize_path(path: &Path, delimiter: &str) -> Option<String> {
    let mut normalized = String::new();
    let mut first = true;
    for component in path.components() {
        match component {
            Component::RootDir | Component::CurDir | Component::ParentDir | Component::Prefix(_) => {
                return None;
            }
            Component::Normal(name) => {
                let name = name.to_str()?;
                if !first {
                    normalized.push_str(delimiter);
                }
                normalized.push_str(name);
                first = false;
            }
        }
    }
    Some(normalized)
}

/// <https://developer.mozilla.org/en-US/docs/Web/HTTP/Headers/Content-Range>
fn fmt_content_range(start: u64, end_inclusive: u64, size: u64) -> String {
    format!("bytes {start}-{end_inclusive}/{size}")
}

fn enable_expected_checksums(hasher: &mut s3s::checksum::ChecksumHasher, checksum: &s3s::dto::Checksum) {
    if checksum.checksum_crc32.is_some() {
        hasher.crc32 = Some(default());
    }
    if checksum.checksum_crc32c.is_some() {
        hasher.crc32c = Some(default());
    }
    if checksum.checksum_sha1.is_some() {
        hasher.sha1 = Some(default());
    }
    if checksum.checksum_sha256.is_some() {
        hasher.sha256 = Some(default());
    }
    if checksum.checksum_crc64nvme.is_some() {
        hasher.crc64nvme = Some(default());
    }
    if checksum.checksum_sha512.is_some() {
        hasher.sha512 = Some(default());
    }
    if checksum.checksum_md5.is_some() {
        hasher.md5 = Some(default());
    }
    if checksum.checksum_xxhash64.is_some() {
        hasher.xxhash64 = Some(default());
    }
    if checksum.checksum_xxhash3.is_some() {
        hasher.xxhash3 = Some(default());
    }
    if checksum.checksum_xxhash128.is_some() {
        hasher.xxhash128 = Some(default());
    }
}

fn enable_checksum_algorithm(hasher: &mut s3s::checksum::ChecksumHasher, algorithm: &str) -> S3Result<()> {
    match algorithm {
        ChecksumAlgorithm::CRC32 => hasher.crc32 = Some(default()),
        ChecksumAlgorithm::CRC32C => hasher.crc32c = Some(default()),
        ChecksumAlgorithm::SHA1 => hasher.sha1 = Some(default()),
        ChecksumAlgorithm::SHA256 => hasher.sha256 = Some(default()),
        ChecksumAlgorithm::CRC64NVME => hasher.crc64nvme = Some(default()),
        ChecksumAlgorithm::SHA512 => hasher.sha512 = Some(default()),
        ChecksumAlgorithm::MD5 => hasher.md5 = Some(default()),
        ChecksumAlgorithm::XXHASH64 => hasher.xxhash64 = Some(default()),
        ChecksumAlgorithm::XXHASH3 => hasher.xxhash3 = Some(default()),
        ChecksumAlgorithm::XXHASH128 => hasher.xxhash128 = Some(default()),
        _ => return Err(s3_error!(NotImplemented, "Unsupported checksum algorithm")),
    }
    Ok(())
}

fn checksum_mismatch(actual: &s3s::dto::Checksum, expected: &s3s::dto::Checksum) -> Option<&'static str> {
    if expected.checksum_crc32.is_some() && actual.checksum_crc32 != expected.checksum_crc32 {
        return Some("checksum_crc32");
    }
    if expected.checksum_crc32c.is_some() && actual.checksum_crc32c != expected.checksum_crc32c {
        return Some("checksum_crc32c");
    }
    if expected.checksum_sha1.is_some() && actual.checksum_sha1 != expected.checksum_sha1 {
        return Some("checksum_sha1");
    }
    if expected.checksum_sha256.is_some() && actual.checksum_sha256 != expected.checksum_sha256 {
        return Some("checksum_sha256");
    }
    if expected.checksum_crc64nvme.is_some() && actual.checksum_crc64nvme != expected.checksum_crc64nvme {
        return Some("checksum_crc64nvme");
    }
    if expected.checksum_sha512.is_some() && actual.checksum_sha512 != expected.checksum_sha512 {
        return Some("checksum_sha512");
    }
    if expected.checksum_md5.is_some() && actual.checksum_md5 != expected.checksum_md5 {
        return Some("checksum_md5");
    }
    if expected.checksum_xxhash64.is_some() && actual.checksum_xxhash64 != expected.checksum_xxhash64 {
        return Some("checksum_xxhash64");
    }
    if expected.checksum_xxhash3.is_some() && actual.checksum_xxhash3 != expected.checksum_xxhash3 {
        return Some("checksum_xxhash3");
    }
    if expected.checksum_xxhash128.is_some() && actual.checksum_xxhash128 != expected.checksum_xxhash128 {
        return Some("checksum_xxhash128");
    }
    None
}

fn has_any_checksum(checksum: &s3s::dto::Checksum) -> bool {
    checksum.checksum_crc32.is_some()
        || checksum.checksum_crc32c.is_some()
        || checksum.checksum_sha1.is_some()
        || checksum.checksum_sha256.is_some()
        || checksum.checksum_crc64nvme.is_some()
        || checksum.checksum_sha512.is_some()
        || checksum.checksum_md5.is_some()
        || checksum.checksum_xxhash64.is_some()
        || checksum.checksum_xxhash3.is_some()
        || checksum.checksum_xxhash128.is_some()
}

fn completed_part_checksum(part: &CompletedPart) -> s3s::dto::Checksum {
    s3s::dto::Checksum {
        checksum_crc32: part.checksum_crc32.clone(),
        checksum_crc32c: part.checksum_crc32c.clone(),
        checksum_sha1: part.checksum_sha1.clone(),
        checksum_sha256: part.checksum_sha256.clone(),
        checksum_crc64nvme: part.checksum_crc64nvme.clone(),
        checksum_sha512: part.checksum_sha512.clone(),
        checksum_md5: part.checksum_md5.clone(),
        checksum_xxhash64: part.checksum_xxhash64.clone(),
        checksum_xxhash3: part.checksum_xxhash3.clone(),
        checksum_xxhash128: part.checksum_xxhash128.clone(),
        ..Default::default()
    }
}

#[async_trait::async_trait]
impl S3 for FileSystem {
    #[tracing::instrument]
    async fn create_bucket(&self, req: S3Request<CreateBucketInput>) -> S3Result<S3Response<CreateBucketOutput>> {
        let input = req.input;
        let path = self.get_bucket_path(&input.bucket)?;

        if path.exists() {
            return Err(s3_error!(BucketAlreadyExists));
        }

        try_!(fs::create_dir(&path).await);

        let output = CreateBucketOutput::default(); // TODO: handle other fields
        Ok(S3Response::new(output))
    }

    #[tracing::instrument]
    async fn copy_object(&self, req: S3Request<CopyObjectInput>) -> S3Result<S3Response<CopyObjectOutput>> {
        let input = req.input;
        let (bucket, key) = match input.copy_source {
            CopySource::AccessPoint { .. } | CopySource::Outpost { .. } => return Err(s3_error!(NotImplemented)),
            CopySource::Bucket { ref bucket, ref key, .. } => (bucket, key),
        };

        let src_path = self.get_object_path(bucket, key)?;
        let dst_path = self.get_object_path(&input.bucket, &input.key)?;

        if src_path.exists().not() {
            return Err(s3_error!(NoSuchKey));
        }

        if self.get_bucket_path(&input.bucket)?.exists().not() {
            return Err(s3_error!(NoSuchBucket));
        }

        let file_metadata = try_!(fs::metadata(&src_path).await);
        let src_last_modified = Timestamp::from(try_!(file_metadata.modified()));

        // Always load internal info – needed for ETag derivation and checksum propagation.
        let src_info = self.load_internal_info(bucket, key).await?;

        // Derive source ETag from stored internal info when available.
        // For ETag-based conditions, fall back to MD5 only when no stored ETag exists.
        let mut src_etag: Option<ETag> = src_info.as_ref().and_then(crate::checksum::load_e_tag).map(ETag::Strong);

        // S3 precedence: If-Match overrides If-Unmodified-Since.
        if let Some(ref condition) = input.copy_source_if_match {
            if src_etag.is_none() {
                src_etag = Some(ETag::Strong(self.get_md5_sum(bucket, key).await?));
            }
            let src = src_etag.as_ref().ok_or_else(|| s3_error!(InternalError))?;
            if !condition.matches_strong(src) {
                return Err(s3_error!(PreconditionFailed));
            }
        } else if let Some(ref if_unmodified_since) = input.copy_source_if_unmodified_since
            && src_last_modified > *if_unmodified_since
        {
            return Err(s3_error!(PreconditionFailed));
        }

        // S3 precedence: If-None-Match overrides If-Modified-Since.
        if let Some(ref condition) = input.copy_source_if_none_match {
            if src_etag.is_none() {
                src_etag = Some(ETag::Strong(self.get_md5_sum(bucket, key).await?));
            }
            let src = src_etag.as_ref().ok_or_else(|| s3_error!(InternalError))?;
            if condition.matches_weak(src) {
                return Err(s3_error!(PreconditionFailed));
            }
        } else if let Some(ref if_modified_since) = input.copy_source_if_modified_since
            && src_last_modified <= *if_modified_since
        {
            return Err(s3_error!(PreconditionFailed));
        }

        if let Some(dir_path) = dst_path.parent() {
            try_!(fs::create_dir_all(&dir_path).await);
        }

        // `fs::copy(p, p)` truncates the file before reading it, so self-replace
        // must preserve bytes in place while still updating LastModified.
        let dst_last_modified = if src_path == dst_path {
            let now = SystemTime::now();
            let file = try_!(std::fs::OpenOptions::new().write(true).open(&dst_path));
            try_!(file.set_times(FileTimes::new().set_modified(now)));
            debug!(path = %dst_path.display(), "replace file in place");
            Timestamp::from(now)
        } else {
            let _ = try_!(fs::copy(&src_path, &dst_path).await);
            debug!(from = %src_path.display(), to = %dst_path.display(), "copy file");
            let dst_metadata = try_!(fs::metadata(&dst_path).await);
            Timestamp::from(try_!(dst_metadata.modified()))
        };

        // Derive the destination ETag from the source ETag when available.
        // This preserves non-MD5 ETag formats (e.g., multipart `{hash}-{part_count}`)
        // and avoids re-hashing the destination file.
        let dst_etag_str = match src_etag {
            Some(etag) => etag.into_value(),
            None => self.get_md5_sum(&input.bucket, &input.key).await?,
        };

        // `MetadataDirective` defaults to `COPY` per AWS API: when the
        // header is absent the destination should inherit the source's
        // metadata sidecar verbatim. When set to `REPLACE`, the
        // destination's metadata is built fresh from the request and
        // anything from the source is dropped (matching the behaviour
        // documented at
        // https://docs.aws.amazon.com/AmazonS3/latest/API/API_CopyObject.html).
        let replace_metadata = input
            .metadata_directive
            .as_ref()
            .is_some_and(|d| d.as_str() == MetadataDirective::REPLACE);

        if replace_metadata {
            let mut dst_attrs = crate::fs::ObjectAttributes {
                user_metadata: input.metadata,
                content_encoding: input.content_encoding,
                content_type: input.content_type,
                content_disposition: input.content_disposition,
                content_language: input.content_language,
                cache_control: input.cache_control,
                expires: None,
                website_redirect_location: input.website_redirect_location,
                checksum_algorithm: None,
                checksum_type: None,
            };
            dst_attrs.expires = input.expires;
            self.save_object_attributes(&input.bucket, &input.key, &dst_attrs, None)
                .await?;
        } else {
            let src_metadata_path = self.get_metadata_path(bucket, key, None)?;
            if src_metadata_path.exists() {
                let dst_metadata_path = self.get_metadata_path(&input.bucket, &input.key, None)?;
                // Same self-replace guard as for the payload above — `fs::copy`
                // would zero the metadata sidecar when src == dst.
                if src_metadata_path != dst_metadata_path {
                    let _ = try_!(fs::copy(src_metadata_path, dst_metadata_path).await);
                }
            }
        }

        {
            let mut info = src_info.unwrap_or_default();
            crate::checksum::save_e_tag(&mut info, &dst_etag_str);
            self.save_internal_info(&input.bucket, &input.key, &info).await?;
        }

        let copy_object_result = CopyObjectResult {
            e_tag: Some(ETag::Strong(dst_etag_str)),
            last_modified: Some(dst_last_modified),
            ..Default::default()
        };

        let output = CopyObjectOutput {
            copy_object_result: Some(copy_object_result),
            ..Default::default()
        };
        Ok(S3Response::new(output))
    }

    #[tracing::instrument]
    async fn delete_bucket(&self, req: S3Request<DeleteBucketInput>) -> S3Result<S3Response<DeleteBucketOutput>> {
        let input = req.input;
        let path = self.get_bucket_path(&input.bucket)?;
        if path.exists() {
            try_!(fs::remove_dir_all(path).await);
        } else {
            return Err(s3_error!(NoSuchBucket));
        }
        Ok(S3Response::new(DeleteBucketOutput {}))
    }

    #[tracing::instrument]
    async fn delete_object(&self, req: S3Request<DeleteObjectInput>) -> S3Result<S3Response<DeleteObjectOutput>> {
        let input = req.input;
        let path = self.get_object_path(&input.bucket, &input.key)?;
        if path.exists().not() {
            if self.get_bucket_path(&input.bucket)?.exists().not() {
                return Err(s3_error!(NoSuchBucket));
            }
            let output = DeleteObjectOutput::default();
            return Ok(S3Response::new(output));
        }
        if input.key.ends_with('/') {
            let mut dir = try_!(fs::read_dir(&path).await);
            let is_empty = try_!(dir.next_entry().await).is_none();
            if is_empty {
                try_!(fs::remove_dir(&path).await);
            }
        } else {
            try_!(fs::remove_file(&path).await);
        }
        let output = DeleteObjectOutput::default(); // TODO: handle other fields
        Ok(S3Response::new(output))
    }

    #[tracing::instrument]
    async fn delete_objects(&self, req: S3Request<DeleteObjectsInput>) -> S3Result<S3Response<DeleteObjectsOutput>> {
        let input = req.input;

        let mut deleted_objects: Vec<DeletedObject> = Vec::new();
        let mut errors = Vec::new();
        for object in input.delete.objects {
            let path = match self.get_object_path(&input.bucket, &object.key) {
                Ok(path) => path,
                Err(error) => {
                    errors.push(s3s::dto::Error {
                        key: Some(object.key),
                        version_id: object.version_id,
                        code: Some(error.code().as_str().to_owned()),
                        message: error.message().map(str::to_owned),
                    });
                    continue;
                }
            };
            if object.key.ends_with('/') {
                match fs::read_dir(&path).await {
                    Ok(mut dir) => {
                        let is_empty = try_!(dir.next_entry().await).is_none();
                        if is_empty {
                            try_!(fs::remove_dir(&path).await);
                        }
                    }
                    Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                    Err(e) => {
                        let _: () = try_!(Err(e));
                    }
                }
            } else {
                match fs::remove_file(&path).await {
                    Ok(()) => {}
                    Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                    Err(e) => {
                        let _: () = try_!(Err(e));
                    }
                }
            }

            let deleted_object = DeletedObject {
                key: Some(object.key),
                version_id: object.version_id,
                ..Default::default()
            };

            deleted_objects.push(deleted_object);
        }

        let output = DeleteObjectsOutput {
            deleted: Some(deleted_objects),
            errors: (!errors.is_empty()).then_some(errors),
            ..Default::default()
        };
        Ok(S3Response::new(output))
    }

    #[tracing::instrument]
    async fn get_bucket_location(&self, req: S3Request<GetBucketLocationInput>) -> S3Result<S3Response<GetBucketLocationOutput>> {
        let input = req.input;
        let path = self.get_bucket_path(&input.bucket)?;

        if !path.exists() {
            return Err(s3_error!(NoSuchBucket));
        }

        let output = GetBucketLocationOutput::default();
        Ok(S3Response::new(output))
    }

    #[tracing::instrument]
    async fn get_object(&self, req: S3Request<GetObjectInput>) -> S3Result<S3Response<GetObjectOutput>> {
        let input = req.input;
        let object_path = self.get_object_path(&input.bucket, &input.key)?;

        let mut file = fs::File::open(&object_path).await.map_err(|e| s3_error!(e, NoSuchKey))?;

        let file_metadata = try_!(file.metadata().await);
        let last_modified = Timestamp::from(try_!(file_metadata.modified()));
        let file_len = file_metadata.len();

        let (content_length, content_range) = match input.range {
            None => (file_len, None),
            Some(range) => {
                let file_range = range.check(file_len)?;
                let content_length = file_range.end - file_range.start;
                let content_range = fmt_content_range(file_range.start, file_range.end - 1, file_len);
                (content_length, Some(content_range))
            }
        };
        let content_length_usize = try_!(usize::try_from(content_length));
        let content_length_i64 = try_!(i64::try_from(content_length));

        match input.range {
            Some(Range::Int { first, .. }) => {
                try_!(file.seek(io::SeekFrom::Start(first)).await);
            }
            Some(Range::Suffix { length }) => {
                let neg_offset = length.numeric_cast::<i64>().neg();
                try_!(file.seek(io::SeekFrom::End(neg_offset)).await);
            }
            None => {}
        }

        let body = bytes_stream(ReaderStream::with_capacity(file, READ_CHUNK_SIZE), content_length_usize);

        let obj_attrs = self.load_object_attributes(&input.bucket, &input.key, None).await?;

        let info = self.load_internal_info(&input.bucket, &input.key).await?;

        let md5_sum = match info.as_ref().and_then(crate::checksum::load_e_tag) {
            Some(e_tag) => e_tag,
            None => self.get_md5_sum(&input.bucket, &input.key).await?,
        };

        let current = ETag::Strong(md5_sum.clone());
        match evaluate_read_condition(input.if_match.as_ref(), input.if_none_match.as_ref(), &current) {
            ReadCondition::PreconditionFailed => return Err(s3_error!(PreconditionFailed)),
            ReadCondition::NotModified => {
                return Ok(S3Response::with_status(
                    GetObjectOutput {
                        e_tag: Some(current),
                        last_modified: Some(last_modified),
                        ..Default::default()
                    },
                    StatusCode::NOT_MODIFIED,
                ));
            }
            ReadCondition::Proceed => {}
        }

        let checksum = match &info {
            // S3 skips returning the checksum if a range is specified that is
            // less than the whole file
            Some(info) if content_length == file_len => crate::checksum::from_internal_info(info),
            _ => default(),
        };

        #[allow(clippy::redundant_closure_for_method_calls)]
        let output = GetObjectOutput {
            body: Some(StreamingBlob::wrap(body)),
            content_length: Some(content_length_i64),
            content_range,
            last_modified: Some(last_modified),
            metadata: obj_attrs.as_ref().and_then(|a| a.user_metadata.clone()),
            content_encoding: obj_attrs.as_ref().and_then(|a| a.content_encoding.clone()),
            content_type: obj_attrs.as_ref().and_then(|a| a.content_type.clone()),
            content_disposition: obj_attrs.as_ref().and_then(|a| a.content_disposition.clone()),
            content_language: obj_attrs.as_ref().and_then(|a| a.content_language.clone()),
            cache_control: obj_attrs.as_ref().and_then(|a| a.cache_control.clone()),
            expires: obj_attrs.as_ref().and_then(|a| a.expires.clone()),
            website_redirect_location: obj_attrs.as_ref().and_then(|a| a.website_redirect_location.clone()),
            e_tag: Some(ETag::Strong(md5_sum)),
            checksum_crc32: checksum.checksum_crc32,
            checksum_crc32c: checksum.checksum_crc32c,
            checksum_sha1: checksum.checksum_sha1,
            checksum_sha256: checksum.checksum_sha256,
            checksum_crc64nvme: checksum.checksum_crc64nvme,
            checksum_sha512: checksum.checksum_sha512,
            checksum_md5: checksum.checksum_md5,
            checksum_xxhash64: checksum.checksum_xxhash64,
            checksum_xxhash3: checksum.checksum_xxhash3,
            checksum_xxhash128: checksum.checksum_xxhash128,
            ..Default::default()
        };
        Ok(S3Response::new(output))
    }

    #[tracing::instrument]
    async fn head_bucket(&self, req: S3Request<HeadBucketInput>) -> S3Result<S3Response<HeadBucketOutput>> {
        let input = req.input;
        let path = self.get_bucket_path(&input.bucket)?;

        if !path.exists() {
            return Err(s3_error!(NoSuchBucket));
        }

        Ok(S3Response::new(HeadBucketOutput::default()))
    }

    #[tracing::instrument]
    async fn head_object(&self, req: S3Request<HeadObjectInput>) -> S3Result<S3Response<HeadObjectOutput>> {
        let input = req.input;
        let path = self.get_object_path(&input.bucket, &input.key)?;

        if !path.exists() {
            if self.get_bucket_path(&input.bucket)?.exists().not() {
                return Err(s3_error!(NoSuchBucket));
            }
            return Err(s3_error!(NoSuchKey));
        }

        let file_metadata = try_!(fs::metadata(path).await);
        if file_metadata.is_dir() {
            return Err(s3_error!(NoSuchKey));
        }
        let last_modified = Timestamp::from(try_!(file_metadata.modified()));
        let file_len = file_metadata.len();

        let obj_attrs = self.load_object_attributes(&input.bucket, &input.key, None).await?;

        let info = self.load_internal_info(&input.bucket, &input.key).await?;

        let md5_sum = match info.as_ref().and_then(crate::checksum::load_e_tag) {
            Some(e_tag) => e_tag,
            None => self.get_md5_sum(&input.bucket, &input.key).await?,
        };

        let current = ETag::Strong(md5_sum.clone());
        match evaluate_read_condition(input.if_match.as_ref(), input.if_none_match.as_ref(), &current) {
            ReadCondition::PreconditionFailed => return Err(s3_error!(PreconditionFailed)),
            ReadCondition::NotModified => {
                return Ok(S3Response::with_status(
                    HeadObjectOutput {
                        e_tag: Some(current),
                        last_modified: Some(last_modified),
                        ..Default::default()
                    },
                    StatusCode::NOT_MODIFIED,
                ));
            }
            ReadCondition::Proceed => {}
        }

        let checksum = match &info {
            Some(info) => crate::checksum::from_internal_info(info),
            _ => default(),
        };

        #[allow(clippy::redundant_closure_for_method_calls)]
        let output = HeadObjectOutput {
            content_length: Some(try_!(i64::try_from(file_len))),
            content_type: obj_attrs.as_ref().and_then(|a| a.content_type.clone()),
            content_encoding: obj_attrs.as_ref().and_then(|a| a.content_encoding.clone()),
            content_disposition: obj_attrs.as_ref().and_then(|a| a.content_disposition.clone()),
            content_language: obj_attrs.as_ref().and_then(|a| a.content_language.clone()),
            cache_control: obj_attrs.as_ref().and_then(|a| a.cache_control.clone()),
            expires: obj_attrs.as_ref().and_then(|a| a.expires.clone()),
            website_redirect_location: obj_attrs.as_ref().and_then(|a| a.website_redirect_location.clone()),
            last_modified: Some(last_modified),
            metadata: obj_attrs.as_ref().and_then(|a| a.user_metadata.clone()),
            e_tag: Some(ETag::Strong(md5_sum)),
            checksum_crc32: checksum.checksum_crc32,
            checksum_crc32c: checksum.checksum_crc32c,
            checksum_sha1: checksum.checksum_sha1,
            checksum_sha256: checksum.checksum_sha256,
            checksum_crc64nvme: checksum.checksum_crc64nvme,
            checksum_sha512: checksum.checksum_sha512,
            checksum_md5: checksum.checksum_md5,
            checksum_xxhash64: checksum.checksum_xxhash64,
            checksum_xxhash3: checksum.checksum_xxhash3,
            checksum_xxhash128: checksum.checksum_xxhash128,
            ..Default::default()
        };
        Ok(S3Response::new(output))
    }

    #[tracing::instrument]
    async fn list_buckets(&self, _: S3Request<ListBucketsInput>) -> S3Result<S3Response<ListBucketsOutput>> {
        let mut buckets: Vec<Bucket> = Vec::new();
        let mut iter = try_!(fs::read_dir(&self.root).await);
        // The service root is not a directory discovered during a walk, so its enumeration
        // keeps surfacing a failure instead of ending the listing silently.
        while let Some(entry) = try_!(iter.next_entry().await) {
            let file_type = try_!(entry.file_type().await);
            if file_type.is_dir().not() {
                continue;
            }

            let file_name = entry.file_name();
            let Some(name) = file_name.to_str() else { continue };
            if s3s::path::check_bucket_name(name).not() {
                continue;
            }

            let file_meta = try_!(entry.metadata().await);
            // Not all filesystems/mounts provide all file attributes like created timestamp,
            // therefore we try to fallback to modified if possible.
            // See https://github.com/Nugine/s3s/pull/22 for more details.
            let created_or_modified_date = Timestamp::from(try_!(file_meta.created().or(file_meta.modified())));

            let bucket = Bucket {
                creation_date: Some(created_or_modified_date),
                name: Some(name.to_owned()),
                bucket_region: None,
                bucket_arn: None,
            };
            buckets.push(bucket);
        }

        let output = ListBucketsOutput {
            buckets: Some(buckets),
            owner: None,
            ..Default::default()
        };
        Ok(S3Response::new(output))
    }

    #[tracing::instrument]
    async fn list_objects(&self, req: S3Request<ListObjectsInput>) -> S3Result<S3Response<ListObjectsOutput>> {
        let v2_resp = self.list_objects_v2(req.map_input(Into::into)).await?;

        Ok(v2_resp.map_output(|v2| ListObjectsOutput {
            contents: v2.contents,
            common_prefixes: v2.common_prefixes,
            delimiter: v2.delimiter,
            encoding_type: v2.encoding_type,
            name: v2.name,
            prefix: v2.prefix,
            max_keys: v2.max_keys,
            is_truncated: v2.is_truncated,
            next_marker: v2.next_continuation_token,
            ..Default::default()
        }))
    }

    #[tracing::instrument]
    async fn list_objects_v2(&self, req: S3Request<ListObjectsV2Input>) -> S3Result<S3Response<ListObjectsV2Output>> {
        let input = req.input;
        let path = self.get_bucket_path(&input.bucket)?;

        if path.exists().not() {
            return Err(s3_error!(NoSuchBucket));
        }

        let delimiter = input.delimiter.as_deref();
        let prefix = input.prefix.as_deref().unwrap_or("").trim_start_matches('/');
        let max_keys = input.max_keys.unwrap_or(1000);

        let start_after = match (input.continuation_token.as_deref(), input.start_after.as_deref()) {
            (Some(ct), Some(sa)) => Some(if ct >= sa { ct } else { sa }),
            (Some(ct), None) => Some(ct),
            (None, Some(sa)) => Some(sa),
            (None, None) => None,
        };

        let query = ListingQuery {
            prefix,
            delimiter,
            start_after,
            max_keys: usize::try_from(max_keys).unwrap_or(1000),
        };

        // The walk collects one page in key order, so a page no longer has to be cut out of the whole
        // bucket. The page arithmetic itself is shared with the full scan the walk replaced.
        let page = self.list_page(&path, &query).await?;

        let contents = page.objects.is_empty().not().then_some(page.objects);
        let common_prefixes = page.common_prefixes.is_empty().not().then_some(page.common_prefixes);

        let output = ListObjectsV2Output {
            key_count: Some(page.key_count),
            max_keys: Some(max_keys),
            is_truncated: Some(page.is_truncated),
            contents,
            common_prefixes,
            continuation_token: input.continuation_token,
            next_continuation_token: page.next_continuation_token,
            delimiter: input.delimiter,
            encoding_type: input.encoding_type,
            name: Some(input.bucket),
            prefix: input.prefix,
            start_after: input.start_after,
            ..Default::default()
        };
        Ok(S3Response::new(output))
    }

    #[tracing::instrument]
    async fn put_object(&self, req: S3Request<PutObjectInput>) -> S3Result<S3Response<PutObjectOutput>> {
        use crate::fs::ObjectAttributes;

        let mut input = req.input;
        if let Some(ref storage_class) = input.storage_class {
            let is_valid = ["STANDARD", "REDUCED_REDUNDANCY"].contains(&storage_class.as_str());
            if !is_valid {
                return Err(s3_error!(InvalidStorageClass));
            }
        }

        let PutObjectInput {
            body,
            bucket,
            key,
            metadata,
            content_length,
            content_md5,
            content_encoding,
            content_type,
            content_disposition,
            content_language,
            cache_control,
            expires,
            website_redirect_location,
            if_match,
            if_none_match,
            ..
        } = input;

        let Some(body) = body else { return Err(s3_error!(IncompleteBody)) };

        // Check conditional headers before modifying any state.
        // If-None-Match: * means "only create if the object doesn't exist".
        // If-Match: <etag> means "only overwrite if ETag matches" (CAS).
        let object_path = self.get_object_path(&bucket, &key)?;
        // If-None-Match: * means "only write if the object does not exist". A tag
        // or a list is not implemented, which is what Amazon S3 answers for it.
        if let Some(ref condition) = if_none_match {
            if !condition.is_any() {
                return Err(s3_error!(NotImplemented));
            }
            if object_path.exists() {
                return Err(s3_error!(PreconditionFailed, "Object already exists"));
            }
        }
        if let Some(ref condition) = if_match {
            if !object_path.exists() {
                return Err(s3_error!(PreconditionFailed, "Object does not exist"));
            }
            if !condition.is_any() && !condition.matches_strong(&self.current_etag(&bucket, &key).await?) {
                return Err(s3_error!(PreconditionFailed, "ETag does not match"));
            }
        }

        let mut checksum: s3s::checksum::ChecksumHasher = default();
        if input.checksum_crc32.is_some() {
            checksum.crc32 = Some(default());
        }
        if input.checksum_crc32c.is_some() {
            checksum.crc32c = Some(default());
        }
        if input.checksum_sha1.is_some() {
            checksum.sha1 = Some(default());
        }
        if input.checksum_sha256.is_some() {
            checksum.sha256 = Some(default());
        }
        if input.checksum_crc64nvme.is_some() {
            checksum.crc64nvme = Some(default());
        }
        if input.checksum_sha512.is_some() {
            checksum.sha512 = Some(default());
        }
        if input.checksum_md5.is_some() {
            checksum.md5 = Some(default());
        }
        if input.checksum_xxhash64.is_some() {
            checksum.xxhash64 = Some(default());
        }
        if input.checksum_xxhash3.is_some() {
            checksum.xxhash3 = Some(default());
        }
        if input.checksum_xxhash128.is_some() {
            checksum.xxhash128 = Some(default());
        }
        if let Some(alg) = input.checksum_algorithm {
            match alg.as_str() {
                ChecksumAlgorithm::CRC32 => checksum.crc32 = Some(default()),
                ChecksumAlgorithm::CRC32C => checksum.crc32c = Some(default()),
                ChecksumAlgorithm::SHA1 => checksum.sha1 = Some(default()),
                ChecksumAlgorithm::SHA256 => checksum.sha256 = Some(default()),
                ChecksumAlgorithm::CRC64NVME => checksum.crc64nvme = Some(default()),
                ChecksumAlgorithm::SHA512 => checksum.sha512 = Some(default()),
                ChecksumAlgorithm::MD5 => checksum.md5 = Some(default()),
                ChecksumAlgorithm::XXHASH64 => checksum.xxhash64 = Some(default()),
                ChecksumAlgorithm::XXHASH3 => checksum.xxhash3 = Some(default()),
                ChecksumAlgorithm::XXHASH128 => checksum.xxhash128 = Some(default()),
                _ => return Err(s3_error!(NotImplemented, "Unsupported checksum algorithm")),
            }
        }

        if key.ends_with('/') {
            if let Some(len) = content_length
                && len > 0
            {
                return Err(s3_error!(UnexpectedContent, "Unexpected request body when creating a directory object."));
            }
            try_!(fs::create_dir_all(&object_path).await);
            let output = PutObjectOutput::default();
            return Ok(S3Response::new(output));
        }

        let mut file_writer = self.prepare_file_write(&object_path).await?;

        let mut md5_hash = Md5::new();
        let stream = body.inspect_ok(|bytes| {
            md5_hash.update(bytes.as_ref());
            checksum.update(bytes.as_ref());
        });
        let size = copy_bytes(stream, file_writer.writer()).await?;

        let md5_sum = hex(md5_hash.finalize());

        if let Some(content_md5) = content_md5 {
            let content_md5 = base64_simd::STANDARD
                .decode_to_vec(content_md5)
                .map_err(|_| s3_error!(InvalidArgument))?;
            let content_md5 = hex(content_md5);
            if content_md5 != md5_sum {
                return Err(s3_error!(BadDigest, "content_md5 mismatch"));
            }
        }

        let checksum = checksum.finalize();

        if let Some(trailers) = req.trailing_headers
            && let Some(trailers) = trailers.take()
        {
            if let Some(crc32) = trailers.get("x-amz-checksum-crc32") {
                input.checksum_crc32 = Some(crc32.to_str().map_err(|_| s3_error!(InvalidArgument))?.to_owned());
            }
            if let Some(crc32c) = trailers.get("x-amz-checksum-crc32c") {
                input.checksum_crc32c = Some(crc32c.to_str().map_err(|_| s3_error!(InvalidArgument))?.to_owned());
            }
            if let Some(sha1) = trailers.get("x-amz-checksum-sha1") {
                input.checksum_sha1 = Some(sha1.to_str().map_err(|_| s3_error!(InvalidArgument))?.to_owned());
            }
            if let Some(sha256) = trailers.get("x-amz-checksum-sha256") {
                input.checksum_sha256 = Some(sha256.to_str().map_err(|_| s3_error!(InvalidArgument))?.to_owned());
            }
            if let Some(crc64nvme) = trailers.get("x-amz-checksum-crc64nvme") {
                input.checksum_crc64nvme = Some(crc64nvme.to_str().map_err(|_| s3_error!(InvalidArgument))?.to_owned());
            }
            if let Some(sha512) = trailers.get("x-amz-checksum-sha512") {
                input.checksum_sha512 = Some(sha512.to_str().map_err(|_| s3_error!(InvalidArgument))?.to_owned());
            }
            if let Some(md5) = trailers.get("x-amz-checksum-md5") {
                input.checksum_md5 = Some(md5.to_str().map_err(|_| s3_error!(InvalidArgument))?.to_owned());
            }
            if let Some(xxhash64) = trailers.get("x-amz-checksum-xxhash64") {
                input.checksum_xxhash64 = Some(xxhash64.to_str().map_err(|_| s3_error!(InvalidArgument))?.to_owned());
            }
            if let Some(xxhash3) = trailers.get("x-amz-checksum-xxhash3") {
                input.checksum_xxhash3 = Some(xxhash3.to_str().map_err(|_| s3_error!(InvalidArgument))?.to_owned());
            }
            if let Some(xxhash128) = trailers.get("x-amz-checksum-xxhash128") {
                input.checksum_xxhash128 = Some(xxhash128.to_str().map_err(|_| s3_error!(InvalidArgument))?.to_owned());
            }
        }

        if checksum.checksum_crc32 != input.checksum_crc32 {
            return Err(s3_error!(
                BadDigest,
                "checksum_crc32 mismatch: expected `{}`, got `{}`",
                input.checksum_crc32.unwrap_or_default(),
                checksum.checksum_crc32.unwrap_or_default()
            ));
        }
        if checksum.checksum_crc32c != input.checksum_crc32c {
            return Err(s3_error!(BadDigest, "checksum_crc32c mismatch"));
        }
        if checksum.checksum_sha1 != input.checksum_sha1 {
            return Err(s3_error!(BadDigest, "checksum_sha1 mismatch"));
        }
        if checksum.checksum_sha256 != input.checksum_sha256 {
            return Err(s3_error!(BadDigest, "checksum_sha256 mismatch"));
        }
        if checksum.checksum_crc64nvme != input.checksum_crc64nvme {
            return Err(s3_error!(BadDigest, "checksum_crc64nvme mismatch"));
        }
        if checksum.checksum_sha512 != input.checksum_sha512 {
            return Err(s3_error!(BadDigest, "checksum_sha512 mismatch"));
        }
        if checksum.checksum_md5 != input.checksum_md5 {
            return Err(s3_error!(BadDigest, "checksum_md5 mismatch"));
        }
        if checksum.checksum_xxhash64 != input.checksum_xxhash64 {
            return Err(s3_error!(BadDigest, "checksum_xxhash64 mismatch"));
        }
        if checksum.checksum_xxhash3 != input.checksum_xxhash3 {
            return Err(s3_error!(BadDigest, "checksum_xxhash3 mismatch"));
        }
        if checksum.checksum_xxhash128 != input.checksum_xxhash128 {
            return Err(s3_error!(BadDigest, "checksum_xxhash128 mismatch"));
        }

        file_writer.done().await?;

        debug!(path = %object_path.display(), ?size, %md5_sum, ?checksum, "write file");

        // Save object attributes (including user metadata and standard attributes)
        let mut obj_attrs = ObjectAttributes {
            user_metadata: metadata,
            content_encoding,
            content_type,
            content_disposition,
            content_language,
            cache_control,
            expires: None,
            website_redirect_location,
            checksum_algorithm: None,
            checksum_type: None,
        };
        obj_attrs.expires = expires;
        self.save_object_attributes(&bucket, &key, &obj_attrs, None).await?;

        let mut info: InternalInfo = default();
        crate::checksum::save_e_tag(&mut info, &md5_sum);
        crate::checksum::modify_internal_info(&mut info, &checksum);
        self.save_internal_info(&bucket, &key, &info).await?;

        let output = PutObjectOutput {
            e_tag: Some(ETag::Strong(md5_sum)),
            checksum_crc32: checksum.checksum_crc32,
            checksum_crc32c: checksum.checksum_crc32c,
            checksum_sha1: checksum.checksum_sha1,
            checksum_sha256: checksum.checksum_sha256,
            checksum_crc64nvme: checksum.checksum_crc64nvme,
            checksum_sha512: checksum.checksum_sha512,
            checksum_md5: checksum.checksum_md5,
            checksum_xxhash64: checksum.checksum_xxhash64,
            checksum_xxhash3: checksum.checksum_xxhash3,
            checksum_xxhash128: checksum.checksum_xxhash128,
            ..Default::default()
        };
        Ok(S3Response::new(output))
    }

    #[tracing::instrument]
    async fn create_multipart_upload(
        &self,
        req: S3Request<CreateMultipartUploadInput>,
    ) -> S3Result<S3Response<CreateMultipartUploadOutput>> {
        use crate::fs::ObjectAttributes;

        let input = req.input;
        if let Some(checksum_type) = input.checksum_type.as_ref()
            && checksum_type.as_str() != ChecksumType::FULL_OBJECT
        {
            return Err(s3_error!(NotImplemented, "Unsupported multipart checksum type"));
        }

        let upload_id = self.create_upload_id(req.credentials.as_ref()).await?;
        let checksum_algorithm = input.checksum_algorithm.as_ref().map(|x| x.as_str().to_owned());
        let checksum_type = input.checksum_type.as_ref().map(|x| x.as_str().to_owned());

        // Save object attributes (including user metadata and standard attributes)
        let mut obj_attrs = ObjectAttributes {
            user_metadata: input.metadata,
            content_encoding: input.content_encoding,
            content_type: input.content_type,
            content_disposition: input.content_disposition,
            content_language: input.content_language,
            cache_control: input.cache_control,
            expires: None,
            website_redirect_location: input.website_redirect_location,
            checksum_algorithm,
            checksum_type,
        };
        obj_attrs.expires = input.expires;
        if let Err(err) = self
            .save_object_attributes(&input.bucket, &input.key, &obj_attrs, Some(upload_id))
            .await
        {
            // The upload directory exists but carries no attributes, so the client could not complete
            // the upload with the metadata it asked for. Drop the half-created upload instead.
            let _ = self.delete_upload_id(&upload_id).await;
            return Err(err.into());
        }

        let output = CreateMultipartUploadOutput {
            bucket: Some(input.bucket),
            key: Some(input.key),
            upload_id: Some(upload_id.to_string()),
            ..Default::default()
        };

        Ok(S3Response::new(output))
    }

    #[tracing::instrument]
    async fn upload_part(&self, req: S3Request<UploadPartInput>) -> S3Result<S3Response<UploadPartOutput>> {
        let mut input = req.input;
        let trailing_headers = req.trailing_headers;

        if input.part_number > 10_000 {
            return Err(s3_error!(
                InvalidArgument,
                "Part number must be an integer between 1 and 10000, inclusive"
            ));
        }

        let body = input.body.take().ok_or_else(|| s3_error!(IncompleteBody))?;

        let upload_id = Uuid::parse_str(&input.upload_id).map_err(|_| s3_error!(InvalidRequest))?;
        if self.verify_upload_id(req.credentials.as_ref(), &upload_id).await?.not() {
            return Err(s3_error!(AccessDenied));
        }

        let upload_attrs = self
            .load_object_attributes(&input.bucket, &input.key, Some(upload_id))
            .await?;

        let file_path = self.resolve_upload_part_path(upload_id, input.part_number)?;

        let mut expected_checksum = s3s::dto::Checksum {
            checksum_crc32: input.checksum_crc32.clone(),
            checksum_crc32c: input.checksum_crc32c.clone(),
            checksum_sha1: input.checksum_sha1.clone(),
            checksum_sha256: input.checksum_sha256.clone(),
            checksum_crc64nvme: input.checksum_crc64nvme.clone(),
            checksum_sha512: input.checksum_sha512.clone(),
            checksum_md5: input.checksum_md5.clone(),
            checksum_xxhash64: input.checksum_xxhash64.clone(),
            checksum_xxhash3: input.checksum_xxhash3.clone(),
            checksum_xxhash128: input.checksum_xxhash128.clone(),
            ..Default::default()
        };

        let mut checksum: s3s::checksum::ChecksumHasher = default();
        enable_expected_checksums(&mut checksum, &expected_checksum);
        if let Some(algorithm) = upload_attrs.as_ref().and_then(|attrs| attrs.checksum_algorithm.as_deref()) {
            enable_checksum_algorithm(&mut checksum, algorithm)?;
        }
        if let Some(algorithm) = input.checksum_algorithm.as_ref() {
            enable_checksum_algorithm(&mut checksum, algorithm.as_str())?;
        }

        let mut md5_hash = Md5::new();
        let stream = body.inspect_ok(|bytes| {
            md5_hash.update(bytes.as_ref());
            checksum.update(bytes.as_ref());
        });

        let mut file_writer = self.prepare_file_write(&file_path).await?;
        let size = copy_bytes(stream, file_writer.writer()).await?;

        let md5_sum = hex(md5_hash.finalize());

        if let Some(trailers) = trailing_headers
            && let Some(trailers) = trailers.take()
        {
            if let Some(crc32) = trailers.get("x-amz-checksum-crc32") {
                expected_checksum.checksum_crc32 = Some(crc32.to_str().map_err(|_| s3_error!(InvalidArgument))?.to_owned());
            }
            if let Some(crc32c) = trailers.get("x-amz-checksum-crc32c") {
                expected_checksum.checksum_crc32c = Some(crc32c.to_str().map_err(|_| s3_error!(InvalidArgument))?.to_owned());
            }
            if let Some(sha1) = trailers.get("x-amz-checksum-sha1") {
                expected_checksum.checksum_sha1 = Some(sha1.to_str().map_err(|_| s3_error!(InvalidArgument))?.to_owned());
            }
            if let Some(sha256) = trailers.get("x-amz-checksum-sha256") {
                expected_checksum.checksum_sha256 = Some(sha256.to_str().map_err(|_| s3_error!(InvalidArgument))?.to_owned());
            }
            if let Some(crc64nvme) = trailers.get("x-amz-checksum-crc64nvme") {
                expected_checksum.checksum_crc64nvme =
                    Some(crc64nvme.to_str().map_err(|_| s3_error!(InvalidArgument))?.to_owned());
            }
            if let Some(sha512) = trailers.get("x-amz-checksum-sha512") {
                expected_checksum.checksum_sha512 = Some(sha512.to_str().map_err(|_| s3_error!(InvalidArgument))?.to_owned());
            }
            if let Some(md5) = trailers.get("x-amz-checksum-md5") {
                expected_checksum.checksum_md5 = Some(md5.to_str().map_err(|_| s3_error!(InvalidArgument))?.to_owned());
            }
            if let Some(xxhash64) = trailers.get("x-amz-checksum-xxhash64") {
                expected_checksum.checksum_xxhash64 = Some(xxhash64.to_str().map_err(|_| s3_error!(InvalidArgument))?.to_owned());
            }
            if let Some(xxhash3) = trailers.get("x-amz-checksum-xxhash3") {
                expected_checksum.checksum_xxhash3 = Some(xxhash3.to_str().map_err(|_| s3_error!(InvalidArgument))?.to_owned());
            }
            if let Some(xxhash128) = trailers.get("x-amz-checksum-xxhash128") {
                expected_checksum.checksum_xxhash128 =
                    Some(xxhash128.to_str().map_err(|_| s3_error!(InvalidArgument))?.to_owned());
            }
        }

        let checksum = checksum.finalize();

        if let Some(field) = checksum_mismatch(&checksum, &expected_checksum) {
            return Err(s3_error!(BadDigest, "{} mismatch", field));
        }

        file_writer.done().await?;

        let mut info: InternalInfo = default();
        crate::checksum::save_e_tag(&mut info, &md5_sum);
        crate::checksum::modify_internal_info(&mut info, &checksum);
        self.save_upload_part_info(upload_id, input.part_number, &info).await?;

        debug!(path = %file_path.display(), ?size, %md5_sum, "write file");

        let output = UploadPartOutput {
            e_tag: Some(ETag::Strong(md5_sum)),
            checksum_crc32: checksum.checksum_crc32,
            checksum_crc32c: checksum.checksum_crc32c,
            checksum_sha1: checksum.checksum_sha1,
            checksum_sha256: checksum.checksum_sha256,
            checksum_crc64nvme: checksum.checksum_crc64nvme,
            checksum_sha512: checksum.checksum_sha512,
            checksum_md5: checksum.checksum_md5,
            checksum_xxhash64: checksum.checksum_xxhash64,
            checksum_xxhash3: checksum.checksum_xxhash3,
            checksum_xxhash128: checksum.checksum_xxhash128,
            ..Default::default()
        };
        Ok(S3Response::new(output))
    }

    #[tracing::instrument]
    async fn upload_part_copy(&self, req: S3Request<UploadPartCopyInput>) -> S3Result<S3Response<UploadPartCopyOutput>> {
        let input = req.input;

        let upload_id = Uuid::parse_str(&input.upload_id).map_err(|_| s3_error!(InvalidRequest))?;
        let part_number = input.part_number;
        if self.verify_upload_id(req.credentials.as_ref(), &upload_id).await?.not() {
            return Err(s3_error!(AccessDenied));
        }

        let upload_attrs = self
            .load_object_attributes(&input.bucket, &input.key, Some(upload_id))
            .await?;

        let (src_bucket, src_key) = match input.copy_source {
            CopySource::AccessPoint { .. } | CopySource::Outpost { .. } => return Err(s3_error!(NotImplemented)),
            CopySource::Bucket { ref bucket, ref key, .. } => (bucket, key),
        };
        let src_path = self.get_object_path(src_bucket, src_key)?;
        let dst_path = self.resolve_upload_part_path(upload_id, part_number)?;

        let mut src_file = fs::File::open(&src_path).await.map_err(|e| s3_error!(e, NoSuchKey))?;
        let file_len = try_!(src_file.metadata().await).len();

        let (start, content_length) = if let Some(copy_range) = &input.copy_source_range {
            if !copy_range.starts_with("bytes=") {
                return Err(s3_error!(InvalidArgument));
            }
            let range = &copy_range["bytes=".len()..];
            let parts: Vec<&str> = range.split('-').collect();
            if parts.len() != 2 {
                return Err(s3_error!(InvalidArgument));
            }

            let start: u64 = parts[0].parse().map_err(|_| s3_error!(InvalidArgument))?;
            let end_inclusive = if parts[1].is_empty() {
                file_len.saturating_sub(1)
            } else {
                parts[1].parse().map_err(|_| s3_error!(InvalidArgument))?
            };
            if start > end_inclusive || start >= file_len || end_inclusive >= file_len {
                return Err(s3_error!(InvalidRange));
            }
            let content_length = end_inclusive - start + 1;
            (start, content_length)
        } else {
            (0, file_len)
        };
        let content_length_usize = try_!(usize::try_from(content_length));

        let _ = try_!(src_file.seek(io::SeekFrom::Start(start)).await);
        let body =
            StreamingBlob::wrap(bytes_stream(ReaderStream::with_capacity(src_file, READ_CHUNK_SIZE), content_length_usize));

        let expected_checksum: s3s::dto::Checksum = default();

        let mut checksum: s3s::checksum::ChecksumHasher = default();
        enable_expected_checksums(&mut checksum, &expected_checksum);
        if let Some(algorithm) = upload_attrs.as_ref().and_then(|attrs| attrs.checksum_algorithm.as_deref()) {
            enable_checksum_algorithm(&mut checksum, algorithm)?;
        }

        let mut md5_hash = Md5::new();
        let stream = body.inspect_ok(|bytes| {
            md5_hash.update(bytes.as_ref());
            checksum.update(bytes.as_ref());
        });

        let mut file_writer = self.prepare_file_write(&dst_path).await?;
        let size = copy_bytes(stream, file_writer.writer()).await?;
        file_writer.done().await?;

        let md5_sum = hex(md5_hash.finalize());
        let checksum = checksum.finalize();

        if let Some(field) = checksum_mismatch(&checksum, &expected_checksum) {
            return Err(s3_error!(BadDigest, "{} mismatch", field));
        }

        let mut info: InternalInfo = default();
        crate::checksum::save_e_tag(&mut info, &md5_sum);
        crate::checksum::modify_internal_info(&mut info, &checksum);
        self.save_upload_part_info(upload_id, part_number, &info).await?;

        debug!(path = %dst_path.display(), ?size, %md5_sum, "write file");

        let output = UploadPartCopyOutput {
            copy_part_result: Some(CopyPartResult {
                e_tag: Some(ETag::Strong(md5_sum)),
                checksum_crc32: checksum.checksum_crc32,
                checksum_crc32c: checksum.checksum_crc32c,
                checksum_sha1: checksum.checksum_sha1,
                checksum_sha256: checksum.checksum_sha256,
                checksum_crc64nvme: checksum.checksum_crc64nvme,
                checksum_sha512: checksum.checksum_sha512,
                checksum_md5: checksum.checksum_md5,
                checksum_xxhash64: checksum.checksum_xxhash64,
                checksum_xxhash3: checksum.checksum_xxhash3,
                checksum_xxhash128: checksum.checksum_xxhash128,
                ..Default::default()
            }),
            ..Default::default()
        };

        Ok(S3Response::new(output))
    }

    #[tracing::instrument]
    async fn list_parts(&self, req: S3Request<ListPartsInput>) -> S3Result<S3Response<ListPartsOutput>> {
        let ListPartsInput {
            bucket, key, upload_id, ..
        } = req.input;

        let upload_uuid = Uuid::parse_str(&upload_id).map_err(|_| s3_error!(InvalidRequest))?;

        // The parts live in the directory that owns this upload, so the listing never walks the root.
        let upload_dir_path = self.get_upload_dir_path(&upload_uuid)?;
        let Some(mut iter) = try_!(skip_vanished(fs::read_dir(&upload_dir_path).await)) else {
            return Err(s3_error!(NoSuchUpload));
        };

        let mut parts: Vec<Part> = Vec::new();

        while let Some(entry) = try_!(skip_vanished_iter(iter.next_entry().await)) {
            let file_type = try_!(entry.file_type().await);
            if file_type.is_file().not() {
                continue;
            }

            let file_name = entry.file_name();
            let Some(name) = file_name.to_str() else { continue };

            // `part-<n>` is a part body; `part-<n>.json` and the upload's own files are not.
            let Some(part_number) = name.strip_prefix("part-").and_then(|segment| segment.parse::<i32>().ok()) else {
                continue;
            };

            let file_meta = try_!(entry.metadata().await);
            let last_modified = Timestamp::from(try_!(file_meta.modified()));
            let size = try_!(i64::try_from(file_meta.len()));
            let part_info = self.load_upload_part_info(upload_uuid, part_number).await?;
            let checksum = part_info.as_ref().map_or_else(default, crate::checksum::from_internal_info);
            let e_tag = part_info.as_ref().and_then(crate::checksum::load_e_tag).map(ETag::Strong);

            let part = Part {
                checksum_crc32: checksum.checksum_crc32,
                checksum_crc32c: checksum.checksum_crc32c,
                checksum_sha1: checksum.checksum_sha1,
                checksum_sha256: checksum.checksum_sha256,
                checksum_crc64nvme: checksum.checksum_crc64nvme,
                checksum_sha512: checksum.checksum_sha512,
                checksum_md5: checksum.checksum_md5,
                checksum_xxhash64: checksum.checksum_xxhash64,
                checksum_xxhash3: checksum.checksum_xxhash3,
                checksum_xxhash128: checksum.checksum_xxhash128,
                e_tag,
                last_modified: Some(last_modified),
                part_number: Some(part_number),
                size: Some(size),
            };
            parts.push(part);
        }

        let output = ListPartsOutput {
            bucket: Some(bucket),
            key: Some(key),
            upload_id: Some(upload_id),
            parts: Some(parts),
            ..Default::default()
        };
        Ok(S3Response::new(output))
    }

    #[tracing::instrument]
    async fn complete_multipart_upload(
        &self,
        req: S3Request<CompleteMultipartUploadInput>,
    ) -> S3Result<S3Response<CompleteMultipartUploadOutput>> {
        let CompleteMultipartUploadInput {
            multipart_upload,
            bucket,
            key,
            upload_id,
            if_match,
            if_none_match,
            checksum_crc32,
            checksum_crc32c,
            checksum_sha1,
            checksum_sha256,
            checksum_crc64nvme,
            checksum_sha512,
            checksum_md5,
            checksum_xxhash64,
            checksum_xxhash3,
            checksum_xxhash128,
            checksum_type,
            ..
        } = req.input;

        let Some(multipart_upload) = multipart_upload else { return Err(s3_error!(InvalidPart)) };

        let parts_count = multipart_upload.parts.as_ref().map_or(0, Vec::len);
        if parts_count == 0 {
            return Err(s3_error!(InvalidPart, "You must specify at least one part"));
        }

        let upload_id = Uuid::parse_str(&upload_id).map_err(|_| s3_error!(InvalidRequest))?;
        if self.verify_upload_id(req.credentials.as_ref(), &upload_id).await?.not() {
            return Err(s3_error!(AccessDenied));
        }

        if let Some(checksum_type) = checksum_type.as_ref()
            && checksum_type.as_str() != ChecksumType::FULL_OBJECT
        {
            return Err(s3_error!(NotImplemented, "Unsupported multipart checksum type"));
        }

        let upload_attrs = self.load_object_attributes(&bucket, &key, Some(upload_id)).await?;
        if let Some(stored_checksum_type) = upload_attrs.as_ref().and_then(|attrs| attrs.checksum_type.as_deref())
            && stored_checksum_type != ChecksumType::FULL_OBJECT
        {
            return Err(s3_error!(NotImplemented, "Unsupported multipart checksum type"));
        }

        // Check conditional headers before modifying any state
        let object_path = self.get_object_path(&bucket, &key)?;
        // If-None-Match: * means "only write if the object does not exist". A tag
        // or a list is not implemented, which is what Amazon S3 answers for it.
        if let Some(ref condition) = if_none_match {
            if !condition.is_any() {
                return Err(s3_error!(NotImplemented));
            }
            if object_path.exists() {
                return Err(s3_error!(PreconditionFailed, "Object already exists"));
            }
        }
        if let Some(ref condition) = if_match {
            if !object_path.exists() {
                return Err(s3_error!(PreconditionFailed, "Object does not exist"));
            }
            if !condition.is_any() && !condition.matches_strong(&self.current_etag(&bucket, &key).await?) {
                return Err(s3_error!(PreconditionFailed, "ETag does not match"));
            }
        }

        let expected_checksum = s3s::dto::Checksum {
            checksum_crc32,
            checksum_crc32c,
            checksum_sha1,
            checksum_sha256,
            checksum_crc64nvme,
            checksum_sha512,
            checksum_md5,
            checksum_xxhash64,
            checksum_xxhash3,
            checksum_xxhash128,
            ..Default::default()
        };

        let mut checksum: s3s::checksum::ChecksumHasher = default();
        enable_expected_checksums(&mut checksum, &expected_checksum);
        if let Some(algorithm) = upload_attrs.as_ref().and_then(|attrs| attrs.checksum_algorithm.as_deref()) {
            enable_checksum_algorithm(&mut checksum, algorithm)?;
        }

        let mut file_writer = self.prepare_file_write(&object_path).await?;

        let mut cnt: i32 = 0;
        let total_parts_cnt = i32::try_from(parts_count).expect("total number of parts must be <= 10000.");

        let mut part_md5_hashes: Vec<[u8; 16]> = Vec::new();
        let mut buf = vec![0u8; COPY_CHUNK_SIZE];

        for part in multipart_upload.parts.into_iter().flatten() {
            let part_number = part
                .part_number
                .ok_or_else(|| s3_error!(InvalidRequest, "missing part number"))?;
            cnt += 1;
            if part_number != cnt {
                return Err(s3_error!(InvalidRequest, "invalid part order"));
            }

            let part_path = self.resolve_upload_part_path(upload_id, part_number)?;
            let part_info = self.load_upload_part_info(upload_id, part_number).await?;
            let saved_checksum = part_info.as_ref().map_or_else(default, crate::checksum::from_internal_info);
            let expected_part_checksum = completed_part_checksum(&part);

            if let Some(field) = checksum_mismatch(&saved_checksum, &expected_part_checksum) {
                return Err(s3_error!(InvalidPart, "{} mismatch for part {}", field, part_number));
            }

            let mut reader = try_!(fs::File::open(&part_path).await);
            let mut part_md5 = Md5::new();
            let mut size: u64 = 0;
            loop {
                let nread = try_!(reader.read(&mut buf).await);
                if nread == 0 {
                    break;
                }
                part_md5.update(&buf[..nread]);
                checksum.update(&buf[..nread]);
                try_!(file_writer.writer().write_all(&buf[..nread]).await);
                size += nread as u64;
            }
            // No flush per part: `done` flushes once, before it renames the temporary object.
            part_md5_hashes.push(part_md5.finalize());

            if part_number != total_parts_cnt && size < 5 * 1024 * 1024 {
                return Err(s3_error!(EntityTooSmall));
            }

            debug!(from = %part_path.display(), tmp = %file_writer.tmp_path().display(), to = %file_writer.dest_path().display(), ?size, "write file");
        }

        // Compute multipart ETag: MD5 of concatenated part MD5 hashes, suffixed with part count
        let mut etag_hash = Md5::new();
        for hash in &part_md5_hashes {
            etag_hash.update(hash);
        }
        let e_tag = format!("{}-{}", hex(etag_hash.finalize()), part_md5_hashes.len());
        let checksum = checksum.finalize();
        let checksum_type = has_any_checksum(&checksum).then(|| ChecksumType::from_static(ChecksumType::FULL_OBJECT));

        if let Some(field) = checksum_mismatch(&checksum, &expected_checksum) {
            return Err(s3_error!(BadDigest, "{} mismatch", field));
        }

        file_writer.done().await?;

        if let Some(attrs) = &upload_attrs {
            self.save_object_attributes(&bucket, &key, attrs, None).await?;
        }

        debug!(?e_tag, path = %object_path.display(), "multipart etag");

        {
            let mut info: InternalInfo = default();
            crate::checksum::save_e_tag(&mut info, &e_tag);
            crate::checksum::modify_internal_info(&mut info, &checksum);
            self.save_internal_info(&bucket, &key, &info).await?;
        }

        // The object is committed. Its upload directory holds the parts, their metadata and the
        // upload record, so removing the directory retires all of them at once; if that fails, the
        // directory stays behind and a later abort retries the cleanup. Cleanup failure never turns
        // into an error response, because the object is already committed.
        if let Err(err) = self.delete_upload_id(&upload_id).await {
            warn!(%upload_id, error = ?err, "failed to remove completed multipart upload state");
        }

        let output = CompleteMultipartUploadOutput {
            // TODO: better example of AWS-like keep-alive behavior
            future: Some(Box::pin(async move {
                Ok(CompleteMultipartUploadOutput {
                    bucket: Some(bucket),
                    checksum_crc32: checksum.checksum_crc32,
                    checksum_crc32c: checksum.checksum_crc32c,
                    checksum_sha1: checksum.checksum_sha1,
                    checksum_sha256: checksum.checksum_sha256,
                    checksum_crc64nvme: checksum.checksum_crc64nvme,
                    checksum_sha512: checksum.checksum_sha512,
                    checksum_md5: checksum.checksum_md5,
                    checksum_xxhash64: checksum.checksum_xxhash64,
                    checksum_xxhash3: checksum.checksum_xxhash3,
                    checksum_xxhash128: checksum.checksum_xxhash128,
                    checksum_type,
                    key: Some(key),
                    e_tag: Some(ETag::Strong(e_tag)),
                    ..Default::default()
                })
            })),
            ..Default::default()
        };

        debug!(?output);

        Ok(S3Response::new(output))
    }

    #[tracing::instrument]
    async fn abort_multipart_upload(
        &self,
        req: S3Request<AbortMultipartUploadInput>,
    ) -> S3Result<S3Response<AbortMultipartUploadOutput>> {
        let AbortMultipartUploadInput {
            bucket, key, upload_id, ..
        } = req.input;

        let upload_id = Uuid::parse_str(&upload_id).map_err(|_| s3_error!(InvalidRequest))?;
        if self.verify_upload_id(req.credentials.as_ref(), &upload_id).await?.not() {
            return Err(s3_error!(AccessDenied));
        }

        // The upload directory owns the parts, their metadata and the upload record, so a single
        // removal retires the whole upload. Removing an upload that is already gone is not an error.
        self.delete_upload_id(&upload_id).await?;

        debug!(bucket = %bucket, key = %key, upload_id = %upload_id, "multipart upload aborted");

        Ok(S3Response::new(AbortMultipartUploadOutput { ..Default::default() }))
    }
}

/// One item of a listing: an object, or the common prefix a delimiter groups keys under.
#[derive(Debug, Clone, PartialEq)]
enum ListingItem {
    /// Boxed because an object is an order of magnitude larger than a common prefix, and a listing
    /// holds mostly objects.
    Object(Box<Object>),
    CommonPrefix(String),
}

impl ListingItem {
    fn key(&self) -> &str {
        match self {
            Self::Object(object) => object.key.as_deref().unwrap_or(""),
            Self::CommonPrefix(prefix) => prefix.as_str(),
        }
    }

    /// A common prefix sorts before an object with the same key, which is how the merge behaved when
    /// a page was cut out of the collected list.
    #[cfg(test)]
    fn rank(&self) -> u8 {
        match self {
            Self::CommonPrefix(_) => 0,
            Self::Object(_) => 1,
        }
    }
}

/// What a listing asks for: the matching prefix, an optional delimiter, the resume point and the
/// number of items one page holds.
struct ListingQuery<'a> {
    prefix: &'a str,
    delimiter: Option<&'a str>,
    start_after: Option<&'a str>,
    max_keys: usize,
}

impl ListingQuery<'_> {
    /// The walk stops after one item more than a page holds: that extra item is what tells a page it
    /// was truncated.
    fn limit(&self) -> usize {
        self.max_keys.saturating_add(1)
    }
}

/// One page of a listing, plus what follows it.
#[derive(Debug, Default)]
struct ListingPage {
    objects: Vec<Object>,
    common_prefixes: Vec<CommonPrefix>,
    is_truncated: bool,
    next_continuation_token: Option<String>,
    key_count: i32,
}

/// Cut an ordered list of items down to one page, dropping the resume point and everything before it.
///
/// The walk already applied the resume point and stopped at one item past the page, so this only
/// has to agree with itself; the same function is used to cut a page out of a full scan.
fn build_page(items: Vec<ListingItem>, query: &ListingQuery<'_>) -> ListingPage {
    let filtered: Vec<ListingItem> = items
        .into_iter()
        .filter(|item| query.start_after.is_none_or(|marker| item.key() > marker))
        .collect();

    let is_truncated = query.max_keys > 0 && filtered.len() > query.max_keys;
    let emitted = filtered.len().min(query.max_keys);

    let mut objects = Vec::new();
    let mut common_prefixes = Vec::new();
    for item in filtered.iter().take(emitted) {
        match item {
            ListingItem::Object(object) => objects.push(object.as_ref().clone()),
            ListingItem::CommonPrefix(prefix) => common_prefixes.push(CommonPrefix {
                prefix: Some(prefix.clone()),
            }),
        }
    }

    let last_key = emitted.checked_sub(1).map(|index| filtered[index].key().to_owned());
    let next_continuation_token = if is_truncated {
        last_key.or_else(|| filtered.get(emitted).map(|item| item.key().to_owned()))
    } else {
        None
    };

    ListingPage {
        key_count: i32::try_from(emitted).unwrap_or(i32::MAX),
        is_truncated,
        next_continuation_token,
        objects,
        common_prefixes,
    }
}

/// Counts the directory entries a listing walks.
///
/// A test shows the ordered walk stops early by comparing this count against the count of a full
/// scan; a wall-clock measurement would be neither stable nor meaningful.
#[cfg(test)]
pub(crate) mod listing_stats {
    use std::cell::Cell;

    thread_local! {
        static ENTRIES_VISITED: Cell<u64> = const { Cell::new(0) };
    }

    pub(crate) fn reset() {
        ENTRIES_VISITED.with(|visited| visited.set(0));
    }

    pub(crate) fn visited() -> u64 {
        ENTRIES_VISITED.with(Cell::get)
    }

    pub(crate) fn count_entry() {
        ENTRIES_VISITED.with(|visited| visited.set(visited.get() + 1));
    }
}

/// Remember a common prefix, once. Every key that maps to one common prefix is contiguous in key
/// order, so a repeat is always the item that was pushed last.
fn push_common_prefix(items: &mut Vec<ListingItem>, common_prefix: String) {
    if items.last().is_some_and(|item| item.key() == common_prefix) {
        return;
    }
    items.push(ListingItem::CommonPrefix(common_prefix));
}

fn join_key(parent: &str, name: &str) -> String {
    if parent.is_empty() {
        name.to_owned()
    } else {
        format!("{parent}/{name}")
    }
}

/// What the conditional headers of a read request ask for.
enum ReadCondition {
    /// Answer the object.
    Proceed,
    /// `If-None-Match` matched the current representation: answer `304 Not Modified`.
    NotModified,
    /// `If-Match` did not match: answer `412 Precondition Failed`.
    PreconditionFailed,
}

/// Evaluates `If-Match` (strong) and `If-None-Match` (weak) against the current
/// representation, as RFC 9110 §13.1.1 and §13.1.2 require.
fn evaluate_read_condition(
    if_match: Option<&ETagCondition>,
    if_none_match: Option<&ETagCondition>,
    current: &ETag,
) -> ReadCondition {
    if let Some(condition) = if_match
        && !condition.matches_strong(current)
    {
        return ReadCondition::PreconditionFailed;
    }
    if let Some(condition) = if_none_match
        && condition.matches_weak(current)
    {
        return ReadCondition::NotModified;
    }
    ReadCondition::Proceed
}

impl FileSystem {
    /// List one page of a bucket in key order.
    async fn list_page(&self, bucket_root: &Path, query: &ListingQuery<'_>) -> S3Result<ListingPage> {
        if query.max_keys == 0 {
            return Ok(build_page(Vec::new(), query));
        }
        let items = self.list_objects_ordered(bucket_root, query).await?;
        Ok(build_page(items, query))
    }

    /// Walk the bucket in key order and stop once one item more than a page has been collected.
    ///
    /// Within one directory a file takes part with its name and a directory with its name plus `/`,
    /// because `.` (0x2E) sorts before `/` (0x2F): that is what keeps `a.txt` ahead of the keys under
    /// `a/`. The walk then follows the merged order, so it never has to collect the whole bucket.
    async fn list_objects_ordered(&self, bucket_root: &Path, query: &ListingQuery<'_>) -> S3Result<Vec<ListingItem>> {
        let mut items = Vec::new();
        Box::pin(self.walk_listing_dir(bucket_root, bucket_root, "", query, &mut items)).await?;
        Ok(items)
    }

    async fn walk_listing_dir(
        &self,
        bucket_root: &Path,
        dir: &Path,
        rel: &str,
        query: &ListingQuery<'_>,
        items: &mut Vec<ListingItem>,
    ) -> S3Result<()> {
        if items.len() >= query.limit() {
            return Ok(());
        }

        let Some(mut iter) = try_!(skip_vanished_dir(dir, fs::read_dir(dir).await).await) else {
            // The caller checks that the bucket exists before the walk, but the bucket root can
            // vanish before the walk reaches it. Only directories discovered during the walk are
            // skipped.
            if dir == bucket_root {
                return Err(s3_error!(NoSuchBucket));
            }
            return Ok(());
        };

        // (sort key, name, is a directory, entry)
        let mut entries: Vec<(String, String, bool, fs::DirEntry)> = Vec::new();
        while let Some(entry) = try_!(skip_vanished_iter(iter.next_entry().await)) {
            #[cfg(test)]
            listing_stats::count_entry();
            let Some(file_type) = try_!(skip_vanished_entry(&entry, entry.file_type().await).await) else {
                continue;
            };
            // A name that is not UTF-8 cannot become a key, and neither can anything below it.
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            let is_dir = file_type.is_dir();
            let sort_key = if is_dir { format!("{name}/") } else { name.clone() };
            entries.push((sort_key, name, is_dir, entry));
        }
        entries.sort_by(|lhs, rhs| lhs.0.cmp(&rhs.0));

        for (_, name, is_dir, entry) in entries {
            if items.len() >= query.limit() {
                return Ok(());
            }
            let key = join_key(rel, &name);

            if is_dir {
                let dir_prefix = format!("{key}/");

                // Every key below this directory starts with `dir_prefix`, so the subtree can only
                // contribute when one of the two is a prefix of the other.
                if !query.prefix.is_empty() && !query.prefix.starts_with(&dir_prefix) && !dir_prefix.starts_with(query.prefix) {
                    continue;
                }

                // With a delimiter, a directory whose own path already contains it puts its whole
                // subtree under a single common prefix, and nothing below it is listed separately.
                if let Some(delimiter) = query.delimiter
                    && let Some(remaining) = key.strip_prefix(query.prefix)
                {
                    let with_separator = format!("{remaining}/");
                    if let Some(position) = with_separator.find(delimiter) {
                        let common_prefix = format!("{}{}", query.prefix, &with_separator[..=position]);
                        if query.start_after.is_none_or(|marker| common_prefix.as_str() > marker)
                            && self.subtree_contains_file(&entry.path()).await?
                        {
                            push_common_prefix(items, common_prefix);
                        }
                        continue;
                    }
                }

                Box::pin(self.walk_listing_dir(bucket_root, &entry.path(), &key, query, items)).await?;
            } else {
                if !query.prefix.is_empty() && !key.starts_with(query.prefix) {
                    continue;
                }

                if let Some(delimiter) = query.delimiter {
                    let remaining = &key[query.prefix.len()..];
                    if let Some(position) = remaining.find(delimiter) {
                        let common_prefix = format!("{}{}", query.prefix, &remaining[..=position]);
                        // A common prefix at or before the resume point is skipped with the whole
                        // group it stands for, and skipping it must not use up a slot of the page.
                        if query.start_after.is_none_or(|marker| common_prefix.as_str() > marker) {
                            push_common_prefix(items, common_prefix);
                        }
                        continue;
                    }
                }

                if query.start_after.is_some_and(|marker| key.as_str() <= marker) {
                    continue;
                }
                let Some(metadata) = try_!(skip_vanished_entry(&entry, entry.metadata().await).await) else {
                    continue;
                };
                let last_modified = Timestamp::from(try_!(metadata.modified()));
                let size = try_!(i64::try_from(metadata.len()));

                items.push(ListingItem::Object(Box::new(Object {
                    key: Some(key),
                    last_modified: Some(last_modified),
                    size: Some(size),
                    ..Default::default()
                })));
            }
        }

        Ok(())
    }

    /// Whether any file exists below `dir`, which is what makes a subtree contribute a common prefix.
    async fn subtree_contains_file(&self, dir: &Path) -> S3Result<bool> {
        let Some(mut iter) = try_!(skip_vanished_dir(dir, fs::read_dir(dir).await).await) else {
            return Ok(false);
        };

        let mut subdirs = Vec::new();
        while let Some(entry) = try_!(skip_vanished_iter(iter.next_entry().await)) {
            #[cfg(test)]
            listing_stats::count_entry();
            let Some(file_type) = try_!(skip_vanished_entry(&entry, entry.file_type().await).await) else {
                continue;
            };
            if entry.file_name().to_str().is_none() {
                continue;
            }
            if file_type.is_dir() {
                subdirs.push(entry.path());
            } else {
                return Ok(true);
            }
        }

        for subdir in subdirs {
            if Box::pin(self.subtree_contains_file(&subdir)).await? {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// The listing as it was computed before the ordered walk: collect the whole bucket, sort it, then
    /// cut a page out of it. Kept as the reference the ordered walk is compared against.
    #[cfg(test)]
    async fn list_objects_full_scan(&self, bucket_root: &Path, query: &ListingQuery<'_>) -> S3Result<Vec<ListingItem>> {
        let mut objects: Vec<Object> = default();
        let mut common_prefixes = std::collections::BTreeSet::new();

        if let Some(delimiter) = query.delimiter {
            self.list_objects_with_delimiter(bucket_root, query.prefix, delimiter, &mut objects, &mut common_prefixes)
                .await?;
        } else {
            self.list_objects_recursive(bucket_root, query.prefix, &mut objects).await?;
        }

        let mut items: Vec<ListingItem> = objects
            .into_iter()
            .map(|object| ListingItem::Object(Box::new(object)))
            .collect();
        items.extend(common_prefixes.into_iter().map(ListingItem::CommonPrefix));
        items.sort_by(|lhs, rhs| lhs.key().cmp(rhs.key()).then_with(|| lhs.rank().cmp(&rhs.rank())));
        Ok(items)
    }

    /// The full scan the ordered walk replaced.
    #[cfg(test)]
    async fn list_objects_recursive(&self, bucket_root: &Path, prefix: &str, objects: &mut Vec<Object>) -> S3Result<()> {
        let mut dir_queue: VecDeque<PathBuf> = default();
        dir_queue.push_back(bucket_root.to_owned());
        let prefix_is_empty = prefix.is_empty();

        while let Some(dir) = dir_queue.pop_front() {
            let Some(mut iter) = try_!(skip_vanished_dir(&dir, fs::read_dir(&dir).await).await) else {
                // The caller checks that the bucket exists before the walk, but the bucket root can
                // vanish before the walk reaches it. Only directories discovered during the walk
                // are skipped.
                if dir.as_path() == bucket_root {
                    return Err(s3_error!(NoSuchBucket));
                }
                continue;
            };
            while let Some(entry) = try_!(skip_vanished_iter(iter.next_entry().await)) {
                #[cfg(test)]
                listing_stats::count_entry();
                let Some(file_type) = try_!(skip_vanished_entry(&entry, entry.file_type().await).await) else {
                    continue;
                };
                if file_type.is_dir() {
                    dir_queue.push_back(entry.path());
                } else {
                    let file_path = entry.path();
                    let key = try_!(file_path.strip_prefix(bucket_root));
                    let Some(key_str) = normalize_path(key, "/") else {
                        continue;
                    };

                    if !prefix_is_empty && !key_str.starts_with(prefix) {
                        continue;
                    }

                    let Some(metadata) = try_!(skip_vanished_entry(&entry, entry.metadata().await).await) else {
                        continue;
                    };
                    let last_modified = Timestamp::from(try_!(metadata.modified()));
                    let size = metadata.len();

                    let object = Object {
                        key: Some(key_str),
                        last_modified: Some(last_modified),
                        size: Some(try_!(i64::try_from(size))),
                        ..Default::default()
                    };
                    objects.push(object);
                }
            }
        }

        Ok(())
    }

    #[cfg(test)]
    async fn list_objects_with_delimiter(
        &self,
        bucket_root: &Path,
        prefix: &str,
        delimiter: &str,
        objects: &mut Vec<Object>,
        common_prefixes: &mut std::collections::BTreeSet<String>,
    ) -> S3Result<()> {
        // For delimiter-based listing, we need to recursively scan all files
        // but group them according to the delimiter rules
        let mut dir_queue: VecDeque<PathBuf> = default();
        dir_queue.push_back(bucket_root.to_owned());
        let prefix_is_empty = prefix.is_empty();

        while let Some(dir) = dir_queue.pop_front() {
            let Some(mut iter) = try_!(skip_vanished_dir(&dir, fs::read_dir(&dir).await).await) else {
                // The caller checks that the bucket exists before the walk, but the bucket root can
                // vanish before the walk reaches it. Only directories discovered during the walk
                // are skipped.
                if dir.as_path() == bucket_root {
                    return Err(s3_error!(NoSuchBucket));
                }
                continue;
            };

            while let Some(entry) = try_!(skip_vanished_iter(iter.next_entry().await)) {
                #[cfg(test)]
                listing_stats::count_entry();
                let Some(file_type) = try_!(skip_vanished_entry(&entry, entry.file_type().await).await) else {
                    continue;
                };
                let entry_path = entry.path();

                // Calculate the key relative to the bucket root
                let key = try_!(entry_path.strip_prefix(bucket_root));
                let Some(key_str) = normalize_path(key, "/") else {
                    continue;
                };

                // Skip if doesn't match prefix
                if !prefix_is_empty && !key_str.starts_with(prefix) {
                    // For directories, also skip if they don't have potential to contain matching files
                    if file_type.is_dir() && !prefix.starts_with(&key_str) && !key_str.starts_with(prefix) {
                        continue;
                    }
                    if file_type.is_file() {
                        continue;
                    }
                }

                if file_type.is_dir() {
                    // Continue scanning this directory
                    dir_queue.push_back(entry_path);
                } else {
                    // For files, determine if they should be listed directly or as common prefixes
                    let remaining = &key_str[prefix.len()..];

                    if remaining.contains(delimiter) {
                        // File is in a subdirectory, add the subdirectory as common prefix
                        if let Some(delimiter_pos) = remaining.find(delimiter) {
                            let mut next_prefix = String::with_capacity(prefix.len() + delimiter_pos + 1);
                            next_prefix.push_str(prefix);
                            next_prefix.push_str(&remaining[..=delimiter_pos]);
                            common_prefixes.insert(next_prefix);
                        }
                    } else {
                        // File is at the current level, include it in objects
                        let Some(metadata) = try_!(skip_vanished_entry(&entry, entry.metadata().await).await) else {
                            continue;
                        };
                        let last_modified = Timestamp::from(try_!(metadata.modified()));
                        let size = metadata.len();

                        let object = Object {
                            key: Some(key_str),
                            last_modified: Some(last_modified),
                            size: Some(try_!(i64::try_from(size))),
                            ..Default::default()
                        };
                        objects.push(object);
                    }
                }
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::env;

    use s3s::S3ErrorCode;
    use uuid::Uuid;

    struct TestRoot(PathBuf);

    impl Drop for TestRoot {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[tokio::test]
    async fn batch_delete_rejects_path_aliases_and_preserves_other_objects() {
        let root = env::temp_dir().join(format!("s3s-fs-delete-path-{}", Uuid::new_v4()));
        std::fs::create_dir_all(root.join("bucket/allowed")).unwrap();
        std::fs::create_dir_all(root.join("other-bucket")).unwrap();
        let _root = TestRoot(root.clone());
        let fs = FileSystem::new(&root).unwrap();
        for (key, content) in [
            ("bucket/allowed/ok", "delete me"),
            ("bucket/allowed/after", "delete me too"),
            ("bucket/victim", "keep victim"),
            ("bucket/allowed/sibling", "keep sibling"),
            ("bucket/allowed/repeated", "keep repeated-separator victim"),
            ("other-bucket/secret", "keep other bucket"),
        ] {
            std::fs::write(root.join(key), content).unwrap();
        }

        let invalid_keys = [
            "allowed//repeated",
            "allowed/../victim",
            "allowed/./sibling",
            "../other-bucket/secret",
            "/other-bucket/secret",
        ];
        let objects = std::iter::once("allowed/ok")
            .chain(invalid_keys)
            .chain(std::iter::once("allowed/after"))
            .map(|key| ObjectIdentifier {
                key: key.to_owned(),
                ..Default::default()
            })
            .collect();
        let input = DeleteObjectsInput {
            bucket: "bucket".to_owned(),
            delete: Delete {
                objects,
                ..Default::default()
            },
            bypass_governance_retention: None,
            checksum_algorithm: None,
            expected_bucket_owner: None,
            mfa: None,
            request_payer: None,
        };
        let request = S3Request {
            input,
            method: http::Method::POST,
            uri: http::Uri::from_static("/bucket?delete"),
            headers: http::HeaderMap::new(),
            extensions: http::Extensions::new(),
            credentials: None,
            region: None,
            service: None,
            trailing_headers: None,
        };
        let result = fs.delete_objects(request).await;

        assert_eq!(std::fs::read(root.join("bucket/victim")).unwrap(), b"keep victim");
        assert_eq!(std::fs::read(root.join("bucket/allowed/sibling")).unwrap(), b"keep sibling");
        assert_eq!(
            std::fs::read(root.join("bucket/allowed/repeated")).unwrap(),
            b"keep repeated-separator victim"
        );
        assert_eq!(std::fs::read(root.join("other-bucket/secret")).unwrap(), b"keep other bucket");
        assert!(!root.join("bucket/allowed/ok").exists());
        assert!(!root.join("bucket/allowed/after").exists());
        let output = result.unwrap().output;
        let deleted = output.deleted.unwrap();
        assert_eq!(deleted.len(), 2);
        assert_eq!(deleted[0].key.as_deref(), Some("allowed/ok"));
        assert_eq!(deleted[1].key.as_deref(), Some("allowed/after"));
        let errors = output.errors.unwrap();
        assert_eq!(errors.len(), invalid_keys.len());
        for (error, key) in errors.iter().zip(invalid_keys) {
            assert_eq!(error.key.as_deref(), Some(key));
            assert_eq!(error.code.as_deref(), Some("InvalidArgument"));
        }
    }

    #[tokio::test]
    async fn a_stat_failure_is_skipped_only_when_the_entry_is_gone() {
        let root = env::temp_dir().join(format!("s3s-fs-vanished-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let _root = TestRoot(root.clone());

        std::fs::write(root.join("present"), b"x").unwrap();
        std::fs::write(root.join("gone"), b"x").unwrap();

        let mut iter = fs::read_dir(&root).await.unwrap();
        let mut checked_gone = false;
        let mut checked_present = false;
        while let Some(entry) = iter.next_entry().await.unwrap() {
            match entry.file_name().to_string_lossy().as_ref() {
                "gone" => {
                    // A `NotFound` stat is skipped, as before.
                    let skipped = skip_vanished_entry::<()>(&entry, Err(io::Error::from(io::ErrorKind::NotFound)))
                        .await
                        .unwrap();
                    assert!(skipped.is_none());
                    // Windows reports a file that is being deleted as `PermissionDenied`; this one is
                    // gone too, so the entry is skipped.
                    std::fs::remove_file(root.join("gone")).unwrap();
                    let skipped = skip_vanished_entry::<()>(&entry, Err(io::Error::from(io::ErrorKind::PermissionDenied)))
                        .await
                        .unwrap();
                    assert!(skipped.is_none());
                    checked_gone = true;
                }
                "present" => {
                    // The entry is still there: the error is kept instead of dropping the object.
                    let err = skip_vanished_entry::<()>(&entry, Err(io::Error::from(io::ErrorKind::PermissionDenied)))
                        .await
                        .unwrap_err();
                    assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
                    // Any other error passes through untouched.
                    let err = skip_vanished_entry::<()>(&entry, Err(io::Error::from(io::ErrorKind::InvalidData)))
                        .await
                        .unwrap_err();
                    assert_eq!(err.kind(), io::ErrorKind::InvalidData);
                    checked_present = true;
                }
                _ => {}
            }
        }
        assert!(checked_gone, "the vanished entry must have been visited");
        assert!(checked_present, "the remaining entry must have been visited");
    }

    #[tokio::test]
    async fn a_directory_read_failure_is_skipped_only_when_the_directory_is_gone() {
        let root = env::temp_dir().join(format!("s3s-fs-vanished-dir-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let _root = TestRoot(root.clone());

        let present = root.join("present");
        std::fs::create_dir_all(&present).unwrap();
        let gone = root.join("gone");
        std::fs::create_dir_all(&gone).unwrap();
        std::fs::remove_dir(&gone).unwrap();

        // A `NotFound` read is skipped, as before.
        let skipped = skip_vanished_dir::<()>(&gone, Err(io::Error::from(io::ErrorKind::NotFound)))
            .await
            .unwrap();
        assert!(skipped.is_none());
        // Windows reports a directory that is being removed as `PermissionDenied`; this one is gone.
        let skipped = skip_vanished_dir::<()>(&gone, Err(io::Error::from(io::ErrorKind::PermissionDenied)))
            .await
            .unwrap();
        assert!(skipped.is_none());
        // The directory is still there: the error is kept instead of ending the walk silently.
        let err = skip_vanished_dir::<()>(&present, Err(io::Error::from(io::ErrorKind::PermissionDenied)))
            .await
            .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
        // Any other error passes through untouched.
        let err = skip_vanished_dir::<()>(&present, Err(io::Error::from(io::ErrorKind::InvalidData)))
            .await
            .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn listing_tolerates_objects_deleted_while_it_walks() {
        let root = env::temp_dir().join(format!("s3s-fs-list-race-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let _root = TestRoot(root.clone());
        let fs = FileSystem::new(&root).unwrap();

        // Enough directories that the walk is still running when the deletions start landing. Half
        // of them lose the whole directory and half lose only the object, so a directory that
        // vanishes mid-walk and an entry that vanishes mid-walk are both exercised.
        let bucket_root = root.join("bucket");
        let dirs: Vec<PathBuf> = (0..500)
            .map(|i| {
                let dir = bucket_root.join(format!("dir{i:04}"));
                std::fs::create_dir_all(&dir).unwrap();
                std::fs::write(dir.join("object"), b"x").unwrap();
                dir
            })
            .collect();

        let deleter = tokio::task::spawn_blocking(move || {
            for (index, dir) in dirs.into_iter().enumerate() {
                if index % 2 == 0 {
                    let _ = std::fs::remove_dir_all(&dir);
                } else {
                    let _ = std::fs::remove_file(dir.join("object"));
                }
            }
        });

        // The production entry point, not the full scan kept for the differential test.
        let query = ListingQuery {
            prefix: "",
            delimiter: None,
            start_after: None,
            max_keys: 1000,
        };
        let listed = fs.list_page(&bucket_root, &query).await;
        deleter.await.unwrap();

        let page = listed.expect("a delete running alongside the walk failed the listing");
        let keys: Vec<&str> = page.objects.iter().filter_map(|object| object.key.as_deref()).collect();
        assert!(keys.windows(2).all(|pair| pair[0] < pair[1]), "a page must stay in key order: {keys:?}");
        assert!(keys.len() <= 1000, "a page must not exceed max_keys: {}", keys.len());
    }

    #[test]
    fn an_enumeration_failure_ends_the_directory_only_for_a_vanished_directory() {
        // `NotFound` and the Windows delete-pending `PermissionDenied` both end the directory.
        for kind in [io::ErrorKind::NotFound, io::ErrorKind::PermissionDenied] {
            let ended = skip_vanished_iter::<u8>(Err(io::Error::from(kind))).unwrap();
            assert!(ended.is_none(), "{kind:?} must end the directory");
        }
        // A successful step is passed through.
        assert_eq!(skip_vanished_iter(Ok(Some(7u8))).unwrap(), Some(7));
        assert_eq!(skip_vanished_iter::<u8>(Ok(None)).unwrap(), None);
        // Any other error is kept.
        let err = skip_vanished_iter::<u8>(Err(io::Error::from(io::ErrorKind::InvalidData))).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[tokio::test]
    async fn listing_a_vanished_bucket_root_reports_no_such_bucket() {
        let root = env::temp_dir().join(format!("s3s-fs-list-vanished-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let _root = TestRoot(root.clone());
        let fs = FileSystem::new(&root).unwrap();
        let bucket_root = root.join("vanished-bucket");

        // Both shapes go through the production walk: plain, and grouped by a delimiter.
        let query = ListingQuery {
            prefix: "",
            delimiter: None,
            start_after: None,
            max_keys: 1000,
        };
        let err = fs
            .list_page(&bucket_root, &query)
            .await
            .expect_err("a vanished bucket root must not be listed as empty");
        assert_eq!(err.code(), &S3ErrorCode::NoSuchBucket);

        let query = ListingQuery {
            prefix: "",
            delimiter: Some("/"),
            start_after: None,
            max_keys: 1000,
        };
        let err = fs
            .list_page(&bucket_root, &query)
            .await
            .expect_err("a vanished bucket root must not be listed as empty");
        assert_eq!(err.code(), &S3ErrorCode::NoSuchBucket);
    }

    /// Write `keys` into a bucket below `root` and return the bucket root.
    fn write_bucket(root: &Path, name: &str, keys: &[&str]) -> PathBuf {
        let bucket_root = root.join(name);
        std::fs::create_dir_all(&bucket_root).unwrap();
        for key in keys {
            let path = bucket_root.join(key);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            std::fs::write(&path, key.as_bytes()).unwrap();
        }
        bucket_root
    }

    /// Why a corpus key cannot be stored on every platform the tests run on, if it cannot.
    ///
    /// Windows rejects `<>:"/\\|?*`, control characters, a name that ends with a dot or a space, and a
    /// few reserved device names. Checking here fails on the machine that adds the name, instead of on
    /// the Windows leg of CI, which only runs after a push.
    fn corpus_key_problem(key: &str) -> Option<String> {
        const RESERVED: [&str; 22] = [
            "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8", "COM9", "LPT1", "LPT2",
            "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
        ];

        for segment in key.split('/') {
            if segment.is_empty() {
                return Some(format!("key {key:?} has an empty path segment"));
            }
            if let Some(character) = segment
                .chars()
                .find(|c| matches!(c, '<' | '>' | ':' | '"' | '\\' | '|' | '?' | '*'))
            {
                return Some(format!("key {key:?} has a segment Windows rejects ({character:?}): {segment:?}"));
            }
            if let Some(character) = segment.chars().find(|c| c.is_control()) {
                return Some(format!("key {key:?} has a control character {character:?} in {segment:?}"));
            }
            if segment.ends_with('.') || segment.ends_with(' ') {
                return Some(format!("key {key:?} has a segment that ends with a dot or a space: {segment:?}"));
            }
            let stem = segment.split('.').next().unwrap_or(segment).to_ascii_uppercase();
            if RESERVED.contains(&stem.as_str()) {
                return Some(format!("key {key:?} has a segment named after a reserved Windows device: {segment:?}"));
            }
        }
        None
    }

    /// Panic when the corpus holds a key that cannot be stored on every platform the tests run on.
    fn assert_corpus_is_portable(keys: &[&str]) {
        for key in keys {
            if let Some(problem) = corpus_key_problem(key) {
                panic!("{problem}");
            }
        }
    }

    /// The guard protects nothing unless it rejects what Windows rejects.
    #[test]
    fn the_portability_guard_rejects_names_windows_cannot_store() {
        let rejected = [
            "quote\".txt",
            "delim::x.txt",
            "back\\slash.txt",
            "star*.txt",
            "question?.txt",
            "pipe|.txt",
            "less<.txt",
            "trailing.",
            "trailing ",
            "CON",
            "nul.txt",
            "aux",
            "com1.dat",
            "lpt9",
            "control\u{7}.txt",
        ];
        for key in rejected {
            assert!(
                corpus_key_problem(key).is_some(),
                "the portability guard accepted {key:?}, which Windows rejects"
            );
        }

        // And it accepts every shape the corpus itself uses.
        assert_corpus_is_portable(&[
            "a.txt",
            "dir/sub/y.txt",
            "sp ace.txt",
            "unicode-é.txt",
            "hash#x.txt",
            "a!x.txt",
            "a.b",
        ]);
    }
    /// Check one page of the ordered walk against a page cut out of a full scan of the same bucket.
    async fn assert_page_matches_a_full_scan(fs: &FileSystem, bucket_root: &Path, query: &ListingQuery<'_>) {
        let page = fs.list_page(bucket_root, query).await.unwrap();
        let oracle = build_page(fs.list_objects_full_scan(bucket_root, query).await.unwrap(), query);
        let case = format!(
            "prefix={:?} delimiter={:?} start_after={:?} max_keys={}",
            query.prefix, query.delimiter, query.start_after, query.max_keys
        );

        let objects = |page: &ListingPage| -> Vec<String> {
            page.objects
                .iter()
                .map(|object| object.key.clone().unwrap_or_default())
                .collect()
        };
        let prefixes = |page: &ListingPage| -> Vec<String> {
            page.common_prefixes
                .iter()
                .map(|prefix| prefix.prefix.clone().unwrap_or_default())
                .collect()
        };

        assert_eq!(objects(&page), objects(&oracle), "objects disagree for {case}");
        assert_eq!(prefixes(&page), prefixes(&oracle), "common prefixes disagree for {case}");
        assert_eq!(page.is_truncated, oracle.is_truncated, "is_truncated disagrees for {case}");
        assert_eq!(
            page.next_continuation_token, oracle.next_continuation_token,
            "next continuation token disagrees for {case}"
        );
        assert_eq!(page.key_count, oracle.key_count, "key count disagrees for {case}");
        for (ours, theirs) in page.objects.iter().zip(oracle.objects.iter()) {
            assert_eq!(ours.size, theirs.size, "size disagrees for {case}");
            assert_eq!(ours.last_modified, theirs.last_modified, "last modified disagrees for {case}");
        }

        // The walk itself must stay ordered and stop within one item of the page.
        let items = fs.list_objects_ordered(bucket_root, query).await.unwrap();
        assert!(items.len() <= query.limit(), "the walk collected more than a page for {case}");
        for pair in items.windows(2) {
            assert!(
                pair[0].key() < pair[1].key(),
                "the walk is out of key order for {case}: {:?} then {:?}",
                pair[0].key(),
                pair[1].key()
            );
        }
    }

    /// The ordered walk must agree with a full scan for every query shape.
    #[tokio::test(flavor = "current_thread")]
    async fn the_ordered_walk_agrees_with_a_full_scan() {
        let root = env::temp_dir().join(format!("s3s-fs-list-diff-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let _root = TestRoot(root.clone());
        let fs = FileSystem::new(&root).unwrap();

        // The corpus has to be storable everywhere the tests run: `#` is a legal name character
        // on every platform, while `::` and `"` are not (the keys that use them are added below
        // only where the file system accepts them).
        let mut keys: Vec<&str> = vec![
            "a.txt",
            "a/b.txt",
            "a/c/d.txt",
            "a0.txt",
            "ab.txt",
            "a!x.txt",
            "a.b",
            "b",
            "B.txt",
            "z.txt",
            "dir/x.txt",
            "dir/sub/y.txt",
            "dir2/z.txt",
            "sp ace.txt",
            "unicode-é.txt",
            "hash#x.txt",
            "hash#y.txt",
            "hash#sub/z.txt",
            "other.txt",
            "prefix.txt",
            "pre/fix.txt",
            "pre/fix/deep.txt",
            "same0",
            "same/child.txt",
            "samely.txt",
        ];
        // A guard rather than a comment: a name Windows rejects would otherwise fail the Windows CI
        // leg, which only runs after a push.
        assert_corpus_is_portable(&keys);
        // Windows rejects a `"` or a `:` in a name, so the keys that put the delimiter inside one are
        // only created where the file system allows them. Both sides of the comparison read the same
        // tree, so the rest of the matrix stays meaningful either way.
        #[cfg(unix)]
        keys.extend(["quote\".txt", "delim::x.txt", "delim::y.txt", "delim::sub/z.txt"]);
        let bucket_root = write_bucket(&root, "bucket", &keys);
        // An empty directory must contribute nothing.
        std::fs::create_dir_all(bucket_root.join("empty-dir/inner")).unwrap();

        let prefixes = [
            "",
            "a",
            "a/",
            "a/b",
            "dir",
            "dir/",
            "pre/fix",
            "delim",
            "delim::",
            "hash",
            "hash#",
            "nonexistent",
            "z",
            "same",
        ];
        let delimiters: [Option<&str>; 7] = [None, Some("/"), Some("#"), Some("##"), Some("::"), Some("x"), Some(".")];
        let markers: [Option<&str>; 7] = [
            None,
            Some("a"),
            Some("a/b.txt"),
            Some("delim::x.txt"),
            Some("hash#x.txt"),
            Some("same/child.txt"),
            Some("zzz"),
        ];
        let max_keys: [usize; 5] = [0, 1, 2, 3, 1000];

        for prefix in prefixes {
            for delimiter in delimiters {
                for start_after in markers {
                    for max_keys in max_keys {
                        let query = ListingQuery {
                            prefix,
                            delimiter,
                            start_after,
                            max_keys,
                        };
                        assert_page_matches_a_full_scan(&fs, &bucket_root, &query).await;
                    }
                }
            }
        }
    }

    /// One page must cost one page, not the whole bucket.
    #[tokio::test(flavor = "current_thread")]
    async fn the_ordered_walk_stops_after_one_page() {
        let root = env::temp_dir().join(format!("s3s-fs-list-stop-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let _root = TestRoot(root.clone());
        let fs = FileSystem::new(&root).unwrap();

        // 200 directories with 50 objects each: 10_000 objects in total.
        let bucket_root = root.join("bucket");
        for dir in 0..200 {
            let dir_path = bucket_root.join(format!("dir{dir:04}"));
            std::fs::create_dir_all(&dir_path).unwrap();
            for file in 0..50 {
                std::fs::write(dir_path.join(format!("obj{file:04}")), b"x").unwrap();
            }
        }

        let query = ListingQuery {
            prefix: "",
            delimiter: None,
            start_after: None,
            max_keys: 1,
        };

        listing_stats::reset();
        let page = fs.list_page(&bucket_root, &query).await.unwrap();
        let visited_ordered = listing_stats::visited();

        assert_eq!(page.objects.len(), 1, "a page holds max_keys objects");
        assert!(page.is_truncated, "another object exists, so the page is truncated");
        // The walk reads the root to order it and then one directory to fill the page: the cost is the
        // fanout along the path it follows, not the number of objects in the bucket.
        assert!(
            visited_ordered <= 300,
            "the ordered walk visited {visited_ordered} entries, which is not a bounded prefix of the bucket"
        );

        listing_stats::reset();
        let all = fs.list_objects_full_scan(&bucket_root, &query).await.unwrap();
        let visited_full = listing_stats::visited();

        assert_eq!(all.len(), 10_000, "the full scan sees every object");
        assert!(visited_full >= 10_000, "the full scan visits the whole bucket: {visited_full}");
    }

    /// A subtree the prefix cannot match must not be entered at all.
    #[tokio::test(flavor = "current_thread")]
    async fn the_ordered_walk_skips_subtrees_the_prefix_cannot_match() {
        let root = env::temp_dir().join(format!("s3s-fs-list-prune-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let _root = TestRoot(root.clone());
        let fs = FileSystem::new(&root).unwrap();

        let bucket_root = root.join("bucket");
        let dir = bucket_root.join("aaa");
        std::fs::create_dir_all(&dir).unwrap();
        for file in 0..1000 {
            std::fs::write(dir.join(format!("obj{file:04}")), b"x").unwrap();
        }

        let query = ListingQuery {
            prefix: "bbb/",
            delimiter: None,
            start_after: None,
            max_keys: 1000,
        };

        listing_stats::reset();
        let page = fs.list_page(&bucket_root, &query).await.unwrap();
        let visited_ordered = listing_stats::visited();

        assert_eq!(page.key_count, 0, "no key can match the requested prefix");
        assert!(
            visited_ordered <= 4,
            "the walk entered a subtree that cannot match the prefix: {visited_ordered} entries"
        );

        listing_stats::reset();
        let all = fs.list_objects_full_scan(&bucket_root, &query).await.unwrap();
        assert_eq!(all.len(), 0, "no item can match the requested prefix");
        assert!(
            listing_stats::visited() >= 1000,
            "the full scan reads the subtree anyway: {} entries",
            listing_stats::visited()
        );
    }
}
