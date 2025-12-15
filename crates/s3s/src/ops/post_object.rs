//! POST Object operation
//!
//! See <https://docs.aws.amazon.com/AmazonS3/latest/API/RESTObjectPOST.html>

use crate::dto::*;
use crate::error::*;
use crate::header::*;
use crate::http;
use crate::ops::{build_s3_request, CallContext, Operation};
use crate::protocol::S3Request;

use hyper::StatusCode;

/// POST Object operation
pub struct PostObject;

impl PostObject {
    /// Deserialize HTTP request with multipart form data into PostObjectInput
    pub fn deserialize_http_multipart(req: &mut http::Request, m: &http::Multipart) -> S3Result<PostObjectInput> {
        let bucket = http::unwrap_bucket(req);
        let key = http::parse_field_value(m, "key")?.ok_or_else(|| invalid_request!("missing key"))?;

        let vec_stream = req.s3ext.vec_stream.take().expect("missing vec stream");

        let content_length = i64::try_from(vec_stream.exact_remaining_length())
            .map_err(|e| s3_error!(e, InvalidArgument, "content-length overflow"))?;
        let content_length = (content_length != 0).then_some(content_length);

        let body: Option<StreamingBlob> = Some(StreamingBlob::new(vec_stream));

        let acl: Option<ObjectCannedACL> = http::parse_field_value(m, "x-amz-acl")?;

        let bucket_key_enabled: Option<BucketKeyEnabled> =
            http::parse_field_value(m, "x-amz-server-side-encryption-bucket-key-enabled")?;

        let cache_control: Option<CacheControl> = http::parse_field_value(m, "cache-control")?;

        let checksum_algorithm: Option<ChecksumAlgorithm> = http::parse_field_value(m, "x-amz-sdk-checksum-algorithm")?;

        let checksum_crc32: Option<ChecksumCRC32> = http::parse_field_value(m, "x-amz-checksum-crc32")?;

        let checksum_crc32c: Option<ChecksumCRC32C> = http::parse_field_value(m, "x-amz-checksum-crc32c")?;

        let checksum_crc64nvme: Option<ChecksumCRC64NVME> = http::parse_field_value(m, "x-amz-checksum-crc64nvme")?;

        let checksum_sha1: Option<ChecksumSHA1> = http::parse_field_value(m, "x-amz-checksum-sha1")?;

        let checksum_sha256: Option<ChecksumSHA256> = http::parse_field_value(m, "x-amz-checksum-sha256")?;

        let content_disposition: Option<ContentDisposition> = http::parse_field_value(m, "content-disposition")?;

        let content_encoding: Option<ContentEncoding> = http::parse_field_value(m, "content-encoding")?;

        let content_language: Option<ContentLanguage> = http::parse_field_value(m, "content-language")?;

        let content_type: Option<ContentType> = http::parse_field_value(m, "content-type")?;

        let expected_bucket_owner: Option<AccountId> = http::parse_field_value(m, "x-amz-expected-bucket-owner")?;

        let expires: Option<Expires> = http::parse_field_value_timestamp(m, "expires", TimestampFormat::HttpDate)?;

        let grant_full_control: Option<GrantFullControl> = http::parse_field_value(m, "x-amz-grant-full-control")?;

        let grant_read: Option<GrantRead> = http::parse_field_value(m, "x-amz-grant-read")?;

        let grant_read_acp: Option<GrantReadACP> = http::parse_field_value(m, "x-amz-grant-read-acp")?;

        let grant_write_acp: Option<GrantWriteACP> = http::parse_field_value(m, "x-amz-grant-write-acp")?;

        let metadata: Option<Metadata> = {
            let mut metadata = Metadata::default();
            for (name, value) in m.fields() {
                if let Some(key) = name.strip_prefix("x-amz-meta-") {
                    if key.is_empty() {
                        continue;
                    }
                    metadata.insert(key.to_owned(), value.clone());
                }
            }
            if metadata.is_empty() { None } else { Some(metadata) }
        };

        let object_lock_legal_hold_status: Option<ObjectLockLegalHoldStatus> =
            http::parse_field_value(m, "x-amz-object-lock-legal-hold")?;

        let object_lock_mode: Option<ObjectLockMode> = http::parse_field_value(m, "x-amz-object-lock-mode")?;

        let object_lock_retain_until_date: Option<ObjectLockRetainUntilDate> =
            http::parse_field_value_timestamp(m, "x-amz-object-lock-retain-until-date", TimestampFormat::DateTime)?;

        let request_payer: Option<RequestPayer> = http::parse_field_value(m, "x-amz-request-payer")?;

        let sse_customer_algorithm: Option<SSECustomerAlgorithm> =
            http::parse_field_value(m, "x-amz-server-side-encryption-customer-algorithm")?;

        let sse_customer_key: Option<SSECustomerKey> = http::parse_field_value(m, "x-amz-server-side-encryption-customer-key")?;

        let sse_customer_key_md5: Option<SSECustomerKeyMD5> =
            http::parse_field_value(m, "x-amz-server-side-encryption-customer-key-md5")?;

        let ssekms_encryption_context: Option<SSEKMSEncryptionContext> =
            http::parse_field_value(m, "x-amz-server-side-encryption-context")?;

        let ssekms_key_id: Option<SSEKMSKeyId> = http::parse_field_value(m, "x-amz-server-side-encryption-aws-kms-key-id")?;

        let server_side_encryption: Option<ServerSideEncryption> = http::parse_field_value(m, "x-amz-server-side-encryption")?;

        let storage_class: Option<StorageClass> = http::parse_field_value(m, "x-amz-storage-class")?;

        let tagging: Option<TaggingHeader> = http::parse_field_value(m, "x-amz-tagging")?;

        let website_redirect_location: Option<WebsiteRedirectLocation> =
            http::parse_field_value(m, "x-amz-website-redirect-location")?;

        // Note: success_action_redirect and success_action_status are POST-specific fields
        let success_action_redirect: Option<String> = http::parse_field_value(m, "success_action_redirect")?;
        let success_action_status: Option<String> = http::parse_field_value(m, "success_action_status")?;

        Ok(PostObjectInput {
            acl,
            body,
            bucket,
            bucket_key_enabled,
            cache_control,
            checksum_algorithm,
            checksum_crc32,
            checksum_crc32c,
            checksum_crc64nvme,
            checksum_sha1,
            checksum_sha256,
            content_disposition,
            content_encoding,
            content_language,
            content_length,
            content_type,
            expected_bucket_owner,
            expires,
            grant_full_control,
            grant_read,
            grant_read_acp,
            grant_write_acp,
            key,
            metadata,
            object_lock_legal_hold_status,
            object_lock_mode,
            object_lock_retain_until_date,
            request_payer,
            sse_customer_algorithm,
            sse_customer_key,
            sse_customer_key_md5,
            ssekms_encryption_context,
            ssekms_key_id,
            server_side_encryption,
            storage_class,
            success_action_redirect,
            success_action_status,
            tagging,
            website_redirect_location,
        })
    }

    /// Serialize HTTP response from PostObjectOutput
    pub fn serialize_http(x: PostObjectOutput) -> S3Result<http::Response> {
        let mut res = http::Response::with_status(StatusCode::NO_CONTENT);

        http::add_opt_header(&mut res, X_AMZ_SERVER_SIDE_ENCRYPTION_BUCKET_KEY_ENABLED, x.bucket_key_enabled)?;
        http::add_opt_header(&mut res, X_AMZ_CHECKSUM_CRC32, x.checksum_crc32)?;
        http::add_opt_header(&mut res, X_AMZ_CHECKSUM_CRC32C, x.checksum_crc32c)?;
        http::add_opt_header(&mut res, X_AMZ_CHECKSUM_CRC64NVME, x.checksum_crc64nvme)?;
        http::add_opt_header(&mut res, X_AMZ_CHECKSUM_SHA1, x.checksum_sha1)?;
        http::add_opt_header(&mut res, X_AMZ_CHECKSUM_SHA256, x.checksum_sha256)?;
        http::add_opt_header(&mut res, ETAG, x.e_tag)?;
        http::add_opt_header(&mut res, X_AMZ_EXPIRATION, x.expiration)?;
        http::add_opt_header(&mut res, LOCATION, x.location)?;
        http::add_opt_header(&mut res, X_AMZ_REQUEST_CHARGED, x.request_charged)?;
        http::add_opt_header(&mut res, X_AMZ_SERVER_SIDE_ENCRYPTION_CUSTOMER_ALGORITHM, x.sse_customer_algorithm)?;
        http::add_opt_header(&mut res, X_AMZ_SERVER_SIDE_ENCRYPTION_CUSTOMER_KEY_MD5, x.sse_customer_key_md5)?;
        http::add_opt_header(&mut res, X_AMZ_SERVER_SIDE_ENCRYPTION_CONTEXT, x.ssekms_encryption_context)?;
        http::add_opt_header(&mut res, X_AMZ_SERVER_SIDE_ENCRYPTION_AWS_KMS_KEY_ID, x.ssekms_key_id)?;
        http::add_opt_header(&mut res, X_AMZ_SERVER_SIDE_ENCRYPTION, x.server_side_encryption)?;
        http::add_opt_header(&mut res, X_AMZ_VERSION_ID, x.version_id)?;

        Ok(res)
    }
}

#[async_trait::async_trait]
impl Operation for PostObject {
    fn name(&self) -> &'static str {
        "PostObject"
    }

    async fn call(&self, ccx: &CallContext<'_>, req: &mut http::Request) -> S3Result<http::Response> {
        let multipart = req.s3ext.multipart.take().expect("multipart data is missing");
        let input = Self::deserialize_http_multipart(req, &multipart)?;
        let s3_req: S3Request<PostObjectInput> = build_s3_request(input, req);
        let s3_resp = ccx.s3.post_object(s3_req).await?;
        let resp = Self::serialize_http(s3_resp.output)?;
        Ok(resp)
    }
}
