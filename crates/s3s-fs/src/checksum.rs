// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 Nugine

use crate::fs::InternalInfo;

use stdx::default::default;

pub fn save_e_tag(info: &mut serde_json::Map<String, serde_json::Value>, e_tag: &str) {
    info.insert("e_tag".to_owned(), serde_json::Value::String(e_tag.to_owned()));
}

pub fn load_e_tag(info: &serde_json::Map<String, serde_json::Value>) -> Option<String> {
    info.get("e_tag").and_then(|v| v.as_str()).map(str::to_owned)
}

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

    if let Some(checksum_sha512) = &checksum.checksum_sha512 {
        info.insert("checksum_sha512".to_owned(), serde_json::Value::String(checksum_sha512.clone()));
    }
    if let Some(checksum_md5) = &checksum.checksum_md5 {
        info.insert("checksum_md5".to_owned(), serde_json::Value::String(checksum_md5.clone()));
    }
    if let Some(checksum_xxhash64) = &checksum.checksum_xxhash64 {
        info.insert("checksum_xxhash64".to_owned(), serde_json::Value::String(checksum_xxhash64.clone()));
    }
    if let Some(checksum_xxhash3) = &checksum.checksum_xxhash3 {
        info.insert("checksum_xxhash3".to_owned(), serde_json::Value::String(checksum_xxhash3.clone()));
    }
    if let Some(checksum_xxhash128) = &checksum.checksum_xxhash128 {
        info.insert("checksum_xxhash128".to_owned(), serde_json::Value::String(checksum_xxhash128.clone()));
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

    if let Some(checksum_sha512) = info.get("checksum_sha512") {
        ans.checksum_sha512 = Some(checksum_sha512.as_str().unwrap().to_owned());
    }
    if let Some(checksum_md5) = info.get("checksum_md5") {
        ans.checksum_md5 = Some(checksum_md5.as_str().unwrap().to_owned());
    }
    if let Some(checksum_xxhash64) = info.get("checksum_xxhash64") {
        ans.checksum_xxhash64 = Some(checksum_xxhash64.as_str().unwrap().to_owned());
    }
    if let Some(checksum_xxhash3) = info.get("checksum_xxhash3") {
        ans.checksum_xxhash3 = Some(checksum_xxhash3.as_str().unwrap().to_owned());
    }
    if let Some(checksum_xxhash128) = info.get("checksum_xxhash128") {
        ans.checksum_xxhash128 = Some(checksum_xxhash128.as_str().unwrap().to_owned());
    }
    ans
}
