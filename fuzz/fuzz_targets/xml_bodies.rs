// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

#![no_main]

//! Fuzz target for all S3 XML request bodies.
//!
//! Input protocol: `data[0] % N` selects the request type (N = number of
//! entries in [`BODIES`]), `data[1..]` is the XML body.
//!
//! Oracles:
//! - A (always): parsing must not panic — every failure is a returned error.
//! - B (roundtrip, per-type opt-out via [`ROUNDTRIP_DISABLED`]): a body that
//!   parses successfully must satisfy `de(ser(x)) == x`. This is an
//!   *idempotence* property, not byte-equality of the serialization: XML has
//!   many equivalent encodings and unknown-element skipping is lossy by design.
//!
//! Type registry maintenance: this table must mirror every top-level DTO fed
//! through `http::take_xml_body` / `http::take_opt_xml_body` / the
//! `match http::take_xml_body(req)` form in `crates/s3s/src/ops/generated.rs`
//! (and `generated_minio.rs`, which resolves to the same set: literal-form
//! ops fall back to the same deserializer). After codegen updates re-derive
//! with:
//!   grep -oE 'let [a-z_]+: [A-Za-z]+(<[^=]*>)? ?= ?http::take_(opt_)?xml_body'
//!   plus 'match http::take_xml_body' sites and 'Option<[A-Za-z]+>' variants.

use libfuzzer_sys::fuzz_target;
use s3s::xml::{Deserialize, Serialize};

use s3s::dto::{
    AbacStatus, AccelerateConfiguration, AccessControlPolicy, AnalyticsConfiguration, AnnotationTableConfigurationUpdates,
    BucketLifecycleConfiguration, BucketLoggingStatus, CORSConfiguration, CompletedMultipartUpload, CreateBucketConfiguration,
    Delete, IntelligentTieringConfiguration, InventoryConfiguration, InventoryTableConfigurationUpdates,
    JournalTableConfigurationUpdates, MetadataConfiguration, MetadataTableConfiguration, MetricsConfiguration,
    NotificationConfiguration, ObjectLockConfiguration, ObjectLockLegalHold, ObjectLockRetention, OwnershipControls,
    PublicAccessBlockConfiguration, ReplicationConfiguration, RequestPaymentConfiguration, RestoreRequest,
    SelectObjectContentRequest, ServerSideEncryptionConfiguration, Tagging, VersioningConfiguration, WebsiteConfiguration,
};

/// Types whose roundtrip oracle (B) is disabled. Keep short; each entry needs
/// an issue link or a written justification.
const ROUNDTRIP_DISABLED: &[&str] = &[];

type BodyFn = fn(&'static str, &[u8]);

fn parse<T>(bytes: &[u8]) -> Result<T, s3s::xml::DeError>
where
    T: for<'x> Deserialize<'x>,
{
    let mut d = s3s::xml::Deserializer::new(bytes);
    let ans = T::deserialize(&mut d)?;
    d.expect_eof()?;
    Ok(ans)
}

fn roundtrip<T>(name: &'static str, bytes: &[u8])
where
    T: for<'x> Deserialize<'x> + Serialize + PartialEq + std::fmt::Debug,
{
    let Ok(value) = parse::<T>(bytes) else { return }; // oracle A

    if ROUNDTRIP_DISABLED.contains(&name) {
        return;
    }

    let mut buf = Vec::new();
    {
        let mut ser = s3s::xml::Serializer::new(&mut buf);
        value.serialize(&mut ser).expect("serialization must not fail");
    }
    let reparsed = parse::<T>(&buf)
        .unwrap_or_else(|e| panic!("{name}: roundtrip re-parse failed: {e}; serialized: {:?}", String::from_utf8_lossy(&buf)));
    assert_eq!(reparsed, value, "{name}: ser∘de is not idempotent");
}

macro_rules! bodies {
    ($($name:ident),* $(,)?) => {
        &[$((stringify!($name), roundtrip::<$name> as BodyFn)),*]
    };
}

/// Alphabetical registry (index = selector byte % len). 32 entries.
static BODIES: &[(&str, BodyFn)] = bodies!(
    AbacStatus,
    AccessControlPolicy,
    AccelerateConfiguration,
    AnalyticsConfiguration,
    AnnotationTableConfigurationUpdates,
    BucketLifecycleConfiguration,
    BucketLoggingStatus,
    CORSConfiguration,
    CompletedMultipartUpload,
    CreateBucketConfiguration,
    Delete,
    IntelligentTieringConfiguration,
    InventoryConfiguration,
    InventoryTableConfigurationUpdates,
    JournalTableConfigurationUpdates,
    MetadataConfiguration,
    MetadataTableConfiguration,
    MetricsConfiguration,
    NotificationConfiguration,
    ObjectLockConfiguration,
    ObjectLockLegalHold,
    ObjectLockRetention,
    OwnershipControls,
    PublicAccessBlockConfiguration,
    ReplicationConfiguration,
    RequestPaymentConfiguration,
    RestoreRequest,
    SelectObjectContentRequest,
    ServerSideEncryptionConfiguration,
    Tagging,
    VersioningConfiguration,
    WebsiteConfiguration,
);

fuzz_target!(|data: &[u8]| {
    let Some((&selector, body)) = data.split_first() else { return };
    let (name, f) = BODIES[usize::from(selector) % BODIES.len()];
    f(name, body);
});
