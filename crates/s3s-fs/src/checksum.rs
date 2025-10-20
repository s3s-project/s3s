use crate::fs::InternalInfo;

use stdx::default::default;

pub fn modify_internal_info(info: &mut serde_json::Map<String, serde_json::Value>, checksum: &s3s::dto::Checksum) {
    if let Some(checksum_crc32) = &checksum.checksum_crc32 {
        info.insert("checksum_crc32".to_owned(), serde_json::Value::String(checksum_crc32.clone()));
    }
    if let Some(checksum_crc32c) = &checksum.checksum_crc32c {
        info.insert("checksum_crc32c".to_owned(), serde_json::Value::String(checksum_crc32c.clone()));
    }
    if let Some(checksum_sha1) = &checksum.checksum_sha1 {
        info.insert("checksum_sha1".to_owned(), serde_json::Value::String(checksum_sha1.clone()));
    }
    if let Some(checksum_sha256) = &checksum.checksum_sha256 {
        info.insert("checksum_sha256".to_owned(), serde_json::Value::String(checksum_sha256.clone()));
    }

    if let Some(checksum_crc64nvme) = &checksum.checksum_crc64nvme {
        info.insert("checksum_crc64nvme".to_owned(), serde_json::Value::String(checksum_crc64nvme.clone()));
    }
}

pub fn from_internal_info(info: &InternalInfo) -> s3s::dto::Checksum {
    let mut ans: s3s::dto::Checksum = default();
    if let Some(checksum_crc32) = info.get("checksum_crc32") {
        ans.checksum_crc32 = Some(checksum_crc32.as_str().unwrap().to_owned());
    }
    if let Some(checksum_crc32c) = info.get("checksum_crc32c") {
        ans.checksum_crc32c = Some(checksum_crc32c.as_str().unwrap().to_owned());
    }
    if let Some(checksum_sha1) = info.get("checksum_sha1") {
        ans.checksum_sha1 = Some(checksum_sha1.as_str().unwrap().to_owned());
    }
    if let Some(checksum_sha256) = info.get("checksum_sha256") {
        ans.checksum_sha256 = Some(checksum_sha256.as_str().unwrap().to_owned());
    }

    if let Some(checksum_crc64nvme) = info.get("checksum_crc64nvme") {
        ans.checksum_crc64nvme = Some(checksum_crc64nvme.as_str().unwrap().to_owned());
    }
    ans
}
