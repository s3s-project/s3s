// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

//! `ListObjectsV2` with the `MinIO` `metadata=true` extension.
//!
//! With the extension every `<Contents>` entry of the listing carries
//! `<UserTags>` and `<UserMetadata>`. The `aws-sdk-s3` output types have no
//! member for either element, so its typed deserializer drops them; the
//! generated proxy delegates the request to the official `MinIO` SDK (`minio`
//! crate) instead, which parses both. Every request that does not ask for the
//! extension keeps the `aws-sdk-s3` path.

use std::collections::HashMap;

use futures::StreamExt as _;
use minio::s3::MinioClient;
use minio::s3::multimap_ext::{Multimap, MultimapExt};
use minio::s3::response_traits::HasS3Fields as _;
use minio::s3::types::ListEntry;
use minio::s3::types::ToStream as _;

use s3s::S3Request;
use s3s::S3Response;
use s3s::S3Result;
use s3s::dto::CommonPrefix;
use s3s::dto::ETag;
use s3s::dto::EncodingType;
use s3s::dto::ListObjectsV2Input;
use s3s::dto::ListObjectsV2Output;
use s3s::dto::Object;
use s3s::dto::ObjectStorageClass;
use s3s::dto::ObjectUserMetadata;
use s3s::dto::Owner;
use s3s::dto::RequestCharged;
use s3s::dto::Timestamp;
use s3s::dto::TimestampFormat;
use s3s::s3_error;

/// Forwards a `ListObjectsV2` request with the `MinIO` `metadata=true` extension.
pub async fn list_objects_v2(
    minio: &MinioClient,
    req: S3Request<ListObjectsV2Input>,
) -> S3Result<S3Response<ListObjectsV2Output>> {
    let input = req.input;
    tracing::debug!(?input);

    let max_keys = match input.max_keys {
        Some(max_keys) => Some(u16::try_from(max_keys).map_err(|e| s3_error!(e, InvalidArgument, "max-keys out of range"))?),
        None => None,
    };

    // The client's `encoding-type` goes upstream verbatim: the `minio` crate would
    // otherwise always add `encoding-type=url` of its own.
    let mut extra_query_params = Multimap::default();
    if let Some(encoding_type) = input.encoding_type.as_ref() {
        extra_query_params.add("encoding-type", encoding_type.as_str());
    }

    // The builder is a typed state machine, so every parameter goes in one chain.
    let b = minio
        .list_objects(input.bucket.as_str())
        .map_err(|e| s3_error!(e, InvalidBucketName, "invalid bucket"))?
        // The extension is what this delegate exists for.
        .include_user_metadata(true)
        .extra_query_params(extra_query_params)
        .disable_url_encoding(true)
        .max_keys(max_keys)
        .fetch_owner(input.fetch_owner.unwrap_or_default())
        .prefix(input.prefix.clone())
        .delimiter(input.delimiter.clone())
        .recursive(recursive_listing(input.delimiter.as_deref()))
        .continuation_token(input.continuation_token.clone())
        .start_after(input.start_after.clone());

    // `ListObjects` auto-paginates, so only the first page is consumed: one client
    // request is one page, exactly like one `aws-sdk-s3` `send`.
    let mut stream = b.build().to_stream().await;
    let Some(page) = stream.next().await else {
        return Err(s3_error!(InternalError, "the upstream returned no listing page"));
    };
    let page = page.map_err(|e| super::minio_error::map_error("failed to list objects", e))?;

    // The `minio` crate decodes the listing values it parses, so the wire form has
    // to be restored before the client sees it: the client decodes unconditionally
    // whatever it receives.
    let encode = is_url_encoded(page.encoding_type.as_deref());

    let mut contents = Vec::with_capacity(page.contents.len());
    let mut common_prefixes = Vec::new();
    for entry in &page.contents {
        if entry.is_prefix {
            common_prefixes.push(CommonPrefix {
                prefix: Some(convert_value(&entry.name, encode)),
            });
        } else {
            contents.push(convert_entry(entry, encode)?);
        }
    }

    let output = ListObjectsV2Output {
        name: Some(page.name.clone()),
        prefix: page.prefix.as_deref().map(|value| convert_value(value, encode)),
        // `MinIO` does not escape the delimiter, so neither does the crate.
        delimiter: page.delimiter.clone(),
        max_keys: page.max_keys.map(i32::from),
        key_count: page.key_count.map(i32::from),
        continuation_token: page.continuation_token.clone(),
        is_truncated: Some(page.is_truncated),
        next_continuation_token: page.next_continuation_token.clone(),
        contents: Some(contents),
        common_prefixes: Some(common_prefixes),
        start_after: page.start_after.clone(),
        // Mirrors the encoding the backend reported, like the `aws-sdk-s3` path.
        encoding_type: page.encoding_type.clone().map(EncodingType::from),
        request_charged: page
            .headers()
            .get("x-amz-request-charged")
            .and_then(|value| value.to_str().ok())
            .map(|value| RequestCharged::from(value.to_owned())),
    };
    Ok(S3Response::new(output))
}

/// Converts one `<Contents>` entry, filling the same DTO fields as the
/// `aws-sdk-s3` conversion does and adding the two extension elements.
fn convert_entry(entry: &ListEntry, encode: bool) -> S3Result<Object> {
    Ok(Object {
        key: Some(convert_value(&entry.name, encode)),
        e_tag: entry.etag.as_deref().map(parse_etag).transpose()?,
        last_modified: entry.last_modified.map(convert_timestamp).transpose()?,
        // S3 object sizes stay far below `i64::MAX`, so the reinterpretation is exact.
        size: entry.size.map(u64::cast_signed),
        storage_class: entry.storage_class.clone().map(ObjectStorageClass::from),
        owner: convert_owner(entry),
        user_tags: entry.user_tags.as_ref().filter(|tags| !tags.is_empty()).map(format_tags),
        user_metadata: entry
            .user_metadata
            .as_ref()
            .filter(|metadata| !metadata.is_empty())
            .map(format_metadata),
        ..Default::default()
    })
}

/// Whether the upstream listing must be requested recursively: the `minio` crate
/// falls back to `delimiter=/` when neither a delimiter nor `recursive(true)` is
/// given, which would roll every nested key into a common prefix, so a request
/// without a delimiter must ask for a flat listing.
fn recursive_listing(delimiter: Option<&str>) -> bool {
    delimiter.is_none()
}

/// Whether a listing response is url-encoded: the backend reports the encoding it
/// applied, and the `minio` crate already escaped the values it parsed, so the
/// proxy re-encodes them exactly when the response is url-encoded. The request side
/// mirrors the client, so a request without `encoding-type` gets literal values
/// back and reports no encoding.
fn is_url_encoded(encoding_type: Option<&str>) -> bool {
    encoding_type == Some("url")
}

/// Restores the wire form of a listing value when the response is url-encoded.
fn convert_value(value: &str, encode: bool) -> String {
    if encode { encode_url(value) } else { value.to_owned() }
}

/// Escapes a listing value the way `MinIO` escapes it for `encoding-type=url`:
/// letters, digits and `-`, `_`, `.`, `/`, `*` stay literal, a space becomes
/// `+`, and every other UTF-8 byte becomes `%XX` with upper-case hex digits.
fn encode_url(value: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'/' | b'*' => out.push(char::from(byte)),
            b' ' => out.push('+'),
            byte => {
                out.push('%');
                out.push(char::from(HEX[usize::from(byte >> 4)]));
                out.push(char::from(HEX[usize::from(byte & 0x0f)]));
            }
        }
    }
    out
}

/// Parses the `ETag` exactly like the `aws-sdk-s3` conversion does: the backend
/// sends the quoted HTTP form.
fn parse_etag(etag: &str) -> S3Result<ETag> {
    ETag::parse_http_header(etag.as_bytes()).map_err(|e| s3_error!(e, InternalError, "invalid upstream etag"))
}

/// The `minio` crate models timestamps as `chrono` values; `s3s` keeps the
/// RFC 3339 text, so format the instant the way the backend sends it.
fn convert_timestamp(value: minio::s3::utils::UtcTime) -> S3Result<Timestamp> {
    let text = value.format("%Y-%m-%dT%H:%M:%S%.fZ").to_string();
    Timestamp::parse(TimestampFormat::DateTime, &text).map_err(|e| s3_error!(e, InternalError, "invalid upstream timestamp"))
}

fn convert_owner(entry: &ListEntry) -> Option<Owner> {
    let has_owner = entry.owner_id.is_some() || entry.owner_name.is_some();
    has_owner.then(|| Owner {
        id: entry.owner_id.clone(),
        display_name: entry.owner_name.clone(),
    })
}

/// Renders `<UserTags>`: the wire form is a single `k=v&k=v` string. The keys
/// are sorted so the output does not depend on the hash map iteration order.
fn format_tags(tags: &HashMap<String, String>) -> String {
    let mut pairs: Vec<(&str, &str)> = tags.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    pairs.sort_unstable();
    pairs.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join("&")
}

/// Renders `<UserMetadata>`: one element per entry. The entries are sorted so the
/// output does not depend on the hash map iteration order.
fn format_metadata(metadata: &HashMap<String, String>) -> ObjectUserMetadata {
    let mut entries: Vec<(String, String)> = metadata.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
    entries.sort_unstable();
    ObjectUserMetadata(entries)
}

#[cfg(test)]
mod tests {
    use super::*;

    use aws_sdk_s3::primitives::DateTime as AwsDateTime;
    use aws_sdk_s3::types::Object as AwsObject;
    use aws_sdk_s3::types::ObjectStorageClass as AwsObjectStorageClass;
    use aws_sdk_s3::types::Owner as AwsOwner;
    use minio::s3::utils::UtcTime;

    use crate::conv::try_from_aws;

    const KEY: &str = "a b/c+ünïcode.txt";
    const ETAG: &str = "\"5d41402abc4b2a76b9719d911017c592\"";

    /// The sample object as the `aws-sdk-s3` path sees it.
    fn aws_object() -> AwsObject {
        AwsObject::builder()
            .key(KEY)
            .e_tag(ETAG)
            .size(5)
            .last_modified(AwsDateTime::from_secs(1_700_000_000))
            .storage_class(AwsObjectStorageClass::Standard)
            .owner(AwsOwner::builder().id("owner-id").display_name("owner-name").build())
            .build()
    }

    /// The same object as the `minio` crate reports it.
    fn minio_entry() -> ListEntry {
        let last_modified: UtcTime = "2023-11-14T22:13:20Z".parse().expect("valid timestamp");
        ListEntry {
            name: KEY.to_owned(),
            last_modified: Some(last_modified),
            etag: Some(ETAG.to_owned()),
            owner_id: Some("owner-id".to_owned()),
            owner_name: Some("owner-name".to_owned()),
            size: Some(5),
            storage_class: Some("STANDARD".to_owned()),
            is_latest: false,
            version_id: None,
            user_metadata: None,
            user_tags: None,
            is_prefix: false,
            is_delete_marker: false,
            encoding_type: Some("url".to_owned()),
        }
    }

    /// The parity criterion: without the extension the two conversion paths
    /// produce the same DTO, field for field.
    #[test]
    fn entry_matches_the_aws_sdk_path() {
        let from_aws: Object = try_from_aws(aws_object()).expect("convert the aws object");
        let from_minio = convert_entry(&minio_entry(), false).expect("convert the minio entry");
        assert_eq!(from_aws, from_minio);
    }

    /// The escape table the backend uses under `encoding-type=url`, pinned per
    /// character class. The end-to-end comparison against the `aws-sdk-s3` path
    /// covers the rest of the listing.
    #[test]
    fn encodes_listing_values_like_the_backend() {
        assert_eq!(encode_url("pct%20name.txt"), "pct%2520name.txt");
        assert_eq!(encode_url("a b/c"), "a+b/c");
        assert_eq!(encode_url("a+b/c"), "a%2Bb/c");
        assert_eq!(encode_url("t~ilde.txt"), "t%7Eilde.txt");
        assert_eq!(encode_url("star*.txt"), "star*.txt");
        assert_eq!(encode_url("keep-_.()"), "keep-_.%28%29");
        assert_eq!(encode_url("uni-中.txt"), "uni-%E4%B8%AD.txt");
        assert_eq!(encode_url("dir/"), "dir/");
        assert_eq!(encode_url(""), "");
    }

    /// A request without a delimiter must stay flat: the crate otherwise inserts
    /// `delimiter=/` of its own.
    #[test]
    fn requests_a_flat_listing_without_a_delimiter() {
        assert!(recursive_listing(None));
        assert!(!recursive_listing(Some("/")));
    }

    /// Both request shapes: with `encoding-type=url` the values are escaped and the
    /// encoding is reported; without it they stay literal and no encoding is
    /// reported.
    #[test]
    fn encodes_only_when_the_backend_reports_it() {
        assert!(is_url_encoded(Some("url")));
        assert!(!is_url_encoded(None));
        assert!(!is_url_encoded(Some("base64")));

        assert_eq!(convert_value("pct%20name.txt", true), "pct%2520name.txt");
        assert_eq!(convert_value("a b/c", true), "a+b/c");
        assert_eq!(convert_value("pct%20name.txt", false), "pct%20name.txt");
        assert_eq!(convert_value("a b/c", false), "a b/c");
    }

    /// An entry with a key that needs escaping carries the wire form again.
    #[test]
    fn entry_reencodes_the_key() {
        let object = convert_entry(&minio_entry(), true).expect("convert the minio entry");
        assert_eq!(object.key.as_deref(), Some("a+b/c%2B%C3%BCn%C3%AFcode.txt"));
    }

    /// Positive control: both extension elements are carried into the DTO in the
    /// form the client parses.
    #[test]
    fn entry_carries_user_tags_and_user_metadata() {
        let mut entry = minio_entry();
        entry.user_tags = Some(HashMap::from([
            ("key2".to_owned(), "value2".to_owned()),
            ("key1".to_owned(), "value1".to_owned()),
        ]));
        entry.user_metadata = Some(HashMap::from([
            ("content-type".to_owned(), "application/x-www-form-urlencoded".to_owned()),
            ("X-Amz-Meta-Test".to_owned(), "test-value".to_owned()),
        ]));

        let object = convert_entry(&entry, false).expect("convert the minio entry");
        assert_eq!(object.user_tags.as_deref(), Some("key1=value1&key2=value2"));
        assert_eq!(
            object.user_metadata,
            Some(ObjectUserMetadata(vec![
                ("X-Amz-Meta-Test".to_owned(), "test-value".to_owned()),
                ("content-type".to_owned(), "application/x-www-form-urlencoded".to_owned()),
            ]))
        );

        // Everything else stays identical to the entry without the extension.
        let plain = convert_entry(&minio_entry(), false).expect("convert the minio entry");
        let without_extension = Object {
            user_tags: None,
            user_metadata: None,
            ..object
        };
        assert_eq!(without_extension, plain);
    }

    /// Negative control: an entry without the extension elements invents nothing.
    #[test]
    fn entry_without_the_extension_stays_empty() {
        let object = convert_entry(&minio_entry(), false).expect("convert the minio entry");
        assert_eq!(object.user_tags, None);
        assert_eq!(object.user_metadata, None);
    }

    /// An empty extension element is absent, like every other empty DTO field.
    #[test]
    fn empty_extension_element_is_absent() {
        let mut entry = minio_entry();
        entry.user_tags = Some(HashMap::new());
        entry.user_metadata = Some(HashMap::new());
        let object = convert_entry(&entry, false).expect("convert the minio entry");
        assert_eq!(object.user_tags, None);
        assert_eq!(object.user_metadata, None);
    }

    /// A common prefix is a listing entry too, so it must not become an object.
    #[test]
    fn common_prefixes_are_not_objects() {
        let mut entry = minio_entry();
        entry.is_prefix = true;
        entry.name = "dir/".to_owned();
        entry.size = None;
        entry.etag = None;
        entry.last_modified = None;
        entry.owner_id = None;
        entry.owner_name = None;
        entry.storage_class = None;
        let object = convert_entry(&entry, false).expect("convert the minio entry");
        assert_eq!(object.key.as_deref(), Some("dir/"));
    }
}
