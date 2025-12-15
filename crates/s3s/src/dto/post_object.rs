//! POST Object DTOs (not generated, manually defined)
//!
//! See <https://docs.aws.amazon.com/AmazonS3/latest/API/RESTObjectPOST.html>

use super::*;

/// POST Object input
///
/// See <https://docs.aws.amazon.com/AmazonS3/latest/API/RESTObjectPOST.html>
#[derive(Debug)]
pub struct PostObjectInput {
    /// The canned ACL to apply to the object
    pub acl: Option<ObjectCannedACL>,

    /// Object data
    pub body: Option<StreamingBlob>,

    /// The bucket name
    pub bucket: BucketName,

    /// Specifies whether Amazon S3 should use an S3 Bucket Key for object encryption
    pub bucket_key_enabled: Option<BucketKeyEnabled>,

    /// Caching behavior along the request/reply chain
    pub cache_control: Option<CacheControl>,

    /// Indicates the algorithm used to create the checksum for the object
    pub checksum_algorithm: Option<ChecksumAlgorithm>,

    /// This header can be used as a data integrity check
    pub checksum_crc32: Option<ChecksumCRC32>,

    /// This header can be used as a data integrity check
    pub checksum_crc32c: Option<ChecksumCRC32C>,

    /// This header can be used as a data integrity check
    pub checksum_crc64nvme: Option<ChecksumCRC64NVME>,

    /// This header can be used as a data integrity check
    pub checksum_sha1: Option<ChecksumSHA1>,

    /// This header can be used as a data integrity check
    pub checksum_sha256: Option<ChecksumSHA256>,

    /// Specifies presentational information for the object
    pub content_disposition: Option<ContentDisposition>,

    /// Specifies what content encodings have been applied to the object
    pub content_encoding: Option<ContentEncoding>,

    /// The language the content is in
    pub content_language: Option<ContentLanguage>,

    /// Size of the body in bytes
    pub content_length: Option<i64>,

    /// A standard MIME type describing the format of the contents
    pub content_type: Option<ContentType>,

    /// The account ID of the expected bucket owner
    pub expected_bucket_owner: Option<AccountId>,

    /// The date and time at which the object is no longer cacheable
    pub expires: Option<Expires>,

    /// Gives the grantee READ, READ_ACP, and WRITE_ACP permissions on the object
    pub grant_full_control: Option<GrantFullControl>,

    /// Allows grantee to read the object data and its metadata
    pub grant_read: Option<GrantRead>,

    /// Allows grantee to read the object ACL
    pub grant_read_acp: Option<GrantReadACP>,

    /// Allows grantee to write the ACL for the applicable object
    pub grant_write_acp: Option<GrantWriteACP>,

    /// Object key for which the POST action was initiated
    pub key: ObjectKey,

    /// A map of metadata to store with the object in S3
    pub metadata: Option<Metadata>,

    /// Specifies whether a legal hold will be applied to this object
    pub object_lock_legal_hold_status: Option<ObjectLockLegalHoldStatus>,

    /// The Object Lock mode that you want to apply to this object
    pub object_lock_mode: Option<ObjectLockMode>,

    /// The date and time when you want this object's Object Lock to expire
    pub object_lock_retain_until_date: Option<ObjectLockRetainUntilDate>,

    /// Confirms that the requester knows that they will be charged for the request
    pub request_payer: Option<RequestPayer>,

    /// Specifies the algorithm to use to when encrypting the object (for example, AES256)
    pub sse_customer_algorithm: Option<SSECustomerAlgorithm>,

    /// Specifies the customer-provided encryption key for Amazon S3 to use in encrypting data
    pub sse_customer_key: Option<SSECustomerKey>,

    /// Specifies the 128-bit MD5 digest of the encryption key
    pub sse_customer_key_md5: Option<SSECustomerKeyMD5>,

    /// Specifies the AWS KMS Encryption Context to use for object encryption
    pub ssekms_encryption_context: Option<SSEKMSEncryptionContext>,

    /// Specifies the ID of the symmetric customer managed key to use for object encryption
    pub ssekms_key_id: Option<SSEKMSKeyId>,

    /// The server-side encryption algorithm used when storing this object in Amazon S3
    pub server_side_encryption: Option<ServerSideEncryption>,

    /// By default, Amazon S3 uses the STANDARD Storage Class to store newly created objects
    pub storage_class: Option<StorageClass>,

    /// The tag-set for the object
    pub tagging: Option<TaggingHeader>,

    /// If the bucket is configured as a website, redirects requests for this object to another object in the same bucket or to an external URL
    pub website_redirect_location: Option<WebsiteRedirectLocation>,

    /// POST-specific: The URL to which the client is redirected upon successful upload
    pub success_action_redirect: Option<String>,

    /// POST-specific: The status code returned to the client upon successful upload
    pub success_action_status: Option<String>,
}

/// POST Object output
///
/// See <https://docs.aws.amazon.com/AmazonS3/latest/API/RESTObjectPOST.html>
#[derive(Debug, Clone, Default)]
pub struct PostObjectOutput {
    /// Indicates whether the uploaded object uses an S3 Bucket Key for server-side encryption with KMS
    pub bucket_key_enabled: Option<BucketKeyEnabled>,

    /// The base64-encoded, 32-bit CRC32 checksum of the object
    pub checksum_crc32: Option<ChecksumCRC32>,

    /// The base64-encoded, 32-bit CRC32C checksum of the object
    pub checksum_crc32c: Option<ChecksumCRC32C>,

    /// The base64-encoded, 64-bit CRC64NVME checksum of the object
    pub checksum_crc64nvme: Option<ChecksumCRC64NVME>,

    /// The base64-encoded, 160-bit SHA-1 digest of the object
    pub checksum_sha1: Option<ChecksumSHA1>,

    /// The base64-encoded, 256-bit SHA-256 digest of the object
    pub checksum_sha256: Option<ChecksumSHA256>,

    /// Entity tag for the uploaded object
    pub e_tag: Option<ETag>,

    /// If the expiration is configured for the object, the response includes this header
    pub expiration: Option<Expiration>,

    /// The URI of the uploaded object
    pub location: Option<String>,

    /// If present, indicates that the requester was successfully charged for the request
    pub request_charged: Option<RequestCharged>,

    /// If server-side encryption with a customer-provided encryption key was requested
    pub sse_customer_algorithm: Option<SSECustomerAlgorithm>,

    /// If server-side encryption with a customer-provided encryption key was requested
    pub sse_customer_key_md5: Option<SSECustomerKeyMD5>,

    /// If present, specifies the AWS KMS Encryption Context to use for object encryption
    pub ssekms_encryption_context: Option<SSEKMSEncryptionContext>,

    /// If present, specifies the ID of the AWS Key Management Service key
    pub ssekms_key_id: Option<SSEKMSKeyId>,

    /// The server-side encryption algorithm used when storing this object in Amazon S3
    pub server_side_encryption: Option<ServerSideEncryption>,

    /// Version of the object
    pub version_id: Option<ObjectVersionId>,
}
