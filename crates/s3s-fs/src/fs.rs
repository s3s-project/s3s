// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

use crate::error::*;
use crate::utils::hex;

use s3s::auth::Credentials;
use s3s::crypto::Checksum;
use s3s::crypto::Md5;
use s3s::dto;
use s3s::dto::PartNumber;

use std::env;
use std::ops::Not;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::SystemTime;

use tokio::fs;
use tokio::fs::File;
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufWriter};

use path_absolutize::Absolutize;
use tracing::warn;
use uuid::Uuid;

#[derive(Debug)]
pub struct FileSystem {
    pub(crate) root: PathBuf,
    tmp_file_counter: AtomicU64,
}

pub(crate) type InternalInfo = serde_json::Map<String, serde_json::Value>;

/// Read a sidecar file, treating a missing one as absent and every other failure as an error.
///
/// A caller used to check `exists()` before reading, which cost a `stat` on every lookup and turned
/// every read failure into `None`. Reading once maps `NotFound` to `None` and lets a real failure
/// (a permission problem, a directory in the way) surface as an error instead of a silent absence.
async fn read_sidecar(path: &Path) -> Result<Option<Vec<u8>>> {
    match fs::read(path).await {
        Ok(content) => Ok(Some(content)),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err.into()),
    }
}

/// Stores standard object attributes alongside user metadata
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub(crate) struct ObjectAttributes {
    /// User-defined metadata (x-amz-meta-*)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_metadata: Option<dto::Metadata>,

    /// Standard object attributes
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content_encoding: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content_disposition: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content_language: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_control: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub website_redirect_location: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub checksum_algorithm: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub checksum_type: Option<String>,
}

/// Name of the directory that owns the state of every in-progress multipart upload.
///
/// One upload occupies one directory, named after its upload ID: `info.json` records the upload,
/// `attributes.json` holds its object attributes, and `part-<n>` / `part-<n>.json` hold the parts
/// and their metadata. Bounding the state to a directory keeps listing the parts and aborting the
/// upload off the file system root.
const UPLOADS_DIR: &str = ".uploads";

/// Identity of an object body: its size and modification time.
///
/// Hashing reads the file that was opened. When the object is replaced while that read runs, the
/// digest describes the old body, so it must not be stored as the `ETag` of the new one.
#[derive(PartialEq)]
struct FileIdentity {
    len: u64,
    modified: Option<SystemTime>,
}

impl FileIdentity {
    fn of(metadata: &std::fs::Metadata) -> Self {
        Self {
            len: metadata.len(),
            modified: metadata.modified().ok(),
        }
    }
}

fn clean_old_tmp_files(root: &Path) -> std::io::Result<()> {
    let entries = match std::fs::read_dir(root) {
        Ok(entries) => Ok(entries),
        Err(ref io_err) if io_err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(io_err) => Err(io_err),
    }?;
    for entry in entries {
        let entry = entry?;
        let file_name = entry.file_name();
        let Some(file_name) = file_name.to_str() else { continue };
        // See `FileSystem::prepare_file_write`
        if file_name.starts_with(".tmp.") && file_name.ends_with(".internal.part") {
            std::fs::remove_file(entry.path())?;
        }
    }
    Ok(())
}

impl FileSystem {
    pub fn new(root: impl AsRef<Path>) -> Result<Self> {
        let root = env::current_dir()?.join(root).canonicalize()?;
        clean_old_tmp_files(&root)?;
        let tmp_file_counter = AtomicU64::new(0);
        Ok(Self { root, tmp_file_counter })
    }

    pub(crate) fn resolve_abs_path(&self, path: impl AsRef<Path>) -> Result<PathBuf> {
        Ok(path.as_ref().absolutize_virtually(&self.root)?.into_owned())
    }

    /// resolve the directory that owns all state of one multipart upload
    pub(crate) fn get_upload_dir_path(&self, upload_id: &Uuid) -> Result<PathBuf> {
        self.resolve_abs_path(format!("{UPLOADS_DIR}/{upload_id}"))
    }

    /// resolve the upload record path under the virtual root
    fn get_upload_info_path(&self, upload_id: &Uuid) -> Result<PathBuf> {
        self.resolve_abs_path(format!("{UPLOADS_DIR}/{upload_id}/info.json"))
    }

    /// resolve the upload-scoped object attributes path under the virtual root
    fn get_upload_attributes_path(&self, upload_id: &Uuid) -> Result<PathBuf> {
        self.resolve_abs_path(format!("{UPLOADS_DIR}/{upload_id}/attributes.json"))
    }

    pub(crate) fn resolve_upload_part_path(&self, upload_id: Uuid, part_number: PartNumber) -> Result<PathBuf> {
        self.resolve_abs_path(format!("{UPLOADS_DIR}/{upload_id}/part-{part_number}"))
    }

    /// resolve object path under the virtual root
    pub(crate) fn get_object_path(&self, bucket: &str, key: &str) -> Result<PathBuf> {
        let dir = Path::new(&bucket);
        let file_path = Path::new(&key);
        self.resolve_abs_path(dir.join(file_path))
    }

    /// resolve bucket path under the virtual root
    pub(crate) fn get_bucket_path(&self, bucket: &str) -> Result<PathBuf> {
        let dir = Path::new(&bucket);
        self.resolve_abs_path(dir)
    }

    /// resolve metadata path under the virtual root (custom format)
    ///
    /// Upload-scoped attributes live in the upload directory, so an upload ID maps to one attributes
    /// file regardless of the bucket and key the caller passes.
    pub(crate) fn get_metadata_path(&self, bucket: &str, key: &str, upload_id: Option<Uuid>) -> Result<PathBuf> {
        if let Some(upload_id) = upload_id {
            return self.get_upload_attributes_path(&upload_id);
        }
        let encode = |s: &str| base64_simd::URL_SAFE_NO_PAD.encode_to_string(s);
        let file_path = format!(".bucket-{}.object-{}.metadata.json", encode(bucket), encode(key));
        self.resolve_abs_path(file_path)
    }

    pub(crate) fn get_internal_info_path(&self, bucket: &str, key: &str) -> Result<PathBuf> {
        let encode = |s: &str| base64_simd::URL_SAFE_NO_PAD.encode_to_string(s);
        let file_path = format!(".bucket-{}.object-{}.internal.json", encode(bucket), encode(key));
        self.resolve_abs_path(file_path)
    }

    pub(crate) fn get_upload_part_info_path(&self, upload_id: Uuid, part_number: PartNumber) -> Result<PathBuf> {
        self.resolve_abs_path(format!("{UPLOADS_DIR}/{upload_id}/part-{part_number}.json"))
    }

    /// load object attributes from fs (with backward compatibility)
    pub(crate) async fn load_object_attributes(
        &self,
        bucket: &str,
        key: &str,
        upload_id: Option<Uuid>,
    ) -> Result<Option<ObjectAttributes>> {
        let path = self.get_metadata_path(bucket, key, upload_id)?;
        let Some(content) = read_sidecar(&path).await? else {
            return Ok(None);
        };

        // Try to deserialize as ObjectAttributes first (new format)
        if let Ok(attrs) = serde_json::from_slice::<ObjectAttributes>(&content) {
            return Ok(Some(attrs));
        }

        // Fall back to old format (just user metadata)
        if let Ok(user_metadata) = serde_json::from_slice::<dto::Metadata>(&content) {
            return Ok(Some(ObjectAttributes {
                user_metadata: Some(user_metadata),
                ..Default::default()
            }));
        }

        Ok(None)
    }

    /// save object attributes to fs
    pub(crate) async fn save_object_attributes(
        &self,
        bucket: &str,
        key: &str,
        attrs: &ObjectAttributes,
        upload_id: Option<Uuid>,
    ) -> Result<()> {
        let path = self.get_metadata_path(bucket, key, upload_id)?;
        let content = serde_json::to_vec(attrs)?;
        let mut file_writer = self.prepare_file_write(&path).await?;
        file_writer.writer().write_all(&content).await?;
        file_writer.writer().flush().await?;
        file_writer.done().await?;
        Ok(())
    }

    pub(crate) async fn load_internal_info(&self, bucket: &str, key: &str) -> Result<Option<InternalInfo>> {
        let path = self.get_internal_info_path(bucket, key)?;
        let Some(content) = read_sidecar(&path).await? else {
            return Ok(None);
        };
        let map = serde_json::from_slice(&content)?;
        Ok(Some(map))
    }

    pub(crate) async fn save_internal_info(&self, bucket: &str, key: &str, info: &InternalInfo) -> Result<()> {
        let path = self.get_internal_info_path(bucket, key)?;
        let content = serde_json::to_vec(info)?;
        let mut file_writer = self.prepare_file_write(&path).await?;
        file_writer.writer().write_all(&content).await?;
        file_writer.writer().flush().await?;
        file_writer.done().await?;
        Ok(())
    }

    pub(crate) async fn load_upload_part_info(&self, upload_id: Uuid, part_number: PartNumber) -> Result<Option<InternalInfo>> {
        let path = self.get_upload_part_info_path(upload_id, part_number)?;
        if path.exists().not() {
            return Ok(None);
        }
        let content = fs::read(&path).await?;
        let map = serde_json::from_slice(&content)?;
        Ok(Some(map))
    }

    pub(crate) async fn save_upload_part_info(
        &self,
        upload_id: Uuid,
        part_number: PartNumber,
        info: &InternalInfo,
    ) -> Result<()> {
        let path = self.get_upload_part_info_path(upload_id, part_number)?;
        let content = serde_json::to_vec(info)?;
        let mut file_writer = self.prepare_file_write(&path).await?;
        file_writer.writer().write_all(&content).await?;
        file_writer.writer().flush().await?;
        file_writer.done().await?;
        Ok(())
    }

    /// get md5 sum
    ///
    /// The computed value is the object's `ETag` when nothing else is stored for it, so it is also
    /// written back as `e_tag` and the next request reads it instead of hashing the whole body
    /// again.
    ///
    /// The value is remembered only while the object keeps the size and modification time it had
    /// when it was opened. Without that check a request racing with a replacement would pin an `ETag`
    /// that never matches the new body, while an unremembered value only stays stale until the next
    /// request, as it did before.
    pub(crate) async fn get_md5_sum(&self, bucket: &str, key: &str) -> Result<String> {
        let object_path = self.get_object_path(bucket, key)?;
        let mut file = File::open(&object_path).await?;
        let opened_identity = FileIdentity::of(&file.metadata().await?);
        let mut buf = vec![0; 65536];
        let mut md5_hash = Md5::new();
        loop {
            let nread = file.read(&mut buf).await?;
            if nread == 0 {
                break;
            }
            md5_hash.update(&buf[..nread]);
        }
        let md5_sum = hex(md5_hash.finalize());

        match fs::metadata(&object_path).await {
            Ok(current) if FileIdentity::of(&current) == opened_identity => {
                self.remember_e_tag(bucket, key, &md5_sum).await;
            }
            Ok(_) => {
                warn!(bucket, key, "not remembering the ETag: the object changed while it was hashed");
            }
            Err(err) => {
                warn!(bucket, key, error = ?err, "not remembering the ETag: the object could not be checked");
            }
        }

        Ok(md5_sum)
    }

    /// Write a computed `e_tag` into the object's `internal.json`, best effort.
    ///
    /// The sidecar is re-read here and merged, so a checksum that a concurrent write stored in the
    /// meantime is kept; only a missing `e_tag` is filled in. The caller already holds the value it
    /// needed, so any failure is logged and ignored instead of changing the response. A concurrent
    /// writer can still replace the sidecar between this read and the write, in which case the
    /// later write wins, exactly as it does for two writers of the same object today.
    async fn remember_e_tag(&self, bucket: &str, key: &str, e_tag: &str) {
        let result: Result<()> = async {
            let path = self.get_internal_info_path(bucket, key)?;
            let mut info = match fs::read(&path).await {
                Ok(content) => serde_json::from_slice::<InternalInfo>(&content)?,
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => InternalInfo::default(),
                Err(err) => return Err(err.into()),
            };
            if crate::checksum::load_e_tag(&info).is_some() {
                return Ok(());
            }
            crate::checksum::save_e_tag(&mut info, e_tag);
            self.save_internal_info(bucket, key, &info).await
        }
        .await;

        if let Err(err) = result {
            warn!(bucket, key, error = ?err, "failed to remember the computed ETag");
        }
    }

    pub(crate) async fn create_upload_id(&self, cred: Option<&Credentials>) -> Result<Uuid> {
        let upload_id = Uuid::new_v4();
        let upload_dir_path = self.get_upload_dir_path(&upload_id)?;
        fs::create_dir_all(&upload_dir_path).await?;

        let upload_info_path = self.get_upload_info_path(&upload_id)?;
        let ak: Option<&str> = cred.map(|c| c.access_key.as_str());
        let content = serde_json::to_vec(&ak)?;

        // An upload directory without its record is not a live upload. Remove the directory again
        // when writing the record fails, so a failed create leaves nothing behind.
        let write_result: Result<()> = async {
            let mut file_writer = self.prepare_file_write(&upload_info_path).await?;
            file_writer.writer().write_all(&content).await?;
            file_writer.writer().flush().await?;
            file_writer.done().await?;
            Ok(())
        }
        .await;
        if let Err(err) = write_result {
            let _ = fs::remove_dir_all(&upload_dir_path).await;
            return Err(err);
        }

        Ok(upload_id)
    }

    pub(crate) async fn verify_upload_id(&self, cred: Option<&Credentials>, upload_id: &Uuid) -> Result<bool> {
        let upload_info_path = self.get_upload_info_path(upload_id)?;
        if upload_info_path.exists().not() {
            return Ok(false);
        }

        let content = fs::read(&upload_info_path).await?;
        let ak: Option<String> = serde_json::from_slice(&content)?;

        Ok(ak.as_deref() == cred.map(|c| c.access_key.as_str()))
    }

    /// Remove the directory that owns an upload, if it still exists.
    ///
    /// The upload record is removed last: a cleanup that fails half-way leaves an upload that
    /// [`FileSystem::verify_upload_id`] still accepts, so aborting again can finish the job instead of
    /// stranding the directory. Removing an upload that is already gone is not an error either, so a
    /// cleanup can be retried.
    pub(crate) async fn delete_upload_id(&self, upload_id: &Uuid) -> Result<()> {
        let upload_dir_path = self.get_upload_dir_path(upload_id)?;
        let mut iter = match fs::read_dir(&upload_dir_path).await {
            Ok(iter) => iter,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(err) => return Err(err.into()),
        };

        let upload_info_path = self.get_upload_info_path(upload_id)?;

        while let Some(entry) = iter.next_entry().await? {
            let path = entry.path();
            if path == upload_info_path {
                continue;
            }
            let removed = if entry.file_type().await?.is_dir() {
                fs::remove_dir_all(&path).await
            } else {
                fs::remove_file(&path).await
            };
            match removed {
                Ok(()) => {}
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                Err(err) => return Err(err.into()),
            }
        }
        drop(iter);

        match fs::remove_file(&upload_info_path).await {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => return Err(err.into()),
        }

        match fs::remove_dir(&upload_dir_path).await {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(err) => Err(err.into()),
        }
    }

    /// Write to the filesystem atomically.
    /// This is done by first writing to a temporary location and then moving the file.
    pub(crate) async fn prepare_file_write<'a>(&self, path: &'a Path) -> Result<FileWriter<'a>> {
        let tmp_name = format!(".tmp.{}.internal.part", self.tmp_file_counter.fetch_add(1, Ordering::SeqCst));
        let tmp_path = self.resolve_abs_path(tmp_name)?;
        let file = File::create(&tmp_path).await?;
        let writer = BufWriter::new(file);
        Ok(FileWriter {
            tmp_path,
            dest_path: path,
            writer,
            clean_tmp: true,
        })
    }
}

pub(crate) struct FileWriter<'a> {
    tmp_path: PathBuf,
    dest_path: &'a Path,
    writer: BufWriter<File>,
    clean_tmp: bool,
}

impl<'a> FileWriter<'a> {
    pub(crate) fn tmp_path(&self) -> &Path {
        &self.tmp_path
    }

    pub(crate) fn dest_path(&self) -> &'a Path {
        self.dest_path
    }

    pub(crate) fn writer(&mut self) -> &mut BufWriter<File> {
        &mut self.writer
    }

    pub(crate) async fn done(mut self) -> Result<()> {
        if let Some(final_dir_path) = self.dest_path().parent() {
            fs::create_dir_all(&final_dir_path).await?;
        }

        fs::rename(&self.tmp_path, self.dest_path()).await?;
        self.clean_tmp = false;
        Ok(())
    }
}

impl Drop for FileWriter<'_> {
    fn drop(&mut self) {
        if self.clean_tmp {
            let _ = std::fs::remove_file(&self.tmp_path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::{Arc, Barrier};

    struct TestRoot(PathBuf);

    impl Drop for TestRoot {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn concurrent_upload_directory_deletion_is_idempotent() -> Result<()> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .max_blocking_threads(1)
            .build()?;

        runtime.block_on(async {
            let root = env::temp_dir().join(format!("s3s-fs-upload-cleanup-{}", Uuid::new_v4()));
            std::fs::create_dir_all(&root)?;
            let _root = TestRoot(root.clone());
            let file_system = FileSystem::new(&root)?;
            let upload_id = file_system.create_upload_id(None).await?;

            // Occupy the sole blocking worker so both deletions reach the file system before either runs.
            let barrier = Arc::new(Barrier::new(2));
            let worker_barrier = Arc::clone(&barrier);
            let blocker = tokio::task::spawn_blocking(move || {
                worker_barrier.wait();
                worker_barrier.wait();
            });
            barrier.wait();

            let first = file_system.delete_upload_id(&upload_id);
            let second = file_system.delete_upload_id(&upload_id);
            tokio::pin!(first);
            tokio::pin!(second);
            let first_pending = futures::poll!(first.as_mut()).is_pending();
            let second_pending = futures::poll!(second.as_mut()).is_pending();
            barrier.wait();

            assert!(first_pending, "first deletion completed before reaching the blocking worker");
            assert!(second_pending, "second deletion completed before reaching the blocking worker");
            let first_result = first.await;
            let second_result = second.await;
            blocker.await.expect("blocking worker should finish");
            first_result?;
            second_result
        })
    }

    /// The upload record must survive a cleanup that fails, or the retry cannot find the upload.
    #[cfg(unix)]
    #[test]
    fn failed_upload_cleanup_keeps_the_record_for_a_retry() -> Result<()> {
        use std::os::unix::fs::PermissionsExt;

        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build()?;

        runtime.block_on(async {
            let root = env::temp_dir().join(format!("s3s-fs-upload-retry-{}", Uuid::new_v4()));
            std::fs::create_dir_all(&root)?;
            let _root = TestRoot(root.clone());
            let file_system = FileSystem::new(&root)?;
            let upload_id = file_system.create_upload_id(None).await?;
            file_system
                .save_upload_part_info(upload_id, 1, &InternalInfo::default())
                .await?;

            let upload_dir = file_system.get_upload_dir_path(&upload_id)?;
            let upload_info_path = file_system.get_upload_info_path(&upload_id)?;
            assert!(upload_info_path.is_file(), "the upload record must exist before the cleanup");

            // A read-only directory keeps its entries readable but refuses to remove them, which is
            // exactly how removing an upload fails.
            std::fs::set_permissions(&upload_dir, std::fs::Permissions::from_mode(0o555))?;
            let probe = upload_dir.join(".permission-probe");
            if std::fs::File::create(&probe).is_ok() {
                // This user bypasses file permissions, so the failure cannot be injected. Leave the
                // upload for the retry check below instead of asserting what the environment cannot do.
                std::fs::remove_file(&probe)?;
                std::fs::set_permissions(&upload_dir, std::fs::Permissions::from_mode(0o755))?;
                return Ok(());
            }

            file_system
                .delete_upload_id(&upload_id)
                .await
                .expect_err("a cleanup that cannot remove an entry must fail");
            assert!(
                upload_info_path.is_file(),
                "a failed cleanup must keep the upload record so that a retry can find the upload"
            );

            std::fs::set_permissions(&upload_dir, std::fs::Permissions::from_mode(0o755))?;
            file_system.delete_upload_id(&upload_id).await?;
            assert!(!upload_dir.exists(), "the retry must remove the upload directory");

            Ok(())
        })
    }

    /// A backfill that cannot read the sidecar must be swallowed, not turned into a failure.
    #[test]
    fn a_backfill_that_cannot_read_the_sidecar_is_ignored() -> Result<()> {
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build()?;

        runtime.block_on(async {
            let root = env::temp_dir().join(format!("s3s-fs-etag-backfill-{}", Uuid::new_v4()));
            std::fs::create_dir_all(&root)?;
            let _root = TestRoot(root.clone());
            let file_system = FileSystem::new(&root)?;

            // A directory at the sidecar path fails the read with an error that is not `NotFound`,
            // which the best-effort backfill has to swallow.
            let sidecar = file_system.get_internal_info_path("bucket", "key")?;
            std::fs::create_dir(&sidecar)?;
            file_system.remember_e_tag("bucket", "key", "0123456789abcdef").await;
            assert!(sidecar.is_dir(), "a failed backfill must leave the sidecar path alone");

            std::fs::remove_dir(&sidecar)?;
            file_system.remember_e_tag("bucket", "key", "0123456789abcdef").await;
            let info: InternalInfo = serde_json::from_slice(&std::fs::read(&sidecar)?)?;
            assert_eq!(
                crate::checksum::load_e_tag(&info).as_deref(),
                Some("0123456789abcdef"),
                "the retry must store the ETag"
            );

            Ok(())
        })
    }
}
