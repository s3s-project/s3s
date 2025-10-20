use crate::fs::FileSystem;
use crate::fs::InternalInfo;
use crate::utils::*;

use s3s::S3;
use s3s::S3Result;
use s3s::crypto::Checksum;
use s3s::crypto::Md5;
use s3s::dto::*;
use s3s::s3_error;
use s3s::{S3Request, S3Response};

use std::collections::VecDeque;
use std::io;
use std::ops::Neg;
use std::ops::Not;
use std::path::Component;
use std::path::{Path, PathBuf};

use tokio::fs;
use tokio::io::AsyncSeekExt;
use tokio_util::io::ReaderStream;

use futures::TryStreamExt;
use numeric_cast::NumericCast;
use stdx::default::default;
use tracing::debug;
use uuid::Uuid;

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
            CopySource::AccessPoint { .. } => return Err(s3_error!(NotImplemented)),
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

        if let Some(dir_path) = dst_path.parent() {
            try_!(fs::create_dir_all(&dir_path).await);
        }

        let file_metadata = try_!(fs::metadata(&src_path).await);
        let last_modified = Timestamp::from(try_!(file_metadata.modified()));

        let _ = try_!(fs::copy(&src_path, &dst_path).await);

        debug!(from = %src_path.display(), to = %dst_path.display(), "copy file");

        let src_metadata_path = self.get_metadata_path(bucket, key, None)?;
        if src_metadata_path.exists() {
            let dst_metadata_path = self.get_metadata_path(&input.bucket, &input.key, None)?;
            let _ = try_!(fs::copy(src_metadata_path, dst_metadata_path).await);
        }

        let md5_sum = self.get_md5_sum(bucket, key).await?;

        let copy_object_result = CopyObjectResult {
            e_tag: Some(ETag::Strong(md5_sum)),
            last_modified: Some(last_modified),
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
            return Err(s3_error!(NoSuchKey));
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
        let mut objects: Vec<(PathBuf, String)> = Vec::new();
        for object in input.delete.objects {
            let path = self.get_object_path(&input.bucket, &object.key)?;
            if path.exists() {
                objects.push((path, object.key));
            }
        }

        let mut deleted_objects: Vec<DeletedObject> = Vec::new();
        for (path, key) in objects {
            try_!(fs::remove_file(path).await);

            let deleted_object = DeletedObject {
                key: Some(key),
                ..Default::default()
            };

            deleted_objects.push(deleted_object);
        }

        let output = DeleteObjectsOutput {
            deleted: Some(deleted_objects),
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

        let body = bytes_stream(ReaderStream::with_capacity(file, 4096), content_length_usize);

        let object_metadata = self.load_metadata(&input.bucket, &input.key, None).await?;

        let md5_sum = self.get_md5_sum(&input.bucket, &input.key).await?;

        let info = self.load_internal_info(&input.bucket, &input.key).await?;
        let checksum = match &info {
            // S3 skips returning the checksum if a range is specified that is
            // less than the whole file
            Some(info) if content_length == file_len => crate::checksum::from_internal_info(info),
            _ => default(),
        };

        let output = GetObjectOutput {
            body: Some(StreamingBlob::wrap(body)),
            content_length: Some(content_length_i64),
            content_range,
            last_modified: Some(last_modified),
            metadata: object_metadata,
            e_tag: Some(ETag::Strong(md5_sum)),
            checksum_crc32: checksum.checksum_crc32,
            checksum_crc32c: checksum.checksum_crc32c,
            checksum_sha1: checksum.checksum_sha1,
            checksum_sha256: checksum.checksum_sha256,
            checksum_crc64nvme: checksum.checksum_crc64nvme,
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
            return Err(s3_error!(NoSuchBucket));
        }

        let file_metadata = try_!(fs::metadata(path).await);
        let last_modified = Timestamp::from(try_!(file_metadata.modified()));
        let file_len = file_metadata.len();

        let object_metadata = self.load_metadata(&input.bucket, &input.key, None).await?;

        // TODO: detect content type
        let content_type = mime::APPLICATION_OCTET_STREAM;

        let output = HeadObjectOutput {
            content_length: Some(try_!(i64::try_from(file_len))),
            content_type: Some(content_type),
            last_modified: Some(last_modified),
            metadata: object_metadata,
            ..Default::default()
        };
        Ok(S3Response::new(output))
    }

    #[tracing::instrument]
    async fn list_buckets(&self, _: S3Request<ListBucketsInput>) -> S3Result<S3Response<ListBucketsOutput>> {
        let mut buckets: Vec<Bucket> = Vec::new();
        let mut iter = try_!(fs::read_dir(&self.root).await);
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

        // Collect all matching objects and common prefixes
        let mut objects: Vec<Object> = default();
        let mut common_prefixes = std::collections::BTreeSet::new();

        if let Some(delimiter) = delimiter {
            self.list_objects_with_delimiter(&path, prefix, delimiter, &mut objects, &mut common_prefixes)
                .await?;
        } else {
            self.list_objects_recursive(&path, prefix, &mut objects).await?;
        }

        // Sort before filtering and limiting
        objects.sort_by(|lhs, rhs| {
            let lhs_key = lhs.key.as_deref().unwrap_or("");
            let rhs_key = rhs.key.as_deref().unwrap_or("");
            lhs_key.cmp(rhs_key)
        });

        // Apply start_after filter if provided
        if let Some(marker) = &input.start_after {
            objects.retain(|obj| obj.key.as_deref().unwrap_or("") > marker.as_str());
        }

        // Convert common_prefixes to sorted list
        let common_prefixes_list: Vec<CommonPrefix> = common_prefixes
            .into_iter()
            .map(|prefix| CommonPrefix { prefix: Some(prefix) })
            .collect();

        // Limit results to max_keys by interleaving objects and common_prefixes
        let mut result_objects = Vec::new();
        let mut result_prefixes = Vec::new();
        let mut total_count = 0;
        let max_keys_usize = usize::try_from(max_keys).unwrap_or(1000);

        let mut obj_idx = 0;
        let mut prefix_idx = 0;

        while total_count < max_keys_usize {
            let obj_key = objects.get(obj_idx).and_then(|o| o.key.as_deref());
            let prefix_key = common_prefixes_list.get(prefix_idx).and_then(|p| p.prefix.as_deref());

            match (obj_key, prefix_key) {
                (Some(ok), Some(pk)) => {
                    if ok < pk {
                        result_objects.push(objects[obj_idx].clone());
                        obj_idx += 1;
                    } else {
                        result_prefixes.push(common_prefixes_list[prefix_idx].clone());
                        prefix_idx += 1;
                    }
                    total_count += 1;
                }
                (Some(_), None) => {
                    result_objects.push(objects[obj_idx].clone());
                    obj_idx += 1;
                    total_count += 1;
                }
                (None, Some(_)) => {
                    result_prefixes.push(common_prefixes_list[prefix_idx].clone());
                    prefix_idx += 1;
                    total_count += 1;
                }
                (None, None) => break,
            }
        }

        let is_truncated = obj_idx < objects.len() || prefix_idx < common_prefixes_list.len();
        let key_count = try_!(i32::try_from(total_count));

        let contents = result_objects.is_empty().not().then_some(result_objects);
        let common_prefixes = result_prefixes.is_empty().not().then_some(result_prefixes);

        let output = ListObjectsV2Output {
            key_count: Some(key_count),
            max_keys: Some(max_keys),
            is_truncated: Some(is_truncated),
            contents,
            common_prefixes,
            delimiter: input.delimiter,
            encoding_type: input.encoding_type,
            name: Some(input.bucket),
            prefix: input.prefix,
            ..Default::default()
        };
        Ok(S3Response::new(output))
    }

    #[tracing::instrument]
    async fn put_object(&self, req: S3Request<PutObjectInput>) -> S3Result<S3Response<PutObjectOutput>> {
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
            ..
        } = input;

        let Some(body) = body else { return Err(s3_error!(IncompleteBody)) };

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
        if let Some(alg) = input.checksum_algorithm {
            match alg.as_str() {
                ChecksumAlgorithm::CRC32 => checksum.crc32 = Some(default()),
                ChecksumAlgorithm::CRC32C => checksum.crc32c = Some(default()),
                ChecksumAlgorithm::SHA1 => checksum.sha1 = Some(default()),
                ChecksumAlgorithm::SHA256 => checksum.sha256 = Some(default()),
                ChecksumAlgorithm::CRC64NVME => checksum.crc64nvme = Some(default()),
                _ => return Err(s3_error!(NotImplemented, "Unsupported checksum algorithm")),
            }
        }

        if key.ends_with('/') {
            if let Some(len) = content_length {
                if len > 0 {
                    return Err(s3_error!(UnexpectedContent, "Unexpected request body when creating a directory object."));
                }
            }
            let object_path = self.get_object_path(&bucket, &key)?;
            try_!(fs::create_dir_all(&object_path).await);
            let output = PutObjectOutput::default();
            return Ok(S3Response::new(output));
        }

        let object_path = self.get_object_path(&bucket, &key)?;
        let mut file_writer = self.prepare_file_write(&object_path).await?;

        let mut md5_hash = Md5::new();
        let stream = body.inspect_ok(|bytes| {
            md5_hash.update(bytes.as_ref());
            checksum.update(bytes.as_ref());
        });

        let size = copy_bytes(stream, file_writer.writer()).await?;
        file_writer.done().await?;

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

        if let Some(trailers) = req.trailing_headers {
            if let Some(trailers) = trailers.take() {
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

        debug!(path = %object_path.display(), ?size, %md5_sum, ?checksum, "write file");

        if let Some(ref metadata) = metadata {
            self.save_metadata(&bucket, &key, metadata, None).await?;
        }

        let mut info: InternalInfo = default();
        crate::checksum::modify_internal_info(&mut info, &checksum);
        self.save_internal_info(&bucket, &key, &info).await?;

        let output = PutObjectOutput {
            e_tag: Some(ETag::Strong(md5_sum)),
            checksum_crc32: checksum.checksum_crc32,
            checksum_crc32c: checksum.checksum_crc32c,
            checksum_sha1: checksum.checksum_sha1,
            checksum_sha256: checksum.checksum_sha256,
            checksum_crc64nvme: checksum.checksum_crc64nvme,
            ..Default::default()
        };
        Ok(S3Response::new(output))
    }

    #[tracing::instrument]
    async fn create_multipart_upload(
        &self,
        req: S3Request<CreateMultipartUploadInput>,
    ) -> S3Result<S3Response<CreateMultipartUploadOutput>> {
        let input = req.input;
        let upload_id = self.create_upload_id(req.credentials.as_ref()).await?;

        if let Some(ref metadata) = input.metadata {
            self.save_metadata(&input.bucket, &input.key, metadata, Some(upload_id))
                .await?;
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
        let UploadPartInput {
            body,
            upload_id,
            part_number,
            ..
        } = req.input;

        if part_number > 10_000 {
            return Err(s3_error!(
                InvalidArgument,
                "Part number must be an integer between 1 and 10000, inclusive"
            ));
        }

        let body = body.ok_or_else(|| s3_error!(IncompleteBody))?;

        let upload_id = Uuid::parse_str(&upload_id).map_err(|_| s3_error!(InvalidRequest))?;
        if self.verify_upload_id(req.credentials.as_ref(), &upload_id).await?.not() {
            return Err(s3_error!(AccessDenied));
        }

        let file_path = self.resolve_upload_part_path(upload_id, part_number)?;

        let mut md5_hash = Md5::new();
        let stream = body.inspect_ok(|bytes| md5_hash.update(bytes.as_ref()));

        let mut file_writer = self.prepare_file_write(&file_path).await?;
        let size = copy_bytes(stream, file_writer.writer()).await?;
        file_writer.done().await?;

        let md5_sum = hex(md5_hash.finalize());

        debug!(path = %file_path.display(), ?size, %md5_sum, "write file");

        let output = UploadPartOutput {
            e_tag: Some(ETag::Strong(md5_sum)),
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

        let (src_bucket, src_key) = match input.copy_source {
            CopySource::AccessPoint { .. } => return Err(s3_error!(NotImplemented)),
            CopySource::Bucket { ref bucket, ref key, .. } => (bucket, key),
        };
        let src_path = self.get_object_path(src_bucket, src_key)?;
        let dst_path = self.resolve_upload_part_path(upload_id, part_number)?;

        let mut src_file = fs::File::open(&src_path).await.map_err(|e| s3_error!(e, NoSuchKey))?;
        let file_len = try_!(src_file.metadata().await).len();

        let (start, end) = if let Some(copy_range) = &input.copy_source_range {
            if !copy_range.starts_with("bytes=") {
                return Err(s3_error!(InvalidArgument));
            }
            let range = &copy_range["bytes=".len()..];
            let parts: Vec<&str> = range.split('-').collect();
            if parts.len() != 2 {
                return Err(s3_error!(InvalidArgument));
            }

            let start: u64 = parts[0].parse().map_err(|_| s3_error!(InvalidArgument))?;
            let mut end = file_len - 1;
            if parts[1].is_empty().not() {
                end = parts[1].parse().map_err(|_| s3_error!(InvalidArgument))?;
            }
            (start, end)
        } else {
            (0, file_len - 1)
        };

        let content_length = end - start + 1;
        let content_length_usize = try_!(usize::try_from(content_length));

        let _ = try_!(src_file.seek(io::SeekFrom::Start(start)).await);
        let body = StreamingBlob::wrap(bytes_stream(ReaderStream::with_capacity(src_file, 4096), content_length_usize));

        let mut md5_hash = Md5::new();
        let stream = body.inspect_ok(|bytes| md5_hash.update(bytes.as_ref()));

        let mut file_writer = self.prepare_file_write(&dst_path).await?;
        let size = copy_bytes(stream, file_writer.writer()).await?;
        file_writer.done().await?;

        let md5_sum = hex(md5_hash.finalize());

        debug!(path = %dst_path.display(), ?size, %md5_sum, "write file");

        let output = UploadPartCopyOutput {
            copy_part_result: Some(CopyPartResult {
                e_tag: Some(ETag::Strong(md5_sum)),
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

        let mut parts: Vec<Part> = Vec::new();
        let mut iter = try_!(fs::read_dir(&self.root).await);

        let prefix = format!(".upload_id-{upload_id}");

        while let Some(entry) = try_!(iter.next_entry().await) {
            let file_type = try_!(entry.file_type().await);
            if file_type.is_file().not() {
                continue;
            }

            let file_name = entry.file_name();
            let Some(name) = file_name.to_str() else { continue };

            let Some(part_segment) = name.strip_prefix(&prefix) else { continue };
            let Some(part_number) = part_segment.strip_prefix(".part-") else { continue };
            let part_number = part_number.parse::<i32>().unwrap();

            let file_meta = try_!(entry.metadata().await);
            let last_modified = Timestamp::from(try_!(file_meta.modified()));
            let size = try_!(i64::try_from(file_meta.len()));

            let part = Part {
                last_modified: Some(last_modified),
                part_number: Some(part_number),
                size: Some(size),
                ..Default::default()
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
            ..
        } = req.input;

        let Some(multipart_upload) = multipart_upload else { return Err(s3_error!(InvalidPart)) };

        let upload_id = Uuid::parse_str(&upload_id).map_err(|_| s3_error!(InvalidRequest))?;
        if self.verify_upload_id(req.credentials.as_ref(), &upload_id).await?.not() {
            return Err(s3_error!(AccessDenied));
        }

        self.delete_upload_id(&upload_id).await?;

        if let Ok(Some(metadata)) = self.load_metadata(&bucket, &key, Some(upload_id)).await {
            self.save_metadata(&bucket, &key, &metadata, None).await?;
            let _ = self.delete_metadata(&bucket, &key, Some(upload_id));
        }

        let object_path = self.get_object_path(&bucket, &key)?;
        let mut file_writer = self.prepare_file_write(&object_path).await?;

        let mut cnt: i32 = 0;
        let total_parts_cnt = multipart_upload
            .parts
            .as_ref()
            .map(|parts| i32::try_from(parts.len()).expect("total number of parts must be <= 10000."))
            .unwrap_or_default();

        for part in multipart_upload.parts.into_iter().flatten() {
            let part_number = part
                .part_number
                .ok_or_else(|| s3_error!(InvalidRequest, "missing part number"))?;
            cnt += 1;
            if part_number != cnt {
                return Err(s3_error!(InvalidRequest, "invalid part order"));
            }

            let part_path = self.resolve_upload_part_path(upload_id, part_number)?;

            let mut reader = try_!(fs::File::open(&part_path).await);
            let size = try_!(tokio::io::copy(&mut reader, &mut file_writer.writer()).await);

            if part_number != total_parts_cnt && size < 5 * 1024 * 1024 {
                return Err(s3_error!(EntityTooSmall));
            }

            debug!(from = %part_path.display(), tmp = %file_writer.tmp_path().display(), to = %file_writer.dest_path().display(), ?size, "write file");
            try_!(fs::remove_file(&part_path).await);
        }
        file_writer.done().await?;

        let file_size = try_!(fs::metadata(&object_path).await).len();
        let md5_sum = self.get_md5_sum(&bucket, &key).await?;

        debug!(?md5_sum, path = %object_path.display(), size = ?file_size, "file md5 sum");

        let output = CompleteMultipartUploadOutput {
            // TODO: better example of AWS-like keep-alive behavior
            future: Some(Box::pin(async move {
                Ok(CompleteMultipartUploadOutput {
                    bucket: Some(bucket),
                    key: Some(key),
                    e_tag: Some(ETag::Strong(md5_sum)),
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

        let _ = self.delete_metadata(&bucket, &key, Some(upload_id));

        let prefix = format!(".upload_id-{upload_id}");
        let mut iter = try_!(fs::read_dir(&self.root).await);
        while let Some(entry) = try_!(iter.next_entry().await) {
            let file_type = try_!(entry.file_type().await);
            if file_type.is_file().not() {
                continue;
            }

            let file_name = entry.file_name();
            let Some(name) = file_name.to_str() else { continue };

            if name.starts_with(&prefix) {
                try_!(fs::remove_file(entry.path()).await);
            }
        }

        self.delete_upload_id(&upload_id).await?;

        debug!(bucket = %bucket, key = %key, upload_id = %upload_id, "multipart upload aborted");

        Ok(S3Response::new(AbortMultipartUploadOutput { ..Default::default() }))
    }
}

impl FileSystem {
    async fn list_objects_recursive(&self, bucket_root: &Path, prefix: &str, objects: &mut Vec<Object>) -> S3Result<()> {
        let mut dir_queue: VecDeque<PathBuf> = default();
        dir_queue.push_back(bucket_root.to_owned());
        let prefix_is_empty = prefix.is_empty();

        while let Some(dir) = dir_queue.pop_front() {
            let mut iter = try_!(fs::read_dir(dir).await);
            while let Some(entry) = try_!(iter.next_entry().await) {
                let file_type = try_!(entry.file_type().await);
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

                    let metadata = try_!(entry.metadata().await);
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
            let mut iter = try_!(fs::read_dir(dir).await);

            while let Some(entry) = try_!(iter.next_entry().await) {
                let file_type = try_!(entry.file_type().await);
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
                        let metadata = try_!(entry.metadata().await);
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
