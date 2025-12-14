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

use tokio::fs;
use tokio::fs::File;
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufWriter};

use path_absolutize::Absolutize;
use uuid::Uuid;

#[derive(Debug)]
pub struct FileSystem {
    pub(crate) root: PathBuf,
    tmp_file_counter: AtomicU64,
}

pub(crate) type InternalInfo = serde_json::Map<String, serde_json::Value>;

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
}

impl ObjectAttributes {
    /// Convert expires Timestamp to String for storage
    pub fn set_expires_timestamp(&mut self, expires: Option<dto::Timestamp>) {
        self.expires = expires.map(|ts| {
            let mut buf = Vec::new();
            ts.format(dto::TimestampFormat::DateTime, &mut buf).ok();
            String::from_utf8_lossy(&buf).into_owned()
        });
    }

    /// Parse expires String back to Timestamp
    pub fn get_expires_timestamp(&self) -> Option<dto::Timestamp> {
        self.expires
            .as_ref()
            .and_then(|s| dto::Timestamp::parse(dto::TimestampFormat::DateTime, s).ok())
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

    pub(crate) fn resolve_upload_part_path(&self, upload_id: Uuid, part_number: PartNumber) -> Result<PathBuf> {
        self.resolve_abs_path(format!(".upload_id-{upload_id}.part-{part_number}"))
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
    pub(crate) fn get_metadata_path(&self, bucket: &str, key: &str, upload_id: Option<Uuid>) -> Result<PathBuf> {
        let encode = |s: &str| base64_simd::URL_SAFE_NO_PAD.encode_to_string(s);
        let u_ext = upload_id.map(|u| format!(".upload-{u}")).unwrap_or_default();
        let file_path = format!(".bucket-{}.object-{}{u_ext}.metadata.json", encode(bucket), encode(key));
        self.resolve_abs_path(file_path)
    }

    pub(crate) fn get_internal_info_path(&self, bucket: &str, key: &str) -> Result<PathBuf> {
        let encode = |s: &str| base64_simd::URL_SAFE_NO_PAD.encode_to_string(s);
        let file_path = format!(".bucket-{}.object-{}.internal.json", encode(bucket), encode(key));
        self.resolve_abs_path(file_path)
    }

    /// load object attributes from fs (with backward compatibility)
    pub(crate) async fn load_object_attributes(
        &self,
        bucket: &str,
        key: &str,
        upload_id: Option<Uuid>,
    ) -> Result<Option<ObjectAttributes>> {
        let path = self.get_metadata_path(bucket, key, upload_id)?;
        if path.exists().not() {
            return Ok(None);
        }
        let content = fs::read(&path).await?;

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

    /// load metadata from fs (deprecated - use `load_object_attributes`)
    #[allow(dead_code)]
    pub(crate) async fn load_metadata(&self, bucket: &str, key: &str, upload_id: Option<Uuid>) -> Result<Option<dto::Metadata>> {
        let attrs = self.load_object_attributes(bucket, key, upload_id).await?;
        Ok(attrs.and_then(|a| a.user_metadata))
    }

    /// save metadata to fs (deprecated - use `save_object_attributes`)
    #[allow(dead_code)]
    pub(crate) async fn save_metadata(
        &self,
        bucket: &str,
        key: &str,
        metadata: &dto::Metadata,
        upload_id: Option<Uuid>,
    ) -> Result<()> {
        let attrs = ObjectAttributes {
            user_metadata: Some(metadata.clone()),
            ..Default::default()
        };
        self.save_object_attributes(bucket, key, &attrs, upload_id).await
    }

    /// remove metadata from fs
    pub(crate) fn delete_metadata(&self, bucket: &str, key: &str, upload_id: Option<Uuid>) -> Result<()> {
        let path = self.get_metadata_path(bucket, key, upload_id)?;
        std::fs::remove_file(path)?;
        Ok(())
    }

    pub(crate) async fn load_internal_info(&self, bucket: &str, key: &str) -> Result<Option<InternalInfo>> {
        let path = self.get_internal_info_path(bucket, key)?;
        if path.exists().not() {
            return Ok(None);
        }
        let content = fs::read(&path).await?;
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

    /// get md5 sum
    pub(crate) async fn get_md5_sum(&self, bucket: &str, key: &str) -> Result<String> {
        let object_path = self.get_object_path(bucket, key)?;
        let mut file = File::open(&object_path).await?;
        let mut buf = vec![0; 65536];
        let mut md5_hash = Md5::new();
        loop {
            let nread = file.read(&mut buf).await?;
            if nread == 0 {
                break;
            }
            md5_hash.update(&buf[..nread]);
        }
        Ok(hex(md5_hash.finalize()))
    }

    fn get_upload_info_path(&self, upload_id: &Uuid) -> Result<PathBuf> {
        self.resolve_abs_path(format!(".upload-{upload_id}.json"))
    }

    pub(crate) async fn create_upload_id(&self, cred: Option<&Credentials>) -> Result<Uuid> {
        let upload_id = Uuid::new_v4();
        let upload_info_path = self.get_upload_info_path(&upload_id)?;

        let ak: Option<&str> = cred.map(|c| c.access_key.as_str());

        let content = serde_json::to_vec(&ak)?;
        let mut file_writer = self.prepare_file_write(&upload_info_path).await?;
        file_writer.writer().write_all(&content).await?;
        file_writer.writer().flush().await?;
        file_writer.done().await?;

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

    pub(crate) async fn delete_upload_id(&self, upload_id: &Uuid) -> Result<()> {
        let upload_info_path = self.get_upload_info_path(upload_id)?;
        if upload_info_path.exists() {
            fs::remove_file(&upload_info_path).await?;
        }
        Ok(())
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
