// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

//! Regenerates the committed seed corpus for the `xml_bodies` fuzz target.
//!
//! Every seed is validated before being written:
//! 1. it must parse successfully into its DTO,
//! 2. serialization must succeed,
//! 3. re-parsing the serialization must yield an equal value (roundtrip
//!    idempotence — the same property the fuzz target enforces).
//!
//! Output layout mirrors the wire protocol of `fuzz_targets/xml_bodies.rs`:
//! file content = `[selector_byte][xml_body]`, where
//! `selector_byte == index % N` selects the request type.
//!
//! Usage (from the repository root):
//! ```text
//! cargo run --manifest-path fuzz/Cargo.toml --release --bin gen_corpus -- \
//!     fuzz/seeds/xml_bodies
//! ```
//!
//! NOTE: the registry below must stay in sync with `BODIES` in
//! `fuzz_targets/xml_bodies.rs` (same order, same length).

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

type ValidateFn = fn(&'static str, &'static str) -> Result<(), String>;

fn seed<T>(name: &'static str, xml: &'static str) -> Result<(), String>
where
    T: for<'x> Deserialize<'x> + Serialize + PartialEq + std::fmt::Debug,
{
    let value = {
        let mut d = s3s::xml::Deserializer::new(xml.as_bytes());
        let ans = T::deserialize(&mut d).map_err(|e| format!("parse failed: {e}"))?;
        d.expect_eof().map_err(|e| format!("trailing data: {e}"))?;
        ans
    };

    let mut buf = Vec::new();
    {
        let mut ser = s3s::xml::Serializer::new(&mut buf);
        value.serialize(&mut ser).map_err(|e| format!("serialize failed: {e}"))?;
    }

    let reparsed = {
        let mut d = s3s::xml::Deserializer::new(&buf);
        let ans = T::deserialize(&mut d).map_err(|e| format!("re-parse failed ({:?}): {e}", String::from_utf8_lossy(&buf)))?;
        d.expect_eof().map_err(|e| format!("re-parse trailing data: {e}"))?;
        ans
    };

    if reparsed != value {
        return Err(format!(
            "roundtrip not idempotent: {value:?} != {reparsed:?} (serialized: {:?})",
            String::from_utf8_lossy(&buf),
        ));
    }

    let _ = name;
    Ok(())
}

/// Alphabetical registry (index = selector byte % len). 32 entries.
static SEEDS: &[(&str, ValidateFn)] = &[
    ("AbacStatus", seed::<AbacStatus>),
    ("AccessControlPolicy", seed::<AccessControlPolicy>),
    ("AccelerateConfiguration", seed::<AccelerateConfiguration>),
    ("AnalyticsConfiguration", seed::<AnalyticsConfiguration>),
    ("AnnotationTableConfigurationUpdates", seed::<AnnotationTableConfigurationUpdates>),
    ("BucketLifecycleConfiguration", seed::<BucketLifecycleConfiguration>),
    ("BucketLoggingStatus", seed::<BucketLoggingStatus>),
    ("CORSConfiguration", seed::<CORSConfiguration>),
    ("CompletedMultipartUpload", seed::<CompletedMultipartUpload>),
    ("CreateBucketConfiguration", seed::<CreateBucketConfiguration>),
    ("Delete", seed::<Delete>),
    ("IntelligentTieringConfiguration", seed::<IntelligentTieringConfiguration>),
    ("InventoryConfiguration", seed::<InventoryConfiguration>),
    ("InventoryTableConfigurationUpdates", seed::<InventoryTableConfigurationUpdates>),
    ("JournalTableConfigurationUpdates", seed::<JournalTableConfigurationUpdates>),
    ("MetadataConfiguration", seed::<MetadataConfiguration>),
    ("MetadataTableConfiguration", seed::<MetadataTableConfiguration>),
    ("MetricsConfiguration", seed::<MetricsConfiguration>),
    ("NotificationConfiguration", seed::<NotificationConfiguration>),
    ("ObjectLockConfiguration", seed::<ObjectLockConfiguration>),
    ("ObjectLockLegalHold", seed::<ObjectLockLegalHold>),
    ("ObjectLockRetention", seed::<ObjectLockRetention>),
    ("OwnershipControls", seed::<OwnershipControls>),
    ("PublicAccessBlockConfiguration", seed::<PublicAccessBlockConfiguration>),
    ("ReplicationConfiguration", seed::<ReplicationConfiguration>),
    ("RequestPaymentConfiguration", seed::<RequestPaymentConfiguration>),
    ("RestoreRequest", seed::<RestoreRequest>),
    ("SelectObjectContentRequest", seed::<SelectObjectContentRequest>),
    ("ServerSideEncryptionConfiguration", seed::<ServerSideEncryptionConfiguration>),
    ("Tagging", seed::<Tagging>),
    ("VersioningConfiguration", seed::<VersioningConfiguration>),
    ("WebsiteConfiguration", seed::<WebsiteConfiguration>),
];

const XML_BODIES: &[&str] = &[
    "<AbacStatus><Status>Enabled</Status></AbacStatus>",
    "<AccessControlPolicy><Owner><ID>o</ID></Owner><AccessControlList><Grant><Grantee xmlns:xsi=\"http://www.w3.org/2001/XMLSchema-instance\" xsi:type=\"CanonicalUser\"><ID>g</ID></Grantee><Permission>FULL_CONTROL</Permission></Grant></AccessControlList></AccessControlPolicy>",
    "<AccelerateConfiguration><Status>Enabled</Status></AccelerateConfiguration>",
    "<AnalyticsConfiguration><Id>a</Id><StorageClassAnalysis/></AnalyticsConfiguration>",
    "<AnnotationTableConfiguration><ConfigurationState>ENABLED</ConfigurationState></AnnotationTableConfiguration>",
    "<LifecycleConfiguration><Rule><ID>r</ID><Status>Enabled</Status><Filter/><Expiration><Days>1</Days></Expiration></Rule></LifecycleConfiguration>",
    "<BucketLoggingStatus><LoggingEnabled><TargetBucket>b</TargetBucket><TargetPrefix>p</TargetPrefix></LoggingEnabled></BucketLoggingStatus>",
    "<CORSConfiguration><CORSRule><AllowedMethod>GET</AllowedMethod><AllowedOrigin>*</AllowedOrigin></CORSRule></CORSConfiguration>",
    "<CompleteMultipartUpload><Part><ETag>&quot;e&quot;</ETag><PartNumber>1</PartNumber></Part></CompleteMultipartUpload>",
    "<CreateBucketConfiguration><LocationConstraint>us-west-1</LocationConstraint></CreateBucketConfiguration>",
    "<Delete xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\"><Object><Key>k</Key></Object><Quiet>true</Quiet></Delete>",
    "<IntelligentTieringConfiguration><Id>i</Id><Status>Enabled</Status><Tiering><Days>90</Days><AccessTier>ARCHIVE_ACCESS</AccessTier></Tiering></IntelligentTieringConfiguration>",
    "<InventoryConfiguration><Id>i</Id><IsEnabled>true</IsEnabled><Destination><S3BucketDestination><Format>CSV</Format><Bucket>arn:aws:s3:::b</Bucket></S3BucketDestination></Destination><IncludedObjectVersions>All</IncludedObjectVersions><Schedule><Frequency>Daily</Frequency></Schedule></InventoryConfiguration>",
    "<InventoryTableConfiguration><ConfigurationState>ENABLED</ConfigurationState></InventoryTableConfiguration>",
    "<JournalTableConfiguration><RecordExpiration><Expiration>ENABLED</Expiration></RecordExpiration></JournalTableConfiguration>",
    "<MetadataConfiguration><JournalTableConfiguration><RecordExpiration><Expiration>ENABLED</Expiration></RecordExpiration></JournalTableConfiguration></MetadataConfiguration>",
    "<MetadataTableConfiguration><S3TablesDestination><TableBucketArn>arn:aws:s3tables:us-east-1:1:tablebucket/tb</TableBucketArn><TableName>t</TableName></S3TablesDestination></MetadataTableConfiguration>",
    "<MetricsConfiguration><Id>m</Id></MetricsConfiguration>",
    "<NotificationConfiguration/>",
    "<ObjectLockConfiguration><ObjectLockEnabled>Enabled</ObjectLockEnabled></ObjectLockConfiguration>",
    "<LegalHold><Status>ON</Status></LegalHold>",
    "<Retention><Mode>GOVERNANCE</Mode><RetainUntilDate>2030-01-01T00:00:00Z</RetainUntilDate></Retention>",
    "<OwnershipControls><Rule><ObjectOwnership>BucketOwnerEnforced</ObjectOwnership></Rule></OwnershipControls>",
    "<PublicAccessBlockConfiguration><BlockPublicAcls>true</BlockPublicAcls></PublicAccessBlockConfiguration>",
    "<ReplicationConfiguration><Role>arn:aws:iam::1:role/r</Role><Rule><Status>Enabled</Status><Destination><Bucket>arn:aws:s3:::d</Bucket></Destination></Rule></ReplicationConfiguration>",
    "<RequestPaymentConfiguration><Payer>Requester</Payer></RequestPaymentConfiguration>",
    "<RestoreRequest><Days>1</Days></RestoreRequest>",
    "<SelectObjectContentRequest><Expression>SELECT * FROM s3object</Expression><ExpressionType>SQL</ExpressionType><InputSerialization><CSV/></InputSerialization><OutputSerialization><CSV/></OutputSerialization></SelectObjectContentRequest>",
    "<ServerSideEncryptionConfiguration><Rule><ApplyServerSideEncryptionByDefault><SSEAlgorithm>AES256</SSEAlgorithm></ApplyServerSideEncryptionByDefault></Rule></ServerSideEncryptionConfiguration>",
    "<Tagging><TagSet><Tag><Key>k</Key><Value>v</Value></Tag></TagSet></Tagging>",
    "<VersioningConfiguration><Status>Enabled</Status></VersioningConfiguration>",
    "<WebsiteConfiguration><IndexDocument><Suffix>index.html</Suffix></IndexDocument></WebsiteConfiguration>",
];

fn main() {
    assert_eq!(SEEDS.len(), XML_BODIES.len(), "registry desync");
    let n = SEEDS.len();

    let mut args = std::env::args().skip(1);
    let Some(out_dir) = args.next() else {
        eprintln!("usage: gen_corpus <output-dir>");
        std::process::exit(2);
    };
    std::fs::create_dir_all(&out_dir).expect("create output dir");

    println!("registry size: {n}");

    for ((name, validate), xml) in SEEDS.iter().zip(XML_BODIES.iter()) {
        if let Err(err) = validate(name, xml) {
            eprintln!("FAILED {name}: {err}");
            std::process::exit(1);
        }
    }
    println!("all {n} seeds validated");

    for (idx, ((name, _), xml)) in SEEDS.iter().zip(XML_BODIES.iter()).enumerate() {
        let mut content = Vec::with_capacity(xml.len() + 1);
        content.push((idx % n) as u8);
        content.extend_from_slice(xml.as_bytes());

        let path = format!("{out_dir}/{idx:02}_{name}.xml");
        std::fs::write(&path, &content).expect("write seed file");
    }
    println!("wrote {n} seed files to {out_dir}");
}
