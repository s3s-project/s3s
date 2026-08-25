// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

use crate::auth::S3Auth;
use crate::auth::SecretKey;
use crate::auth::signature::Signature;
use crate::config::{S3Config, S3ConfigProvider};
use crate::error::*;
use crate::http::{self, OrderedQs};
use crate::http::{AwsChunkedStream, Body, Multipart, MultipartLimits};
use crate::post_policy::PostPolicy;
use crate::protocol::TrailingHeaders;
use crate::sig_v2;
use crate::sig_v2::{AuthorizationV2, PostSignatureV2, PresignedUrlV2};
use crate::sig_v4;
use crate::sig_v4::AmzContentSha256;
use crate::sig_v4::AmzDate;
use crate::sig_v4::UploadStream;
use crate::sig_v4::{AuthorizationV4, CredentialV4, PostSignatureV4, PresignedUrlV4};
use crate::stream::ByteStream as _;
use crate::utils::crypto::Sha256Sum;
use crate::utils::crypto::hex_sha256;
use crate::utils::is_base64_encoded;

use std::mem;
use std::ops::Not;
use std::sync::Arc;

use hyper::HeaderMap;
use hyper::Method;
use hyper::Uri;
use mime::Mime;
use smallvec::SmallVec;
use tracing::debug;

/// Maximum allowed size for STS request body (8KB should be enough for operations like `AssumeRole`)
const MAX_STS_BODY_SIZE: usize = 8192;

type SignedHeaderPairs<'a> = SmallVec<[(&'a str, &'a str); 16]>;

fn extract_amz_content_sha256(hs: &HeaderMap) -> S3Result<Option<AmzContentSha256>> {
    let Some(val) = http::get_unique_header_str(hs, crate::header::X_AMZ_CONTENT_SHA256.as_str()) else {
        return Ok(None);
    };
    match AmzContentSha256::parse(val) {
        Ok(x) => Ok(Some(x)),
        Err(e) => {
            // https://docs.aws.amazon.com/IAM/latest/UserGuide/reference_sigv-troubleshooting.html
            Err(s3_error!(e, SignatureDoesNotMatch, "invalid header: x-amz-content-sha256"))
        }
    }
}

fn extract_authorization_v4(hs: &HeaderMap) -> S3Result<Option<AuthorizationV4<'_>>> {
    let Some(val) = http::get_unique_header_str(hs, crate::header::AUTHORIZATION.as_str()) else {
        return Ok(None);
    };
    match AuthorizationV4::parse(val) {
        Ok(x) => Ok(Some(x)),
        Err(e) => Err(invalid_request!(e, "invalid header: authorization")),
    }
}

fn extract_amz_date(hs: &HeaderMap) -> S3Result<Option<AmzDate>> {
    let Some(val) = http::get_unique_header_str(hs, crate::header::X_AMZ_DATE.as_str()) else {
        return Ok(None);
    };
    match AmzDate::parse(val) {
        Ok(x) => Ok(Some(x)),
        Err(e) => Err(invalid_request!(e, "invalid header: x-amz-date")),
    }
}

fn collect_signed_headers<'a>(
    hs: &'a HeaderMap,
    names: &[&'a str],
    on_missing: impl Fn(&'a str) -> Option<&'a str>,
) -> S3Result<SignedHeaderPairs<'a>> {
    let mut headers = SignedHeaderPairs::new();

    for &name in names {
        let mut has_value = false;
        let mut has_invalid_value = false;
        for value in hs.get_all(name) {
            if let Some(value) = http::header_value_to_str(value) {
                headers.push((name, value));
                has_value = true;
            } else {
                has_invalid_value = true;
            }
        }
        if has_invalid_value {
            return Err(s3_error!(SignatureDoesNotMatch, "invalid signed header: {name}"));
        }
        if !has_value {
            let Some(value) = on_missing(name) else {
                return Err(s3_error!(SignatureDoesNotMatch, "missing signed header: {name}"));
            };
            headers.push((name, value));
        }
    }

    Ok(headers)
}

pub struct SignatureContext<'a> {
    pub auth: Option<&'a dyn S3Auth>,
    pub config: &'a Arc<dyn S3ConfigProvider>,

    pub req_version: ::http::Version,
    pub req_method: &'a Method,
    pub req_uri: &'a Uri,
    pub req_body: &'a mut Body,

    pub qs: Option<&'a OrderedQs>,
    pub hs: &'a HeaderMap,

    pub decoded_uri_path: &'a str,
    pub raw_uri_path: &'a str,
    pub vh_bucket: Option<&'a str>,

    pub content_length: Option<u64>,
    pub mime: Option<Mime>,
    pub decoded_content_length: Option<usize>,

    pub transformed_body: Option<Body>,
    pub multipart: Option<Multipart>,

    pub trailing_headers: Option<TrailingHeaders>,
}

#[derive(Debug)]
pub struct CredentialsExt {
    pub access_key: String,
    pub secret_key: SecretKey,
    pub region: Option<String>,
    pub service: Option<String>,
}

fn require_auth(auth: Option<&dyn S3Auth>) -> S3Result<&dyn S3Auth> {
    auth.ok_or_else(|| s3_error!(NotImplemented, "This service has no authentication provider"))
}

fn has_unencoded_reserved_path_char(path: &str) -> bool {
    // Percent-encoded paths should be handled by normal S3 canonicalization.
    path.bytes().any(|b| {
        !matches!(
            b,
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' | b'%'
        )
    })
}

struct SignatureVerificationContext<'a> {
    expected_signature: Signature,
    raw_uri_path: &'a str,
    secret_key: &'a SecretKey,
    amz_date: &'a AmzDate,
    region: &'a str,
    service: &'a str,
}

fn validate_sig_v4_clock_skew(amz_date: &AmzDate, now: time::OffsetDateTime, config: &S3Config) -> S3Result<()> {
    let request_time = amz_date.to_time().ok_or_else(|| invalid_request!("invalid amz date"))?;
    let duration = now - request_time;
    let max_skew_time = time::Duration::seconds(i64::from(config.presigned_url_max_skew_time_secs));

    if duration.abs() > max_skew_time {
        return Err(s3_error!(RequestTimeTooSkewed, "request time is too far from server time"));
    }

    Ok(())
}

fn validate_sig_v4_region(region: &str, config: &S3Config) -> S3Result<()> {
    if let Some(expected_region) = &config.expected_region
        && region != expected_region.as_str()
    {
        return Err(s3_error!(
            AuthorizationHeaderMalformed,
            "The authorization header is malformed; the region is wrong; expecting '{expected_region}'."
        ));
    }

    Ok(())
}

impl SignatureVerificationContext<'_> {
    fn verify_with_raw_path_fallback(
        &self,
        canonical_request: &str,
        raw_canonical_request: impl FnOnce() -> String,
    ) -> S3Result<Signature> {
        let string_to_sign = sig_v4::create_string_to_sign(canonical_request, self.amz_date, self.region, self.service);
        let signature = sig_v4::calculate_signature(&string_to_sign, self.secret_key, self.amz_date, self.region, self.service);

        if Signature::compare(&signature, &self.expected_signature) {
            return Ok(signature);
        }

        if !has_unencoded_reserved_path_char(self.raw_uri_path) {
            debug!(?signature, expected=?self.expected_signature, "signature mismatch");
            return Err(s3_error!(SignatureDoesNotMatch));
        }

        let canonical_request = raw_canonical_request();
        let string_to_sign = sig_v4::create_string_to_sign(&canonical_request, self.amz_date, self.region, self.service);
        let raw_signature =
            sig_v4::calculate_signature(&string_to_sign, self.secret_key, self.amz_date, self.region, self.service);

        if !Signature::compare(&raw_signature, &self.expected_signature) {
            debug!(?signature, ?raw_signature, expected=?self.expected_signature, "signature mismatch");
            return Err(s3_error!(SignatureDoesNotMatch));
        }

        Ok(raw_signature)
    }
}

impl<'a> SignatureContext<'a> {
    fn signed_host_fallback(&self, name: &'a str) -> Option<&'a str> {
        if name == "host"
            && matches!(self.req_version, ::http::Version::HTTP_2 | ::http::Version::HTTP_3)
            && let Some(authority) = self.req_uri.authority()
        {
            return Some(authority.as_str());
        }
        None
    }

    /// Rejects `SigV2` requests when the `enable_sig_v2` configuration is off.
    ///
    /// `SigV2` is disabled by default for security. When disabled, a recognized
    /// `SigV2` request is rejected with `AccessDenied` (fail-closed) rather than
    /// being treated as anonymous.
    fn ensure_v2_enabled(&self) -> S3Result<()> {
        let config = self.config.snapshot();
        if !config.enable_sig_v2 {
            return Err(s3_error!(AccessDenied, "Signature Version 2 is disabled by server configuration"));
        }
        Ok(())
    }

    pub async fn check(&mut self) -> S3Result<Option<CredentialsExt>> {
        if self.req_method == Method::POST
            && let Some(ref mime) = self.mime
            && mime.type_() == mime::MULTIPART
            && mime.subtype() == mime::FORM_DATA
        {
            return self.check_post_signature().await;
        }

        if let Some(result) = self.v2_check().await {
            debug!("checked signature v2");
            return Ok(Some(result?));
        }

        if let Some(result) = self.v4_check().await {
            debug!("checked signature v4");
            return Ok(Some(result?));
        }

        Ok(None)
    }

    #[tracing::instrument(skip(self))]
    async fn check_post_signature(&mut self) -> S3Result<Option<CredentialsExt>> {
        let multipart = {
            let Some(mime) = self.mime.as_ref() else {
                return Err(invalid_request!("internal error: mime was unexpectedly None"));
            };

            let boundary = mime
                .get_param(mime::BOUNDARY)
                .ok_or_else(|| invalid_request!("missing boundary"))?;

            let body = mem::take(self.req_body);
            let config = self.config.snapshot();
            let limits = MultipartLimits {
                max_field_size: config.form_max_field_size,
                max_fields_size: config.form_max_fields_size,
                max_parts: config.form_max_parts,
            };
            http::transform_multipart(body, boundary.as_str().as_bytes(), limits, self.content_length)
                .await
                .map_err(|e| s3_error!(e, MalformedPOSTRequest))?
        };

        debug!(?multipart);

        if multipart.find_field_value("x-amz-signature").is_some() {
            debug!("checking post signature v4");
            return Ok(Some(self.v4_check_post_signature(multipart).await?));
        }

        if multipart.find_field_value("signature").is_some() {
            debug!("checking post signature v2");
            return Ok(Some(self.v2_check_post_signature(multipart).await?));
        }

        self.multipart = Some(multipart);
        Ok(None)
    }

    #[tracing::instrument(skip(self))]
    pub async fn v4_check(&mut self) -> Option<S3Result<CredentialsExt>> {
        // query auth
        if let Some(qs) = self.qs
            && qs.has("X-Amz-Signature")
        {
            debug!("checking presigned url");
            return Some(self.v4_check_presigned_url().await);
        }

        // header auth
        if http::get_unique_header_str(self.hs, crate::header::AUTHORIZATION.as_str()).is_some() {
            debug!("checking header auth");
            return Some(self.v4_check_header_auth().await);
        }

        None
    }

    pub async fn v4_check_post_signature(&mut self, multipart: Multipart) -> S3Result<CredentialsExt> {
        let auth = require_auth(self.auth)?;

        let info = PostSignatureV4::extract(&multipart).ok_or_else(|| invalid_request!("missing required multipart fields"))?;

        if is_base64_encoded(info.policy.as_bytes()).not() {
            return Err(invalid_request!("invalid field: policy"));
        }

        if info.x_amz_algorithm != "AWS4-HMAC-SHA256" {
            return Err(s3_error!(
                NotImplemented,
                "x-amz-algorithm other than AWS4-HMAC-SHA256 is not implemented"
            ));
        }

        let credential =
            CredentialV4::parse(info.x_amz_credential).map_err(|_| invalid_request!("invalid field: x-amz-credential"))?;

        let amz_date = AmzDate::parse(info.x_amz_date).map_err(|_| invalid_request!("invalid field: x-amz-date"))?;

        // Per AWS SigV4 spec, the signed POST policy must contain eq conditions
        // for x-amz-date, x-amz-credential, and x-amz-algorithm that match the
        // submitted form fields exactly.
        //
        // TODO: the policy is parsed again later in `prepare` via
        // `PostPolicy::from_base64` + `validate_conditions_only`. Consider
        // caching the parsed `PostPolicy` here and reusing it downstream to
        // avoid the double base64-decode + JSON-parse.
        {
            let policy = PostPolicy::from_base64(info.policy).map_err(|e| s3_error!(e, InvalidPolicyDocument))?;

            let policy_date = policy.eq_condition_value("x-amz-date");
            if policy_date != Some(info.x_amz_date) {
                return Err(s3_error!(InvalidPolicyDocument, "x-amz-date does not match policy"));
            }

            let policy_credential = policy.eq_condition_value("x-amz-credential");
            if policy_credential != Some(info.x_amz_credential) {
                return Err(s3_error!(InvalidPolicyDocument, "x-amz-credential does not match policy"));
            }

            let policy_algo = policy.eq_condition_value("x-amz-algorithm");
            if policy_algo != Some(info.x_amz_algorithm) {
                return Err(s3_error!(InvalidPolicyDocument, "x-amz-algorithm does not match policy"));
            }
        }

        // Per AWS SigV4 spec, the credential scope date must match the x-amz-date date.
        if credential.date != amz_date.fmt_date().as_str() {
            return Err(s3_error!(SignatureDoesNotMatch, "credential scope date does not match x-amz-date"));
        }

        let region = credential.aws_region;

        {
            let config = self.config.snapshot();
            validate_sig_v4_region(region, &config)?;
            validate_sig_v4_clock_skew(&amz_date, time::OffsetDateTime::now_utc(), &config)?;
        }

        let access_key = credential.access_key_id.to_owned();
        let secret_key = auth.get_secret_key(&access_key).await?;

        let service = credential.aws_service;

        if !matches!(service, "s3" | "sts") {
            return Err(s3_error!(
                NotImplemented,
                "unknown service '{}' in credential scope; expected 's3' or 'sts'",
                service,
            ));
        }

        let string_to_sign = info.policy;
        let signature = sig_v4::calculate_signature(string_to_sign, &secret_key, &amz_date, region, service);

        let expected_signature = Signature::from_hex(info.x_amz_signature).ok_or_else(|| s3_error!(SignatureDoesNotMatch))?;
        if !Signature::compare(&signature, &expected_signature) {
            debug!(?signature, expected=?expected_signature, "signature mismatch");
            return Err(s3_error!(SignatureDoesNotMatch));
        }

        let region = region.to_owned();
        let service = service.to_owned();

        self.multipart = Some(multipart);
        Ok(CredentialsExt {
            access_key,
            secret_key,
            region: Some(region),
            service: Some(service),
        })
    }

    pub async fn v4_check_presigned_url(&mut self) -> S3Result<CredentialsExt> {
        let qs = self.qs.unwrap(); // assume: qs has "X-Amz-Signature"
        let config = self.config.snapshot();

        let presigned_url = PresignedUrlV4::parse(qs, config.presigned_url_max_expires_secs).map_err(|err| {
            s3_error!(
                err,
                AuthorizationQueryParametersError,
                "The authorization query parameters that you provided are not valid."
            )
        })?;

        if presigned_url.algorithm != "AWS4-HMAC-SHA256" {
            return Err(s3_error!(
                NotImplemented,
                "X-Amz-Algorithm other than AWS4-HMAC-SHA256 is not implemented"
            ));
        }

        // Per AWS SigV4 spec, the credential scope date must match the x-amz-date date.
        if presigned_url.credential.date != presigned_url.amz_date.fmt_date().as_str() {
            return Err(s3_error!(SignatureDoesNotMatch, "credential scope date does not match x-amz-date"));
        }

        let region = presigned_url.credential.aws_region;

        let amz_content_sha256 = extract_amz_content_sha256(self.hs)?;

        // Presigned URLs do not support streaming (chunked) payload signing,
        // so reject them here before reaching the SingleChunk handler below.
        if amz_content_sha256.is_some_and(|v| v.is_streaming()) {
            return Err(s3_error!(NotImplemented, "streaming payload for presigned URLs is not implemented"));
        }

        {
            // check expiration
            validate_sig_v4_region(region, &config)?;

            let now = time::OffsetDateTime::now_utc();

            let date = presigned_url
                .amz_date
                .to_time()
                .ok_or_else(|| invalid_request!("invalid amz date"))?;

            let duration = now - date;

            // Allow requests that are up to max_skew_time_secs in the future.
            // This is to account for clock skew between the client and server.
            // See also https://github.com/minio/minio/blob/b5177993b371817699d3fa25685f54f88d8bfcce/cmd/signature-v4.go#L238-L242

            let max_skew_time = time::Duration::seconds(i64::from(config.presigned_url_max_skew_time_secs));
            if duration.is_negative() && duration.abs() > max_skew_time {
                return Err(s3_error!(RequestTimeTooSkewed, "request date is later than server time too much"));
            }

            if duration > presigned_url.expires {
                return Err(s3_error!(AccessDenied, "Request has expired"));
            }
        }

        let auth = require_auth(self.auth)?;
        let access_key = presigned_url.credential.access_key_id;
        let secret_key = auth.get_secret_key(access_key).await?;

        let service = presigned_url.credential.aws_service;

        if !matches!(service, "s3" | "sts") {
            return Err(s3_error!(
                NotImplemented,
                "unknown service '{}' in credential scope; expected 's3' or 'sts'",
                service,
            ));
        }

        let expected_signature = Signature::from_hex(presigned_url.signature).ok_or_else(|| s3_error!(SignatureDoesNotMatch))?;
        let headers = collect_signed_headers(self.hs, &presigned_url.signed_headers, |name| self.signed_host_fallback(name))?;

        let method = &self.req_method;
        let amz_date = &presigned_url.amz_date;
        let verifier = SignatureVerificationContext {
            expected_signature,
            raw_uri_path: self.raw_uri_path,
            secret_key: &secret_key,
            amz_date,
            region,
            service,
        };
        let canonical_request = sig_v4::create_presigned_canonical_request(method, self.decoded_uri_path, qs.as_ref(), &headers);
        verifier.verify_with_raw_path_fallback(&canonical_request, || {
            sig_v4::create_presigned_canonical_request_with_raw_uri_path(method, self.raw_uri_path, qs.as_ref(), &headers)
        })?;

        // Verify body hash for presigned URL requests.
        // For presigned URLs the canonical request uses UNSIGNED-PAYLOAD (the
        // body is unknown at signing time), but the actual request MUST carry
        // the real SHA256 hash in x-amz-content-sha256, and the server must
        // verify it.  This mirrors MinIO's behavior: the body is wrapped in a
        // hash-validating reader that compares the hash as it is consumed.
        if let Some(AmzContentSha256::SingleChunk(expected_checksum)) = amz_content_sha256 {
            let length = if let Some(content_length) = self.content_length {
                usize::try_from(content_length).map_err(|_| invalid_request!("content-length exceeds platform limits"))?
            } else {
                self.req_body
                    .remaining_length()
                    .exact()
                    .ok_or_else(|| s3_error!(MissingContentLength, "missing header: content-length"))?
            };

            let body = mem::take(self.req_body);
            let stream = UploadStream::new(body, length, expected_checksum);
            *self.req_body = Body::from(stream.into_byte_stream());
        }

        Ok(CredentialsExt {
            access_key: access_key.into(),
            secret_key,
            region: Some(region.into()),
            service: Some(service.into()),
        })
    }

    #[tracing::instrument(skip(self))]
    #[allow(clippy::too_many_lines)]
    pub async fn v4_check_header_auth(&mut self) -> S3Result<CredentialsExt> {
        let authorization: AuthorizationV4<'_> = {
            // assume: headers has "authorization"
            extract_authorization_v4(self.hs)?.unwrap()
        };
        let region = authorization.credential.aws_region;
        let service = authorization.credential.aws_service;

        if !matches!(service, "s3" | "sts") {
            return Err(s3_error!(
                NotImplemented,
                "unknown service '{}' in credential scope; expected 's3' or 'sts'",
                service,
            ));
        }

        let auth = require_auth(self.auth)?;

        // Reject stale requests before doing I/O work (secret key lookup).
        let amz_date = extract_amz_date(self.hs)?.ok_or_else(|| invalid_request!("missing header: x-amz-date"))?;

        // Per AWS SigV4 spec, the credential scope date must match the x-amz-date date.
        if authorization.credential.date != amz_date.fmt_date().as_str() {
            return Err(s3_error!(SignatureDoesNotMatch, "credential scope date does not match x-amz-date"));
        }

        {
            let config = self.config.snapshot();
            validate_sig_v4_region(region, &config)?;
            validate_sig_v4_clock_skew(&amz_date, time::OffsetDateTime::now_utc(), &config)?;
        }

        let amz_content_sha256 = extract_amz_content_sha256(self.hs)?;

        if service == "s3" && amz_content_sha256.is_none() {
            return Err(invalid_request!("missing header: x-amz-content-sha256"));
        }

        let access_key = authorization.credential.access_key_id;
        let secret_key = auth.get_secret_key(access_key).await?;

        let is_stream = amz_content_sha256.is_some_and(|v| v.is_streaming());

        let expected_signature = Signature::from_hex(authorization.signature).ok_or_else(|| s3_error!(SignatureDoesNotMatch))?;
        let method = &self.req_method;
        let query_strings: &[(String, String)] = self.qs.as_ref().map_or(&[], AsRef::as_ref);

        let payload_hash;
        let payload = match amz_content_sha256 {
            Some(AmzContentSha256::StreamingAws4HmacSha256Payload) => sig_v4::Payload::MultipleChunks,
            Some(AmzContentSha256::StreamingAws4HmacSha256PayloadTrailer) => sig_v4::Payload::MultipleChunksWithTrailer,
            Some(AmzContentSha256::UnsignedPayload) => sig_v4::Payload::Unsigned,
            Some(AmzContentSha256::StreamingUnsignedPayloadTrailer) => sig_v4::Payload::UnsignedMultipleChunksWithTrailer,
            Some(AmzContentSha256::SingleChunk(checksum)) => {
                payload_hash = checksum.to_hex_string();
                sig_v4::Payload::SingleChunk(&payload_hash)
            }
            Some(
                AmzContentSha256::StreamingAws4EcdsaP256Sha256Payload
                | AmzContentSha256::StreamingAws4EcdsaP256Sha256PayloadTrailer,
            ) => {
                return Err(s3_error!(NotImplemented, "AWS4-ECDSA-P256-SHA256 signing method is not implemented yet"));
            }
            None => {
                // For STS requests, x-amz-content-sha256 header is not required
                // For S3 requests, this case should have been caught earlier.
                if service == "sts" {
                    // STS requests require computing the payload hash from the body
                    // Read the body (it's small for STS requests like AssumeRole)
                    let body_bytes = self
                        .req_body
                        .store_all_limited(MAX_STS_BODY_SIZE)
                        .await
                        .map_err(|e| invalid_request!("failed to read STS request body: {}", e))?;

                    payload_hash = hex_sha256(&body_bytes, str::to_owned);
                    sig_v4::Payload::SingleChunk(&payload_hash)
                } else {
                    // According to AWS S3 protocol, x-amz-content-sha256 header is required for
                    // all S3 requests authenticated with Signature V4. Reject if missing.
                    return Err(invalid_request!("missing header: x-amz-content-sha256"));
                }
            }
        };

        let headers = collect_signed_headers(self.hs, &authorization.signed_headers, |name| self.signed_host_fallback(name))?;

        let verifier = SignatureVerificationContext {
            expected_signature,
            raw_uri_path: self.raw_uri_path,
            secret_key: &secret_key,
            amz_date: &amz_date,
            region,
            service,
        };
        let canonical_request = sig_v4::create_canonical_request(method, self.decoded_uri_path, query_strings, &headers, payload);
        let signature = verifier.verify_with_raw_path_fallback(&canonical_request, || {
            sig_v4::create_canonical_request_with_raw_uri_path(method, self.raw_uri_path, query_strings, &headers, payload)
        })?;

        if is_stream {
            // For streaming with trailers, AWS requires x-amz-trailer header present.
            let has_trailer = amz_content_sha256.is_some_and(|v| v.has_trailer());
            if has_trailer && http::get_unique_header_str(self.hs, "x-amz-trailer").is_none() {
                return Err(invalid_request!("missing header: x-amz-trailer"));
            }
            let decoded_content_length = self
                .decoded_content_length
                .ok_or_else(|| s3_error!(MissingContentLength, "missing header: x-amz-decoded-content-length"))?;

            let unsigned = matches!(amz_content_sha256, Some(AmzContentSha256::StreamingUnsignedPayloadTrailer));
            let seed_signature = Sha256Sum::from_hex(signature.as_str())
                .ok_or_else(|| s3_error!(InternalError, "verified request signature is not canonical hex"))?;
            let stream = AwsChunkedStream::new(
                mem::take(self.req_body),
                seed_signature,
                amz_date,
                region.into(),
                service.into(),
                secret_key.clone(),
                decoded_content_length,
                unsigned,
                self.config.snapshot().aws_chunked_stream_max_chunk_size,
            );

            debug!(len=?stream.exact_remaining_length(), "aws-chunked");

            // Capture a handle to trailing headers so that it can be exposed to end users
            // via S3Request after the stream is consumed.
            let trailers = stream.trailing_headers_handle();
            self.transformed_body = Some(Body::from(stream.into_byte_stream()));
            self.trailing_headers = Some(trailers);
        } else if let Some(AmzContentSha256::SingleChunk(expected_checksum)) = amz_content_sha256 {
            let length = if let Some(content_length) = self.content_length {
                usize::try_from(content_length).map_err(|_| invalid_request!("content-length exceeds platform limits"))?
            } else {
                self.req_body
                    .remaining_length()
                    .exact()
                    .ok_or_else(|| s3_error!(MissingContentLength, "missing header: content-length"))?
            };

            let body = mem::take(self.req_body);
            let stream = UploadStream::new(body, length, expected_checksum);
            *self.req_body = Body::from(stream.into_byte_stream());
        } else if matches!(amz_content_sha256, Some(AmzContentSha256::UnsignedPayload)) {
            // For non-streaming unsigned payloads, require Content-Length.
            // This aligns with MinIO behavior: PutObject with chunked Transfer-Encoding
            // (no Content-Length) is rejected with MissingContentLength (411).
            if self.content_length.is_none() && self.req_body.remaining_length().exact().is_none() {
                return Err(s3_error!(MissingContentLength, "missing header: content-length"));
            }
        }

        Ok(CredentialsExt {
            access_key: access_key.into(),
            secret_key,
            region: Some(region.into()),
            service: Some(service.into()),
        })
    }

    #[tracing::instrument(skip(self))]
    pub async fn v2_check(&mut self) -> Option<S3Result<CredentialsExt>> {
        // query auth
        if let Some(qs) = self.qs
            && qs.has("Signature")
        {
            debug!("checking presigned url");
            return Some(self.v2_check_presigned_url().await);
        }

        // header auth
        if let Some(auth) = http::get_unique_header_str(self.hs, crate::header::AUTHORIZATION.as_str())
            && let Ok(auth) = AuthorizationV2::parse(auth)
        {
            debug!("checking header auth");
            return Some(self.v2_check_header_auth(auth).await);
        }

        None
    }

    pub async fn v2_check_header_auth(&mut self, auth_v2: AuthorizationV2<'_>) -> S3Result<CredentialsExt> {
        self.ensure_v2_enabled()?;

        let method = &self.req_method;

        let date = http::get_unique_header_str(self.hs, "date").or_else(|| http::get_unique_header_str(self.hs, "x-amz-date"));
        if date.is_none() {
            return Err(invalid_request!("missing date"));
        }

        let auth = require_auth(self.auth)?;
        let access_key = auth_v2.access_key;
        let secret_key = auth.get_secret_key(access_key).await?;

        let string_to_sign = sig_v2::create_string_to_sign(
            sig_v2::Mode::HeaderAuth,
            method,
            self.req_uri.path(),
            self.qs,
            self.hs,
            self.vh_bucket,
        )?;
        let signature = sig_v2::calculate_signature(&secret_key, &string_to_sign);

        debug!(?string_to_sign, "sig_v2 header_auth");

        let expected_signature = Signature::from_base64(auth_v2.signature).ok_or_else(|| s3_error!(SignatureDoesNotMatch))?;
        if !Signature::compare(&signature, &expected_signature) {
            debug!(?signature, expected=?expected_signature, "signature mismatch");
            return Err(s3_error!(SignatureDoesNotMatch));
        }

        Ok(CredentialsExt {
            access_key: access_key.into(),
            secret_key,
            region: None,
            service: Some("s3".into()),
        })
    }

    pub async fn v2_check_post_signature(&mut self, multipart: Multipart) -> S3Result<CredentialsExt> {
        self.ensure_v2_enabled()?;

        let auth = require_auth(self.auth)?;

        let info = PostSignatureV2::extract(&multipart).ok_or_else(|| invalid_request!("missing required multipart fields"))?;

        if is_base64_encoded(info.policy.as_bytes()).not() {
            return Err(invalid_request!("invalid field: policy"));
        }

        let access_key = info.access_key_id.to_owned();
        let secret_key = auth.get_secret_key(&access_key).await?;

        // For v2 POST signature, the string to sign is the base64-encoded policy
        let string_to_sign = info.policy;
        let signature = sig_v2::calculate_signature(&secret_key, string_to_sign);

        let expected_signature = Signature::from_base64(info.signature).ok_or_else(|| s3_error!(SignatureDoesNotMatch))?;
        if !Signature::compare(&signature, &expected_signature) {
            debug!(?signature, expected=?expected_signature, "signature mismatch");
            return Err(s3_error!(SignatureDoesNotMatch));
        }

        self.multipart = Some(multipart);
        Ok(CredentialsExt {
            access_key,
            secret_key,
            region: None,
            service: Some("s3".into()),
        })
    }

    pub async fn v2_check_presigned_url(&mut self) -> S3Result<CredentialsExt> {
        self.ensure_v2_enabled()?;

        let qs = self.qs.unwrap(); // assume: qs has "Signature"
        let presigned_url = PresignedUrlV2::parse(qs).map_err(|err| invalid_request!(err, "missing presigned url v2 fields"))?;

        if time::OffsetDateTime::now_utc() > presigned_url.expires_time {
            return Err(s3_error!(AccessDenied, "Request has expired"));
        }

        let auth = require_auth(self.auth)?;
        let access_key = presigned_url.access_key;
        let secret_key = auth.get_secret_key(access_key).await?;

        let string_to_sign = sig_v2::create_string_to_sign(
            sig_v2::Mode::PresignedUrl,
            self.req_method,
            self.req_uri.path(),
            self.qs,
            self.hs,
            self.vh_bucket,
        )?;
        let signature = sig_v2::calculate_signature(&secret_key, &string_to_sign);

        let expected_signature =
            Signature::from_base64(presigned_url.signature.as_ref()).ok_or_else(|| s3_error!(SignatureDoesNotMatch))?;
        if !Signature::compare(&signature, &expected_signature) {
            debug!(?signature, expected=?expected_signature, "signature mismatch");
            return Err(s3_error!(SignatureDoesNotMatch));
        }

        Ok(CredentialsExt {
            access_key: access_key.into(),
            secret_key,
            region: None,
            service: Some("s3".into()),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use hyper::header::{HeaderName, HeaderValue};

    fn headers_from_slice(slice: &[(&str, &str)]) -> HeaderMap {
        let mut headers = HeaderMap::new();
        for &(name, value) in slice {
            headers.append(
                HeaderName::from_bytes(name.as_bytes()).expect("valid test header name"),
                HeaderValue::from_bytes(value.as_bytes()).expect("valid test header value"),
            );
        }
        headers
    }

    fn fmt_current_amz_date(dt: time::OffsetDateTime) -> String {
        format!(
            "{:04}{:02}{:02}T{:02}{:02}{:02}Z",
            dt.year(),
            u8::from(dt.month()),
            dt.day(),
            dt.hour(),
            dt.minute(),
            dt.second()
        )
    }

    fn presigned_query_fields(amz_date: &AmzDate, service: &str) -> Vec<(String, String)> {
        vec![
            ("X-Amz-Algorithm".to_owned(), "AWS4-HMAC-SHA256".to_owned()),
            (
                "X-Amz-Credential".to_owned(),
                format!("AKIAIOSFODNN7EXAMPLE/{}/us-east-1/{service}/aws4_request", amz_date.fmt_date()),
            ),
            ("X-Amz-Date".to_owned(), amz_date.fmt_iso8601().to_string()),
            ("X-Amz-Expires".to_owned(), "604800".to_owned()),
            ("X-Amz-SignedHeaders".to_owned(), "host".to_owned()),
        ]
    }

    #[test]
    fn test_extract_amz_content_sha256_missing() {
        // Test that extract_amz_content_sha256 returns None when header is missing
        let headers = headers_from_slice(&[("host", "example.s3.amazonaws.com"), ("x-amz-date", "20130524T000000Z")]);
        let result = extract_amz_content_sha256(&headers).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn test_extract_amz_content_sha256_present() {
        // Test that extract_amz_content_sha256 returns Some when header is present
        let headers = headers_from_slice(&[
            ("host", "example.s3.amazonaws.com"),
            ("x-amz-content-sha256", "UNSIGNED-PAYLOAD"),
            ("x-amz-date", "20130524T000000Z"),
        ]);
        let result = extract_amz_content_sha256(&headers).unwrap();
        assert!(result.is_some());
        assert_eq!(result.unwrap(), AmzContentSha256::UnsignedPayload);
    }

    #[test]
    fn test_extract_amz_content_sha256_invalid() {
        // Test that extract_amz_content_sha256 returns error for invalid header value
        let headers = headers_from_slice(&[
            ("host", "example.s3.amazonaws.com"),
            ("x-amz-content-sha256", "INVALID-VALUE"),
            ("x-amz-date", "20130524T000000Z"),
        ]);
        let result = extract_amz_content_sha256(&headers);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.message().unwrap().contains("x-amz-content-sha256"));
    }

    #[test]
    fn collect_signed_headers_preserves_duplicate_arrival_order() {
        let headers = headers_from_slice(&[
            ("host", "example.s3.amazonaws.com"),
            ("x-amz-meta-reviewer", "joe@example.com"),
            ("x-amz-meta-reviewer", "jane@example.com"),
        ]);
        let signed_headers = ["host", "x-amz-meta-reviewer"];

        let collected = collect_signed_headers(&headers, &signed_headers, |_| None).unwrap();

        assert_eq!(
            collected.as_slice(),
            [
                ("host", "example.s3.amazonaws.com"),
                ("x-amz-meta-reviewer", "joe@example.com"),
                ("x-amz-meta-reviewer", "jane@example.com"),
            ]
        );
    }

    #[test]
    fn collect_signed_headers_rejects_missing_header() {
        let headers = headers_from_slice(&[("host", "example.s3.amazonaws.com")]);
        let signed_headers = ["host", "x-amz-meta-missing"];

        let err = collect_signed_headers(&headers, &signed_headers, |_| None).expect_err("missing signed header must fail");

        assert_eq!(err.code(), &S3ErrorCode::SignatureDoesNotMatch);
        assert_eq!(err.message(), Some("missing signed header: x-amz-meta-missing"));
    }

    #[test]
    fn collect_signed_headers_rejects_non_utf8_values() {
        let mut headers = HeaderMap::new();
        headers.insert(
            HeaderName::from_static("x-amz-meta-name"),
            HeaderValue::from_bytes(b"JULI\xC1N").expect("valid opaque header value"),
        );
        let signed_headers = ["x-amz-meta-name"];

        let err = collect_signed_headers(&headers, &signed_headers, |_| Some("example.com"))
            .expect_err("non-UTF-8 value must not be signed");

        assert_eq!(err.code(), &S3ErrorCode::SignatureDoesNotMatch);
        assert_eq!(err.message(), Some("invalid signed header: x-amz-meta-name"));
    }

    #[test]
    fn collect_signed_headers_uses_missing_header_fallback() {
        let headers = HeaderMap::new();
        let signed_headers = ["host"];

        let collected =
            collect_signed_headers(&headers, &signed_headers, |name| (name == "host").then_some("example.com")).unwrap();

        assert_eq!(collected.as_slice(), [("host", "example.com")]);
    }

    #[test]
    fn sig_v4_region_validation_is_optional_and_reports_mismatch() {
        let mut config = S3Config::default();
        validate_sig_v4_region("us-east-1", &config).expect("unset expected region should accept any region");

        config.expected_region = Some("us-west-2".parse().expect("valid test region"));
        validate_sig_v4_region("us-west-2", &config).expect("matching region should be accepted");

        let err = validate_sig_v4_region("us-east-1", &config).expect_err("mismatched region should be rejected");
        assert_eq!(err.code(), &S3ErrorCode::AuthorizationHeaderMalformed);
        assert_eq!(
            err.message(),
            Some("The authorization header is malformed; the region is wrong; expecting 'us-west-2'.")
        );
    }

    #[test]
    fn raw_path_fallback_rejects_missing_or_mismatched_signatures() {
        let secret_key: SecretKey = "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY".into();
        let amz_date = AmzDate::parse("20130524T000000Z").unwrap();
        let method = Method::GET;
        let headers = [
            ("host", "s3.amazonaws.com"),
            ("x-amz-content-sha256", "UNSIGNED-PAYLOAD"),
            ("x-amz-date", "20130524T000000Z"),
        ];

        let canonical_request = sig_v4::create_canonical_request(
            &method,
            "/test-bucket/path",
            &[] as &[(&str, &str)],
            headers,
            sig_v4::Payload::Unsigned,
        );
        let verifier = SignatureVerificationContext {
            expected_signature: Signature::from_hex("0000000000000000000000000000000000000000000000000000000000000000").unwrap(),
            raw_uri_path: "/test-bucket/path",
            secret_key: &secret_key,
            amz_date: &amz_date,
            region: "us-east-1",
            service: "s3",
        };
        let err = verifier
            .verify_with_raw_path_fallback(&canonical_request, || panic!("raw fallback should not be attempted"))
            .expect_err("signature mismatch without raw reserved characters should be rejected");
        assert_eq!(err.code(), &S3ErrorCode::SignatureDoesNotMatch);

        let canonical_request = sig_v4::create_canonical_request(
            &method,
            "/test-bucket/path=",
            &[] as &[(&str, &str)],
            headers,
            sig_v4::Payload::Unsigned,
        );
        let verifier = SignatureVerificationContext {
            expected_signature: Signature::from_hex("0000000000000000000000000000000000000000000000000000000000000000").unwrap(),
            raw_uri_path: "/test-bucket/path=",
            secret_key: &secret_key,
            amz_date: &amz_date,
            region: "us-east-1",
            service: "s3",
        };
        let err = verifier
            .verify_with_raw_path_fallback(&canonical_request, || {
                sig_v4::create_canonical_request_with_raw_uri_path(
                    &method,
                    "/test-bucket/path=",
                    &[] as &[(&str, &str)],
                    headers,
                    sig_v4::Payload::Unsigned,
                )
            })
            .expect_err("raw fallback signature mismatch should be rejected");
        assert_eq!(err.code(), &S3ErrorCode::SignatureDoesNotMatch);
    }

    #[tokio::test]
    async fn v4_presigned_url_rejects_invalid_expires_as_authorization_query_error() {
        use crate::config::StaticConfigProvider;

        let qs = OrderedQs::parse(concat!(
            "X-Amz-Algorithm=AWS4-HMAC-SHA256",
            "&X-Amz-Credential=AKIAIOSFODNN7EXAMPLE%2F20130524%2Fus-east-1%2Fs3%2Faws4_request",
            "&X-Amz-Date=20130524T000000Z",
            "&X-Amz-Expires=604801",
            "&X-Amz-SignedHeaders=host",
            "&X-Amz-Signature=aeeed9bbccd4d02ee5c0109b86d86835f995330da4c265957d157751f604d404"
        ))
        .expect("query should parse");
        let config: Arc<dyn S3ConfigProvider> = Arc::new(StaticConfigProvider::default());
        let method = Method::GET;
        let uri = Uri::from_static("https://s3.amazonaws.com/test.txt");
        let headers = headers_from_slice(&[("authorization", "AWS4-HMAC-SHA256 Credential=invalid")]);
        let mut body = Body::empty();
        let mut cx = SignatureContext {
            auth: None,
            config: &config,
            req_version: ::http::Version::HTTP_11,
            req_method: &method,
            req_uri: &uri,
            req_body: &mut body,
            qs: Some(&qs),
            hs: &headers,
            decoded_uri_path: "/test.txt",
            raw_uri_path: "/test.txt",
            vh_bucket: None,
            content_length: None,
            mime: None,
            decoded_content_length: None,
            transformed_body: None,
            multipart: None,
            trailing_headers: None,
        };

        let err = cx
            .v4_check()
            .await
            .expect("X-Amz-Signature must take precedence over header auth")
            .expect_err("expiration beyond seven days must be rejected before authentication");
        assert_eq!(err.code(), &S3ErrorCode::AuthorizationQueryParametersError);
        assert_eq!(err.message(), Some("The authorization query parameters that you provided are not valid."));
        assert!(err.source().is_some(), "parse error must remain available as the source");
    }

    #[tokio::test]
    async fn v4_presigned_url_accepts_expires_beyond_aws_default_when_configured() {
        use crate::config::{S3Config, S3ConfigProvider, StaticConfigProvider};

        let amz_date = AmzDate::parse(&fmt_current_amz_date(time::OffsetDateTime::now_utc()))
            .expect("current time should produce a valid x-amz-date");
        let mut query_strings = presigned_query_fields(&amz_date, "s3");
        query_strings[3] = ("X-Amz-Expires".to_owned(), "604801".to_owned());
        query_strings.push((
            "X-Amz-Signature".to_owned(),
            "aeeed9bbccd4d02ee5c0109b86d86835f995330da4c265957d157751f604d404".to_owned(),
        ));
        let qs = OrderedQs::from_vec_unchecked(query_strings);

        let s3_config = S3Config {
            presigned_url_max_skew_time_secs: u32::MAX,
            presigned_url_max_expires_secs: 700_000,
            ..Default::default()
        };
        let config: Arc<dyn S3ConfigProvider> = Arc::new(StaticConfigProvider::new(Arc::new(s3_config)));
        let method = Method::GET;
        let uri = Uri::from_static("https://s3.amazonaws.com/test.txt");
        let headers = headers_from_slice(&[("host", "s3.amazonaws.com")]);
        let mut body = Body::empty();
        let mut cx = SignatureContext {
            auth: None,
            config: &config,
            req_version: ::http::Version::HTTP_11,
            req_method: &method,
            req_uri: &uri,
            req_body: &mut body,
            qs: Some(&qs),
            hs: &headers,
            decoded_uri_path: "/test.txt",
            raw_uri_path: "/test.txt",
            vh_bucket: None,
            content_length: None,
            mime: None,
            decoded_content_length: None,
            transformed_body: None,
            multipart: None,
            trailing_headers: None,
        };

        let err = cx
            .v4_check()
            .await
            .expect("X-Amz-Signature must select presigned auth")
            .expect_err("missing auth provider should fail after presigned parsing succeeds");
        assert_eq!(err.code(), &S3ErrorCode::NotImplemented);
    }

    #[tokio::test]
    async fn x_amz_expires_limit_applies_only_to_presigned_query_auth() {
        use crate::config::StaticConfigProvider;

        let qs = OrderedQs::parse("X-Amz-Expires=604801").expect("query should parse");
        let config: Arc<dyn S3ConfigProvider> = Arc::new(StaticConfigProvider::default());
        let method = Method::GET;
        let uri = Uri::from_static("https://s3.amazonaws.com/test.txt");
        let anonymous_headers = HeaderMap::new();
        let mut body = Body::empty();
        let mut anonymous = SignatureContext {
            auth: None,
            config: &config,
            req_version: ::http::Version::HTTP_11,
            req_method: &method,
            req_uri: &uri,
            req_body: &mut body,
            qs: Some(&qs),
            hs: &anonymous_headers,
            decoded_uri_path: "/test.txt",
            raw_uri_path: "/test.txt",
            vh_bucket: None,
            content_length: None,
            mime: None,
            decoded_content_length: None,
            transformed_body: None,
            multipart: None,
            trailing_headers: None,
        };
        assert!(
            anonymous.v4_check().await.is_none(),
            "an unsigned query must not be treated as presigned auth"
        );

        let header_auth_headers = headers_from_slice(&[("authorization", "AWS4-HMAC-SHA256 Credential=invalid")]);
        let mut body = Body::empty();
        let mut header_auth = SignatureContext {
            auth: None,
            config: &config,
            req_version: ::http::Version::HTTP_11,
            req_method: &method,
            req_uri: &uri,
            req_body: &mut body,
            qs: Some(&qs),
            hs: &header_auth_headers,
            decoded_uri_path: "/test.txt",
            raw_uri_path: "/test.txt",
            vh_bucket: None,
            content_length: None,
            mime: None,
            decoded_content_length: None,
            transformed_body: None,
            multipart: None,
            trailing_headers: None,
        };
        let err = header_auth
            .v4_check()
            .await
            .expect("authorization header must select header auth")
            .expect_err("malformed header auth should fail");
        assert_ne!(err.code(), &S3ErrorCode::AuthorizationQueryParametersError);
    }

    #[tokio::test]
    async fn post_signature_allows_anonymous() {
        use crate::config::{S3ConfigProvider, StaticConfigProvider};
        use std::sync::Arc;

        let boundary = "boundary123";
        let body = format!(
            "\r\n--{boundary}\r\n\
Content-Disposition: form-data; name=\"key\"; filename=\"key\"\r\n\r\n\
foo.txt\r\n\
--{boundary}\r\n\
Content-Disposition: form-data; name=\"file\"; filename=\"file.txt\"\r\n\
Content-Type: text/plain\r\n\r\n\
file content\r\n\
--{boundary}--\r\n"
        );
        let mut body = Body::from(body);
        let mime: Mime = format!("multipart/form-data; boundary={boundary}").parse().unwrap();

        let config: Arc<dyn S3ConfigProvider> = Arc::new(StaticConfigProvider::default());
        let method = Method::POST;
        let uri = Uri::from_static("http://localhost/test-bucket");
        let headers = HeaderMap::new();

        let mut cx = SignatureContext {
            auth: None,
            config: &config,
            req_version: ::http::Version::HTTP_11,
            req_method: &method,
            req_uri: &uri,
            req_body: &mut body,
            qs: None,
            hs: &headers,
            decoded_uri_path: "/test-bucket",
            raw_uri_path: "/test-bucket",
            vh_bucket: None,
            content_length: None,
            mime: Some(mime),
            decoded_content_length: None,
            transformed_body: None,
            multipart: None,
            trailing_headers: None,
        };

        let credentials = cx.check().await.unwrap();
        assert!(credentials.is_none(), "anonymous POST should not require credentials");

        let multipart = cx.multipart.expect("multipart should be stored");
        assert_eq!(multipart.find_field_value("key"), Some("foo.txt"));
        assert_eq!(multipart.file.name, "file.txt");
    }

    fn sig_v2_test_config(enable_sig_v2: bool) -> Arc<dyn S3ConfigProvider> {
        use crate::config::{S3Config, StaticConfigProvider};

        let config = S3Config {
            enable_sig_v2,
            ..Default::default()
        };
        Arc::new(StaticConfigProvider::new(Arc::new(config)))
    }

    #[allow(clippy::too_many_arguments)]
    fn sig_v2_test_context<'a>(
        config: &'a Arc<dyn S3ConfigProvider>,
        method: &'a Method,
        uri: &'a Uri,
        body: &'a mut Body,
        qs: Option<&'a OrderedQs>,
        hs: &'a HeaderMap,
        mime: Option<Mime>,
    ) -> SignatureContext<'a> {
        SignatureContext {
            auth: None,
            config,
            req_version: ::http::Version::HTTP_11,
            req_method: method,
            req_uri: uri,
            req_body: body,
            qs,
            hs,
            decoded_uri_path: "/test.txt",
            raw_uri_path: "/test.txt",
            vh_bucket: None,
            content_length: None,
            mime,
            decoded_content_length: None,
            transformed_body: None,
            multipart: None,
            trailing_headers: None,
        }
    }

    #[tokio::test]
    async fn sig_v2_header_auth_rejected_when_disabled() {
        let config = sig_v2_test_config(false);
        let method = Method::GET;
        let uri = Uri::from_static("https://s3.amazonaws.com/test.txt");
        let headers = headers_from_slice(&[("authorization", "AWS AKIAIOSFODNN7EXAMPLE:qgk2+6Sv9/oM7G3qLEjTH1a1l1g=")]);
        let mut body = Body::empty();
        let mut cx = sig_v2_test_context(&config, &method, &uri, &mut body, None, &headers, None);

        let err = cx
            .v2_check()
            .await
            .expect("v2 header auth must be detected")
            .expect_err("SigV2 must be rejected when disabled");
        assert_eq!(err.code(), &S3ErrorCode::AccessDenied);
    }

    #[tokio::test]
    async fn sig_v2_presigned_url_rejected_when_disabled() {
        let config = sig_v2_test_config(false);
        let method = Method::GET;
        let uri = Uri::from_static("https://s3.amazonaws.com/test.txt");
        let qs = OrderedQs::parse("AWSAccessKeyId=AKIAIOSFODNN7EXAMPLE&Signature=abc&Expires=1175139620").unwrap();
        let headers = HeaderMap::new();
        let mut body = Body::empty();
        let mut cx = sig_v2_test_context(&config, &method, &uri, &mut body, Some(&qs), &headers, None);

        let err = cx
            .v2_check()
            .await
            .expect("v2 presigned url must be detected")
            .expect_err("SigV2 must be rejected when disabled");
        assert_eq!(err.code(), &S3ErrorCode::AccessDenied);
    }

    #[tokio::test]
    async fn sig_v2_post_rejected_when_disabled() {
        let boundary = "boundary123";
        let body = format!(
            "\r\n--{boundary}\r\n\
Content-Disposition: form-data; name=\"signature\"\r\n\r\n\
abc\r\n\
--{boundary}\r\n\
Content-Disposition: form-data; name=\"key\"; filename=\"key\"\r\n\r\n\
foo.txt\r\n\
--{boundary}\r\n\
Content-Disposition: form-data; name=\"file\"; filename=\"file.txt\"\r\n\
Content-Type: text/plain\r\n\r\n\
file content\r\n\
--{boundary}--\r\n"
        );
        let mut body = Body::from(body);
        let mime: Mime = format!("multipart/form-data; boundary={boundary}").parse().unwrap();

        let config = sig_v2_test_config(false);
        let method = Method::POST;
        let uri = Uri::from_static("http://localhost/test-bucket");
        let headers = HeaderMap::new();
        let mut cx = sig_v2_test_context(&config, &method, &uri, &mut body, None, &headers, Some(mime));

        let err = cx.check().await.expect_err("SigV2 POST must be rejected when disabled");
        assert_eq!(err.code(), &S3ErrorCode::AccessDenied);
    }

    #[tokio::test]
    async fn sig_v2_passes_gate_when_enabled() {
        let config = sig_v2_test_config(true);
        let method = Method::GET;
        let uri = Uri::from_static("https://s3.amazonaws.com/test.txt");

        // When SigV2 is explicitly enabled, the gate passes and the request proceeds to
        // signature verification; without an auth provider it fails at the auth
        // lookup with NotImplemented, not AccessDenied.
        let headers = headers_from_slice(&[
            ("authorization", "AWS AKIAIOSFODNN7EXAMPLE:qgk2+6Sv9/oM7G3qLEjTH1a1l1g="),
            ("date", "Mon, 26 Nov 2024 00:00:00 GMT"),
        ]);
        let mut body = Body::empty();
        let mut cx = sig_v2_test_context(&config, &method, &uri, &mut body, None, &headers, None);

        let err = cx
            .v2_check()
            .await
            .expect("v2 header auth must be detected")
            .expect_err("header auth without provider should fail at auth lookup");
        assert_eq!(err.code(), &S3ErrorCode::NotImplemented);
    }

    #[tokio::test]
    async fn test_sts_body_hash_computation() {
        // Test that STS request body hash is computed correctly
        // Typical STS AssumeRole request body
        let body_content = b"Action=AssumeRole&RoleArn=arn:aws:iam::123456789012:role/test-role&RoleSessionName=test-session";

        // Compute hash
        let hash = hex_sha256(body_content, str::to_owned);

        // Verify hash is a valid hex string of correct length (64 chars for SHA256)
        assert_eq!(hash.len(), 64);
        assert!(hash.chars().all(|c| c.is_ascii_hexdigit()));

        // Verify hash is deterministic
        let hash2 = hex_sha256(body_content, str::to_owned);
        assert_eq!(hash, hash2);
    }

    #[tokio::test]
    async fn test_sts_body_size_limit_enforced() {
        // Test that body size limit is enforced for STS requests
        use bytes::Bytes;

        // Create a body that exceeds MAX_STS_BODY_SIZE
        let large_body = vec![b'x'; MAX_STS_BODY_SIZE + 1];
        let mut body = Body::from(Bytes::from(large_body));

        // Try to read with limit
        let result = body.store_all_limited(MAX_STS_BODY_SIZE).await;

        // Should fail due to size limit
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_sts_body_within_limit() {
        // Test that body reading succeeds when within limit
        use bytes::Bytes;

        // Create a body within the limit
        let small_body = b"Action=AssumeRole&RoleArn=test&RoleSessionName=session";
        let mut body = Body::from(Bytes::from(&small_body[..]));

        // Try to read with limit
        let result = body.store_all_limited(MAX_STS_BODY_SIZE).await;

        // Should succeed
        assert!(result.is_ok());
        let bytes = result.unwrap();
        assert_eq!(&bytes[..], &small_body[..]);
    }

    #[test]
    fn test_sts_max_body_size_constant() {
        // Verify the constant is set to a reasonable value
        assert_eq!(MAX_STS_BODY_SIZE, 8192);
        // STS requests are typically small (under 2KB for AssumeRole)
        // 8KB provides a good safety margin
    }

    /// V4 presigned URL with an unknown service name must be rejected as `NotImplemented`.
    ///
    /// Covers the service whitelist fix in `v4_check_presigned_url`.
    #[tokio::test]
    async fn v4_presigned_url_rejects_unknown_service() {
        use crate::S3ErrorCode;
        use crate::auth::SecretKey;
        use crate::config::{S3ConfigProvider, StaticConfigProvider};
        use std::sync::Arc;

        let amz_date = AmzDate::parse(&fmt_current_amz_date(time::OffsetDateTime::now_utc()))
            .expect("current time should produce a valid x-amz-date");
        let mut query_strings = presigned_query_fields(&amz_date, "custom-svc");
        query_strings.push((
            "X-Amz-Signature".to_owned(),
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_owned(),
        ));
        let qs = OrderedQs::from_vec_unchecked(query_strings);

        let access_key = "AKIAIOSFODNN7EXAMPLE";
        let secret_key: SecretKey = "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY".into();
        let auth = crate::auth::SimpleAuth::from_single(access_key, secret_key);
        let config: Arc<dyn S3ConfigProvider> = Arc::new(StaticConfigProvider::default());

        let method = Method::GET;
        let uri = Uri::from_static("https://s3.amazonaws.com/test.txt");
        let headers = HeaderMap::new();
        let mut body = Body::empty();

        let mut cx = SignatureContext {
            auth: Some(&auth),
            config: &config,
            req_version: ::http::Version::HTTP_11,
            req_method: &method,
            req_uri: &uri,
            req_body: &mut body,
            qs: Some(&qs),
            hs: &headers,
            decoded_uri_path: "/test.txt",
            raw_uri_path: "/test.txt",
            vh_bucket: None,
            content_length: None,
            mime: None,
            decoded_content_length: None,
            transformed_body: None,
            multipart: None,
            trailing_headers: None,
        };

        let err = cx
            .v4_check_presigned_url()
            .await
            .expect_err("unknown service must be rejected");
        assert_eq!(err.code(), &S3ErrorCode::NotImplemented);
    }

    #[tokio::test]
    async fn v4_presigned_url_rejects_wrong_region() {
        use crate::config::{S3ConfigProvider, StaticConfigProvider};
        use std::sync::Arc;

        let qs = OrderedQs::parse(concat!(
            "X-Amz-Algorithm=AWS4-HMAC-SHA256",
            "&X-Amz-Credential=AKIAIOSFODNN7EXAMPLE%2F20130524%2Fus-east-1%2Fs3%2Faws4_request",
            "&X-Amz-Date=20130524T000000Z",
            "&X-Amz-Expires=3600",
            "&X-Amz-SignedHeaders=host",
            "&X-Amz-Signature=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        ))
        .unwrap();

        let auth = crate::ops::tests::NeverGetSecretKeyAuth;
        let s3_config = S3Config {
            expected_region: Some("us-west-2".parse().expect("valid test region")),
            ..Default::default()
        };
        let config: Arc<dyn S3ConfigProvider> = Arc::new(StaticConfigProvider::new(Arc::new(s3_config)));

        let method = Method::GET;
        let uri = Uri::from_static("https://s3.amazonaws.com/test.txt");
        let headers = headers_from_slice(&[("host", "s3.amazonaws.com")]);
        let mut body = Body::empty();
        let mut cx = SignatureContext {
            auth: Some(&auth),
            config: &config,
            req_version: ::http::Version::HTTP_11,
            req_method: &method,
            req_uri: &uri,
            req_body: &mut body,
            qs: Some(&qs),
            hs: &headers,
            decoded_uri_path: "/test.txt",
            raw_uri_path: "/test.txt",
            vh_bucket: None,
            content_length: None,
            mime: None,
            decoded_content_length: None,
            transformed_body: None,
            multipart: None,
            trailing_headers: None,
        };

        let err = cx
            .v4_check_presigned_url()
            .await
            .expect_err("presigned URL for another region should be rejected");
        assert_eq!(err.code(), &S3ErrorCode::AuthorizationHeaderMalformed);
    }

    #[tokio::test]
    async fn v4_presigned_url_accepts_standard_and_raw_uri_path_signatures() {
        use crate::auth::SecretKey;
        use crate::auth::SimpleAuth;
        use crate::config::{S3ConfigProvider, StaticConfigProvider};
        use std::sync::Arc;

        let access_key = "AKIAIOSFODNN7EXAMPLE";
        let secret_key: SecretKey = "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY".into();
        let auth = SimpleAuth::from_single(access_key, secret_key.clone());
        let s3_config = S3Config {
            expected_region: Some("us-east-1".parse().expect("valid test region")),
            ..Default::default()
        };
        let config: Arc<dyn S3ConfigProvider> = Arc::new(StaticConfigProvider::new(Arc::new(s3_config)));

        let method = Method::GET;
        let uri = Uri::from_static("https://s3.amazonaws.com/test-bucket/path/sitemap.xmlage=");
        let decoded_uri_path = "/test-bucket/path/sitemap.xmlage=";
        let raw_uri_path = "/test-bucket/path/sitemap.xmlage=";
        let amz_date = AmzDate::parse(&fmt_current_amz_date(time::OffsetDateTime::now_utc()))
            .expect("current time should produce a valid x-amz-date");
        let headers_for_signing = [("host", "s3.amazonaws.com")];
        let query_strings_for_signing = presigned_query_fields(&amz_date, "s3");

        let canonical_requests = [
            sig_v4::create_presigned_canonical_request(
                &method,
                decoded_uri_path,
                &query_strings_for_signing,
                headers_for_signing,
            ),
            sig_v4::create_presigned_canonical_request_with_raw_uri_path(
                &method,
                raw_uri_path,
                &query_strings_for_signing,
                headers_for_signing,
            ),
        ];
        assert_ne!(canonical_requests[0], canonical_requests[1]);

        for canonical_request in canonical_requests {
            let string_to_sign = sig_v4::create_string_to_sign(&canonical_request, &amz_date, "us-east-1", "s3");
            let signature = sig_v4::calculate_signature(&string_to_sign, &secret_key, &amz_date, "us-east-1", "s3");
            let mut signed_query_strings = query_strings_for_signing.clone();
            signed_query_strings.push(("X-Amz-Signature".to_owned(), signature.as_str().to_owned()));
            let qs = OrderedQs::from_vec_unchecked(signed_query_strings);
            let headers = headers_from_slice(&[("host", "s3.amazonaws.com")]);

            let mut body = Body::empty();
            let mut cx = SignatureContext {
                auth: Some(&auth),
                config: &config,
                req_version: ::http::Version::HTTP_11,
                req_method: &method,
                req_uri: &uri,
                req_body: &mut body,
                qs: Some(&qs),
                hs: &headers,
                decoded_uri_path,
                raw_uri_path,
                vh_bucket: None,
                content_length: None,
                mime: None,
                decoded_content_length: None,
                transformed_body: None,
                multipart: None,
                trailing_headers: None,
            };

            let cred = cx
                .v4_check_presigned_url()
                .await
                .expect("valid presigned URL with a raw '=' URI path should succeed");
            assert_eq!(cred.access_key, access_key);
        }
    }

    #[tokio::test]
    async fn v4_presigned_url_uses_http2_authority_for_signed_host() {
        use crate::auth::SecretKey;
        use crate::auth::SimpleAuth;
        use crate::config::{S3ConfigProvider, StaticConfigProvider};
        use std::sync::Arc;

        let access_key = "AKIAIOSFODNN7EXAMPLE";
        let secret_key: SecretKey = "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY".into();
        let auth = SimpleAuth::from_single(access_key, secret_key.clone());
        let config: Arc<dyn S3ConfigProvider> = Arc::new(StaticConfigProvider::default());

        let method = Method::GET;
        let uri = Uri::from_static("https://s3.amazonaws.com/test-bucket/path/sitemap.xmlage=");
        let decoded_uri_path = "/test-bucket/path/sitemap.xmlage=";
        let raw_uri_path = "/test-bucket/path/sitemap.xmlage=";
        let amz_date = AmzDate::parse(&fmt_current_amz_date(time::OffsetDateTime::now_utc()))
            .expect("current time should produce a valid x-amz-date");
        let headers_for_signing = [("host", "s3.amazonaws.com")];
        let query_strings_for_signing = presigned_query_fields(&amz_date, "s3");
        let canonical_request = sig_v4::create_presigned_canonical_request(
            &method,
            decoded_uri_path,
            &query_strings_for_signing,
            headers_for_signing,
        );
        let string_to_sign = sig_v4::create_string_to_sign(&canonical_request, &amz_date, "us-east-1", "s3");
        let signature = sig_v4::calculate_signature(&string_to_sign, &secret_key, &amz_date, "us-east-1", "s3");
        let mut signed_query_strings = query_strings_for_signing;
        signed_query_strings.push(("X-Amz-Signature".to_owned(), signature.as_str().to_owned()));
        let qs = OrderedQs::from_vec_unchecked(signed_query_strings);

        let headers = HeaderMap::new();
        let mut body = Body::empty();
        let mut cx = SignatureContext {
            auth: Some(&auth),
            config: &config,
            req_version: ::http::Version::HTTP_2,
            req_method: &method,
            req_uri: &uri,
            req_body: &mut body,
            qs: Some(&qs),
            hs: &headers,
            decoded_uri_path,
            raw_uri_path,
            vh_bucket: None,
            content_length: None,
            mime: None,
            decoded_content_length: None,
            transformed_body: None,
            multipart: None,
            trailing_headers: None,
        };

        let cred = cx
            .v4_check_presigned_url()
            .await
            .expect("HTTP/2 authority should be used for a signed host header");
        assert_eq!(cred.access_key, access_key);
    }

    #[tokio::test]
    async fn v4_presigned_url_put_with_valid_content_sha256() {
        use crate::auth::SecretKey;
        use crate::auth::SimpleAuth;
        use crate::config::{S3Config, S3ConfigProvider, StaticConfigProvider};
        use bytes::Bytes;
        use std::sync::Arc;

        let access_key = "AKIAIOSFODNN7EXAMPLE";
        let secret_key: SecretKey = "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY".into();
        let auth = SimpleAuth::from_single(access_key, secret_key.clone());
        let s3_config = S3Config {
            presigned_url_max_skew_time_secs: u32::MAX,
            ..Default::default()
        };
        let config: Arc<dyn S3ConfigProvider> = Arc::new(StaticConfigProvider::new(Arc::new(s3_config)));

        let body_data = b"hello world";
        let content_sha256 = hex_sha256(body_data, str::to_owned);
        let method = Method::PUT;
        let uri = Uri::from_static("https://s3.amazonaws.com/test-bucket/test-key");
        let amz_date = AmzDate::parse(&fmt_current_amz_date(time::OffsetDateTime::now_utc()))
            .expect("current time should produce a valid x-amz-date");
        let headers_for_signing = [("host", "s3.amazonaws.com")];
        let query_strings_for_signing = presigned_query_fields(&amz_date, "s3");

        let canonical_request = sig_v4::create_presigned_canonical_request(
            &method,
            "/test-bucket/test-key",
            &query_strings_for_signing,
            headers_for_signing,
        );
        let string_to_sign = sig_v4::create_string_to_sign(&canonical_request, &amz_date, "us-east-1", "s3");
        let signature = sig_v4::calculate_signature(&string_to_sign, &secret_key, &amz_date, "us-east-1", "s3");

        let mut signed_query_strings = query_strings_for_signing;
        signed_query_strings.push(("X-Amz-Signature".to_owned(), signature.as_str().to_owned()));
        let qs = OrderedQs::from_vec_unchecked(signed_query_strings);

        let headers = headers_from_slice(&[
            ("host", "s3.amazonaws.com"),
            ("x-amz-content-sha256", content_sha256.as_str()),
        ]);

        let mut body = Body::from(Bytes::from_static(body_data));
        let mut cx = SignatureContext {
            auth: Some(&auth),
            config: &config,
            req_version: ::http::Version::HTTP_11,
            req_method: &method,
            req_uri: &uri,
            req_body: &mut body,
            qs: Some(&qs),
            hs: &headers,
            decoded_uri_path: "/test-bucket/test-key",
            raw_uri_path: "/test-bucket/test-key",
            vh_bucket: None,
            content_length: Some(body_data.len() as u64),
            mime: None,
            decoded_content_length: None,
            transformed_body: None,
            multipart: None,
            trailing_headers: None,
        };

        let cred = cx
            .v4_check_presigned_url()
            .await
            .expect("PUT presigned URL with valid content-sha256 should succeed");
        assert_eq!(cred.access_key, access_key);

        // Verify body was replaced with UploadStream: reading it back gives original data
        let stored = cx
            .req_body
            .store_all_limited(100)
            .await
            .expect("body should be readable through UploadStream");
        assert_eq!(stored, &body_data[..]);
    }

    #[tokio::test]
    async fn v4_presigned_url_put_rejects_streaming_content_sha256() {
        use crate::auth::SecretKey;
        use crate::auth::SimpleAuth;
        use crate::config::{S3Config, S3ConfigProvider, StaticConfigProvider};
        use bytes::Bytes;
        use std::sync::Arc;

        let access_key = "AKIAIOSFODNN7EXAMPLE";
        let secret_key: SecretKey = "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY".into();
        let auth = SimpleAuth::from_single(access_key, secret_key.clone());
        let s3_config = S3Config {
            presigned_url_max_skew_time_secs: u32::MAX,
            ..Default::default()
        };
        let config: Arc<dyn S3ConfigProvider> = Arc::new(StaticConfigProvider::new(Arc::new(s3_config)));

        let body_data = b"hello";
        let method = Method::PUT;
        let uri = Uri::from_static("https://s3.amazonaws.com/test-bucket/test-key");
        let amz_date = AmzDate::parse(&fmt_current_amz_date(time::OffsetDateTime::now_utc()))
            .expect("current time should produce a valid x-amz-date");
        let headers_for_signing = [("host", "s3.amazonaws.com")];
        let query_strings_for_signing = presigned_query_fields(&amz_date, "s3");

        let canonical_request = sig_v4::create_presigned_canonical_request(
            &method,
            "/test-bucket/test-key",
            &query_strings_for_signing,
            headers_for_signing,
        );
        let string_to_sign = sig_v4::create_string_to_sign(&canonical_request, &amz_date, "us-east-1", "s3");
        let signature = sig_v4::calculate_signature(&string_to_sign, &secret_key, &amz_date, "us-east-1", "s3");

        let mut signed_query_strings = query_strings_for_signing;
        signed_query_strings.push(("X-Amz-Signature".to_owned(), signature.as_str().to_owned()));
        let qs = OrderedQs::from_vec_unchecked(signed_query_strings);

        let headers = headers_from_slice(&[
            ("host", "s3.amazonaws.com"),
            ("x-amz-content-sha256", "STREAMING-AWS4-HMAC-SHA256-PAYLOAD"),
        ]);

        let mut body = Body::from(Bytes::from_static(body_data));
        let mut cx = SignatureContext {
            auth: Some(&auth),
            config: &config,
            req_version: ::http::Version::HTTP_11,
            req_method: &method,
            req_uri: &uri,
            req_body: &mut body,
            qs: Some(&qs),
            hs: &headers,
            decoded_uri_path: "/test-bucket/test-key",
            raw_uri_path: "/test-bucket/test-key",
            vh_bucket: None,
            content_length: Some(body_data.len() as u64),
            mime: None,
            decoded_content_length: None,
            transformed_body: None,
            multipart: None,
            trailing_headers: None,
        };

        let err = cx
            .v4_check_presigned_url()
            .await
            .expect_err("streaming content-sha256 should be rejected");
        assert_eq!(err.code(), &S3ErrorCode::NotImplemented);
    }

    #[tokio::test]
    async fn v4_header_auth_rejects_wrong_region() {
        use crate::config::{S3Config, S3ConfigProvider, StaticConfigProvider};
        use std::sync::Arc;

        let access_key = "AKIAIOSFODNN7EXAMPLE";
        let auth = crate::ops::tests::NeverGetSecretKeyAuth;
        let s3_config = S3Config {
            presigned_url_max_skew_time_secs: u32::MAX,
            expected_region: Some("us-west-2".parse().expect("valid test region")),
            ..Default::default()
        };
        let config: Arc<dyn S3ConfigProvider> = Arc::new(StaticConfigProvider::new(Arc::new(s3_config)));

        let authorization = format!(
            "AWS4-HMAC-SHA256 Credential={access_key}/20130524/us-east-1/s3/aws4_request, SignedHeaders=host;x-amz-content-sha256;x-amz-date, Signature=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
        );
        let headers = headers_from_slice(&[
            ("authorization", authorization.as_str()),
            ("host", "s3.amazonaws.com"),
            ("x-amz-content-sha256", "UNSIGNED-PAYLOAD"),
            ("x-amz-date", "20130524T000000Z"),
        ]);
        let method = Method::GET;
        let uri = Uri::from_static("https://s3.amazonaws.com/test.txt");
        let mut body = Body::empty();
        let mut cx = SignatureContext {
            auth: Some(&auth),
            config: &config,
            req_version: ::http::Version::HTTP_11,
            req_method: &method,
            req_uri: &uri,
            req_body: &mut body,
            qs: None,
            hs: &headers,
            decoded_uri_path: "/test.txt",
            raw_uri_path: "/test.txt",
            vh_bucket: None,
            content_length: Some(0),
            mime: None,
            decoded_content_length: None,
            transformed_body: None,
            multipart: None,
            trailing_headers: None,
        };

        let err = cx
            .v4_check_header_auth()
            .await
            .expect_err("header signature for another region should be rejected");
        assert_eq!(err.code(), &S3ErrorCode::AuthorizationHeaderMalformed);
    }

    #[tokio::test]
    async fn v4_header_auth_accepts_standard_and_raw_uri_path_signatures() {
        use crate::auth::SecretKey;
        use crate::auth::SimpleAuth;
        use crate::config::{S3Config, S3ConfigProvider, StaticConfigProvider};
        use std::sync::Arc;

        let access_key = "AKIAIOSFODNN7EXAMPLE";
        let secret_key: SecretKey = "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY".into();
        let auth = SimpleAuth::from_single(access_key, secret_key.clone());
        let s3_config = S3Config {
            presigned_url_max_skew_time_secs: u32::MAX,
            expected_region: Some("us-east-1".parse().expect("valid test region")),
            ..Default::default()
        };
        let config: Arc<dyn S3ConfigProvider> = Arc::new(StaticConfigProvider::new(Arc::new(s3_config)));

        let method = Method::GET;
        let uri = Uri::from_static("https://s3.amazonaws.com/test-bucket/path/sitemap.xmlage=");
        let decoded_uri_path = "/test-bucket/path/sitemap.xmlage=";
        let raw_uri_path = "/test-bucket/path/sitemap.xmlage=";
        let amz_date = AmzDate::parse("20130524T000000Z").unwrap();
        let headers_for_signing = [
            ("host", "s3.amazonaws.com"),
            ("x-amz-content-sha256", "UNSIGNED-PAYLOAD"),
            ("x-amz-date", "20130524T000000Z"),
        ];

        let canonical_requests = [
            sig_v4::create_canonical_request(
                &method,
                decoded_uri_path,
                &[] as &[(&str, &str)],
                headers_for_signing,
                sig_v4::Payload::Unsigned,
            ),
            sig_v4::create_canonical_request_with_raw_uri_path(
                &method,
                raw_uri_path,
                &[] as &[(&str, &str)],
                headers_for_signing,
                sig_v4::Payload::Unsigned,
            ),
        ];

        for canonical_request in canonical_requests {
            let string_to_sign = sig_v4::create_string_to_sign(&canonical_request, &amz_date, "us-east-1", "s3");
            let signature = sig_v4::calculate_signature(&string_to_sign, &secret_key, &amz_date, "us-east-1", "s3");
            let authorization = format!(
                "AWS4-HMAC-SHA256 Credential={access_key}/20130524/us-east-1/s3/aws4_request, SignedHeaders=host;x-amz-content-sha256;x-amz-date, Signature={}",
                signature.as_str(),
            );
            let headers = headers_from_slice(&[
                ("authorization", authorization.as_str()),
                ("host", "s3.amazonaws.com"),
                ("x-amz-content-sha256", "UNSIGNED-PAYLOAD"),
                ("x-amz-date", "20130524T000000Z"),
            ]);

            let mut body = Body::empty();
            let mut cx = SignatureContext {
                auth: Some(&auth),
                config: &config,
                req_version: ::http::Version::HTTP_11,
                req_method: &method,
                req_uri: &uri,
                req_body: &mut body,
                qs: None,
                hs: &headers,
                decoded_uri_path,
                raw_uri_path,
                vh_bucket: None,
                content_length: Some(0),
                mime: None,
                decoded_content_length: None,
                transformed_body: None,
                multipart: None,
                trailing_headers: None,
            };

            let cred = cx
                .v4_check_header_auth()
                .await
                .expect("valid SigV4 auth with a raw '=' URI path should succeed");
            assert_eq!(cred.access_key, access_key);
        }
    }

    #[tokio::test]
    async fn v4_header_auth_accepts_rest_base64_content_sha256() {
        use crate::auth::SecretKey;
        use crate::auth::SimpleAuth;
        use crate::config::{S3Config, S3ConfigProvider, StaticConfigProvider};
        use bytes::Bytes;
        use std::sync::Arc;

        let access_key = "AKIAIOSFODNN7EXAMPLE";
        let secret_key: SecretKey = "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY".into();
        let auth = SimpleAuth::from_single(access_key, secret_key.clone());
        let s3_config = S3Config {
            presigned_url_max_skew_time_secs: u32::MAX,
            ..Default::default()
        };
        let config: Arc<dyn S3ConfigProvider> = Arc::new(StaticConfigProvider::new(Arc::new(s3_config)));

        let method = Method::POST;
        let uri = Uri::from_static("https://s3.amazonaws.com/iceberg/v1/catalog/commit");
        let path = "/iceberg/v1/catalog/commit";
        let body_data = b"hello world";
        let payload_hash = "b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9";
        let content_sha256 = "uU0nuZNNPgilLlLX2n2r+sSE7+N6U4DukIj3rOLvzek=";
        let amz_date = AmzDate::parse("20130524T000000Z").unwrap();
        let headers_for_signing = [
            ("host", "s3.amazonaws.com"),
            ("x-amz-content-sha256", content_sha256),
            ("x-amz-date", "20130524T000000Z"),
        ];
        let canonical_request = sig_v4::create_canonical_request(
            &method,
            path,
            &[] as &[(&str, &str)],
            headers_for_signing,
            sig_v4::Payload::SingleChunk(payload_hash),
        );
        let string_to_sign = sig_v4::create_string_to_sign(&canonical_request, &amz_date, "us-east-1", "s3");
        let signature = sig_v4::calculate_signature(&string_to_sign, &secret_key, &amz_date, "us-east-1", "s3");
        let authorization = format!(
            "AWS4-HMAC-SHA256 Credential={access_key}/20130524/us-east-1/s3/aws4_request, SignedHeaders=host;x-amz-content-sha256;x-amz-date, Signature={}",
            signature.as_str(),
        );

        let headers = headers_from_slice(&[
            ("authorization", authorization.as_str()),
            ("host", "s3.amazonaws.com"),
            ("x-amz-content-sha256", content_sha256),
            ("x-amz-date", "20130524T000000Z"),
        ]);
        let mut body = Body::from(Bytes::from_static(body_data));
        let mut cx = SignatureContext {
            auth: Some(&auth),
            config: &config,
            req_version: ::http::Version::HTTP_11,
            req_method: &method,
            req_uri: &uri,
            req_body: &mut body,
            qs: None,
            hs: &headers,
            decoded_uri_path: path,
            raw_uri_path: path,
            vh_bucket: None,
            content_length: Some(body_data.len() as u64),
            mime: None,
            decoded_content_length: None,
            transformed_body: None,
            multipart: None,
            trailing_headers: None,
        };
        let cred = cx
            .v4_check_header_auth()
            .await
            .expect("valid REST SigV4 base64 content checksum should succeed");
        assert_eq!(cred.access_key, access_key);
        let stored = cx
            .req_body
            .store_all_limited(100)
            .await
            .expect("valid REST payload checksum should remain readable");
        assert_eq!(stored, &body_data[..]);
    }

    #[tokio::test]
    async fn v4_header_auth_uses_http2_authority_for_signed_host() {
        use crate::auth::SecretKey;
        use crate::auth::SimpleAuth;
        use crate::config::{S3Config, S3ConfigProvider, StaticConfigProvider};
        use std::sync::Arc;

        let access_key = "AKIAIOSFODNN7EXAMPLE";
        let secret_key: SecretKey = "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY".into();
        let auth = SimpleAuth::from_single(access_key, secret_key.clone());
        let s3_config = S3Config {
            presigned_url_max_skew_time_secs: u32::MAX,
            ..Default::default()
        };
        let config: Arc<dyn S3ConfigProvider> = Arc::new(StaticConfigProvider::new(Arc::new(s3_config)));

        let method = Method::GET;
        let uri = Uri::from_static("https://s3.amazonaws.com/test-bucket/path/sitemap.xmlage=");
        let decoded_uri_path = "/test-bucket/path/sitemap.xmlage=";
        let raw_uri_path = "/test-bucket/path/sitemap.xmlage=";
        let amz_date = AmzDate::parse("20130524T000000Z").unwrap();
        let headers_for_signing = [
            ("host", "s3.amazonaws.com"),
            ("x-amz-content-sha256", "UNSIGNED-PAYLOAD"),
            ("x-amz-date", "20130524T000000Z"),
        ];
        let canonical_request = sig_v4::create_canonical_request(
            &method,
            decoded_uri_path,
            &[] as &[(&str, &str)],
            headers_for_signing,
            sig_v4::Payload::Unsigned,
        );
        let string_to_sign = sig_v4::create_string_to_sign(&canonical_request, &amz_date, "us-east-1", "s3");
        let signature = sig_v4::calculate_signature(&string_to_sign, &secret_key, &amz_date, "us-east-1", "s3");
        let authorization = format!(
            "AWS4-HMAC-SHA256 Credential={access_key}/20130524/us-east-1/s3/aws4_request, SignedHeaders=host;x-amz-content-sha256;x-amz-date, Signature={}",
            signature.as_str(),
        );
        let headers = headers_from_slice(&[
            ("authorization", authorization.as_str()),
            ("x-amz-content-sha256", "UNSIGNED-PAYLOAD"),
            ("x-amz-date", "20130524T000000Z"),
        ]);

        let mut body = Body::empty();
        let mut cx = SignatureContext {
            auth: Some(&auth),
            config: &config,
            req_version: ::http::Version::HTTP_2,
            req_method: &method,
            req_uri: &uri,
            req_body: &mut body,
            qs: None,
            hs: &headers,
            decoded_uri_path,
            raw_uri_path,
            vh_bucket: None,
            content_length: Some(0),
            mime: None,
            decoded_content_length: None,
            transformed_body: None,
            multipart: None,
            trailing_headers: None,
        };

        let cred = cx
            .v4_check_header_auth()
            .await
            .expect("HTTP/2 authority should be used for a signed host header");
        assert_eq!(cred.access_key, access_key);
    }

    #[allow(clippy::too_many_lines)]
    #[tokio::test]
    async fn v4_header_auth_raw_uri_path_signature_seeds_streaming_body() {
        use crate::auth::SecretKey;
        use crate::auth::SimpleAuth;
        use crate::config::{S3Config, S3ConfigProvider, StaticConfigProvider};
        use bytes::Bytes;
        use std::sync::Arc;

        let access_key = "AKIAIOSFODNN7EXAMPLE";
        let secret_key: SecretKey = "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY".into();
        let auth = SimpleAuth::from_single(access_key, secret_key.clone());
        let s3_config = S3Config {
            presigned_url_max_skew_time_secs: u32::MAX,
            ..Default::default()
        };
        let config: Arc<dyn S3ConfigProvider> = Arc::new(StaticConfigProvider::new(Arc::new(s3_config)));

        let method = Method::PUT;
        let uri = Uri::from_static("https://s3.amazonaws.com/test-bucket/path/sitemap.xmlage=");
        let decoded_uri_path = "/test-bucket/path/sitemap.xmlage=";
        let raw_uri_path = "/test-bucket/path/sitemap.xmlage=";
        let amz_date = AmzDate::parse("20130524T000000Z").unwrap();
        let chunk_data = Bytes::from_static(b"hello");
        let decoded_content_length = chunk_data.len();
        let headers_for_signing = [
            ("host", "s3.amazonaws.com"),
            ("x-amz-content-sha256", "STREAMING-AWS4-HMAC-SHA256-PAYLOAD"),
            ("x-amz-date", "20130524T000000Z"),
            ("x-amz-decoded-content-length", "5"),
        ];

        let standard_canonical_request = sig_v4::create_canonical_request(
            &method,
            decoded_uri_path,
            &[] as &[(&str, &str)],
            headers_for_signing,
            sig_v4::Payload::MultipleChunks,
        );
        let raw_canonical_request = sig_v4::create_canonical_request_with_raw_uri_path(
            &method,
            raw_uri_path,
            &[] as &[(&str, &str)],
            headers_for_signing,
            sig_v4::Payload::MultipleChunks,
        );
        assert_ne!(standard_canonical_request, raw_canonical_request);

        let seed_string_to_sign = sig_v4::create_string_to_sign(&raw_canonical_request, &amz_date, "us-east-1", "s3");
        let seed_signature = sig_v4::calculate_signature(&seed_string_to_sign, &secret_key, &amz_date, "us-east-1", "s3");

        let chunk_string_to_sign = sig_v4::create_chunk_string_to_sign(
            &amz_date,
            "us-east-1",
            "s3",
            seed_signature.as_str(),
            std::slice::from_ref(&chunk_data),
        );
        let chunk_signature = sig_v4::calculate_signature(&chunk_string_to_sign, &secret_key, &amz_date, "us-east-1", "s3");
        let final_string_to_sign =
            sig_v4::create_chunk_string_to_sign(&amz_date, "us-east-1", "s3", chunk_signature.as_str(), &[]);
        let final_signature = sig_v4::calculate_signature(&final_string_to_sign, &secret_key, &amz_date, "us-east-1", "s3");

        let mut streaming_body = Vec::new();
        streaming_body
            .extend_from_slice(format!("{:x};chunk-signature={}\r\n", chunk_data.len(), chunk_signature.as_str()).as_bytes());
        streaming_body.extend_from_slice(&chunk_data);
        streaming_body.extend_from_slice(b"\r\n");
        streaming_body.extend_from_slice(format!("0;chunk-signature={}\r\n\r\n", final_signature.as_str()).as_bytes());
        let content_length = u64::try_from(streaming_body.len()).unwrap();

        let authorization = format!(
            "AWS4-HMAC-SHA256 Credential={access_key}/20130524/us-east-1/s3/aws4_request, SignedHeaders=host;x-amz-content-sha256;x-amz-date;x-amz-decoded-content-length, Signature={}",
            seed_signature.as_str()
        );
        let headers = headers_from_slice(&[
            ("authorization", authorization.as_str()),
            ("host", "s3.amazonaws.com"),
            ("x-amz-content-sha256", "STREAMING-AWS4-HMAC-SHA256-PAYLOAD"),
            ("x-amz-date", "20130524T000000Z"),
            ("x-amz-decoded-content-length", "5"),
        ]);

        let mut body = Body::from(Bytes::from(streaming_body));
        let mut cx = SignatureContext {
            auth: Some(&auth),
            config: &config,
            req_version: ::http::Version::HTTP_11,
            req_method: &method,
            req_uri: &uri,
            req_body: &mut body,
            qs: None,
            hs: &headers,
            decoded_uri_path,
            raw_uri_path,
            vh_bucket: None,
            content_length: Some(content_length),
            mime: None,
            decoded_content_length: Some(decoded_content_length),
            transformed_body: None,
            multipart: None,
            trailing_headers: None,
        };

        let cred = cx
            .v4_check_header_auth()
            .await
            .expect("valid streaming SigV4 auth with a raw '=' URI path should succeed");
        assert_eq!(cred.access_key, access_key);

        let mut transformed_body = cx.transformed_body.take().expect("streaming body should be transformed");
        let decoded_body = transformed_body
            .store_all_limited(decoded_content_length)
            .await
            .expect("raw-path seed signature should validate aws-chunked body");
        assert_eq!(decoded_body, chunk_data);
    }

    /// `SigV2` does not carry region in the credential scope, so `CredentialsExt.region`
    /// must always be `None` and `service` must always be `Some("s3")`.
    ///
    /// Covers the documented `SigV2` behavior (`VirtualHost` region fallback relies on this).
    #[tokio::test]
    async fn v2_header_auth_returns_no_region() {
        use crate::auth::SecretKey;
        use crate::config::{S3Config, S3ConfigProvider, StaticConfigProvider};
        use std::sync::Arc;

        let access_key = "AKIAIOSFODNN7EXAMPLE";
        let secret_key: SecretKey = "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY".into();
        let auth = crate::auth::SimpleAuth::from_single(access_key, secret_key.clone());
        let config = S3Config {
            enable_sig_v2: true,
            ..Default::default()
        };
        let config: Arc<dyn S3ConfigProvider> = Arc::new(StaticConfigProvider::new(Arc::new(config)));

        let date = "Fri, 24 Jan 2030 12:00:00 +0000";
        let hs = headers_from_slice(&[("date", date), ("host", "s3.amazonaws.com")]);

        let method = Method::GET;
        let uri = Uri::from_static("https://s3.amazonaws.com/test-bucket/test-key");
        let mut body = Body::empty();

        // Compute the expected signature using the same logic as the verification path.
        let string_to_sign = crate::sig_v2::create_string_to_sign(
            crate::sig_v2::Mode::HeaderAuth,
            &method,
            "/test-bucket/test-key",
            None,
            &hs,
            None,
        )
        .unwrap();
        let signature = crate::sig_v2::calculate_signature(&secret_key, &string_to_sign);

        let auth_v2 = AuthorizationV2 {
            access_key,
            signature: signature.as_str(),
        };

        let mut cx = SignatureContext {
            auth: Some(&auth),
            config: &config,
            req_version: ::http::Version::HTTP_11,
            req_method: &method,
            req_uri: &uri,
            req_body: &mut body,
            qs: None,
            hs: &hs,
            decoded_uri_path: "/test-bucket/test-key",
            raw_uri_path: "/test-bucket/test-key",
            vh_bucket: None,
            content_length: None,
            mime: None,
            decoded_content_length: None,
            transformed_body: None,
            multipart: None,
            trailing_headers: None,
        };

        let cred = cx
            .v2_check_header_auth(auth_v2)
            .await
            .expect("valid SigV2 auth should succeed");
        assert_eq!(cred.region, None, "SigV2 carries no region");
        assert_eq!(cred.service.as_deref(), Some("s3"), "SigV2 service is always 's3'");
    }

    #[tokio::test]
    async fn v4_header_auth_rejects_stale_request_time() {
        use crate::S3ErrorCode;
        use crate::auth::SecretKey;
        use crate::auth::SimpleAuth;
        use crate::config::{S3ConfigProvider, StaticConfigProvider};
        use std::sync::Arc;

        let access_key = "AKIAIOSFODNN7EXAMPLE";
        let secret_key: SecretKey = "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY".into();
        let auth = SimpleAuth::from_single(access_key, secret_key.clone());
        let config: Arc<dyn S3ConfigProvider> = Arc::new(StaticConfigProvider::default());

        let skew = time::Duration::seconds(i64::from(config.snapshot().presigned_url_max_skew_time_secs));
        let request_time = time::OffsetDateTime::now_utc() - skew - time::Duration::minutes(1);
        let amz_date_str = fmt_current_amz_date(request_time);
        let amz_date = AmzDate::parse(&amz_date_str).unwrap();

        let method = Method::GET;
        let uri = Uri::from_static("https://s3.amazonaws.com/test.txt");
        let headers_for_signing = [
            ("host", "s3.amazonaws.com"),
            ("x-amz-content-sha256", "UNSIGNED-PAYLOAD"),
            ("x-amz-date", amz_date_str.as_str()),
        ];
        let canonical_request = sig_v4::create_canonical_request(
            &method,
            "/test.txt",
            &[] as &[(&str, &str)],
            headers_for_signing,
            sig_v4::Payload::Unsigned,
        );
        let string_to_sign = sig_v4::create_string_to_sign(&canonical_request, &amz_date, "us-east-1", "s3");
        let signature = sig_v4::calculate_signature(&string_to_sign, &secret_key, &amz_date, "us-east-1", "s3");
        let authorization = format!(
            "AWS4-HMAC-SHA256 Credential={access_key}/{}/us-east-1/s3/aws4_request, SignedHeaders=host;x-amz-content-sha256;x-amz-date, Signature={}",
            amz_date.fmt_date(),
            signature.as_str(),
        );

        let headers = headers_from_slice(&[
            ("authorization", authorization.as_str()),
            ("host", "s3.amazonaws.com"),
            ("x-amz-content-sha256", "UNSIGNED-PAYLOAD"),
            ("x-amz-date", amz_date_str.as_str()),
        ]);

        let mut body = Body::empty();
        let mut cx = SignatureContext {
            auth: Some(&auth),
            config: &config,
            req_version: ::http::Version::HTTP_11,
            req_method: &method,
            req_uri: &uri,
            req_body: &mut body,
            qs: None,
            hs: &headers,
            decoded_uri_path: "/test.txt",
            raw_uri_path: "/test.txt",
            vh_bucket: None,
            content_length: Some(0),
            mime: None,
            decoded_content_length: None,
            transformed_body: None,
            multipart: None,
            trailing_headers: None,
        };

        let err = cx
            .v4_check_header_auth()
            .await
            .expect_err("stale signed header request should be rejected");
        assert_eq!(err.code(), &S3ErrorCode::RequestTimeTooSkewed);
    }

    #[tokio::test]
    async fn v4_post_signature_rejects_stale_request_time() {
        use crate::S3ErrorCode;
        use crate::auth::SecretKey;
        use crate::auth::SimpleAuth;
        use crate::config::{S3ConfigProvider, StaticConfigProvider};
        use std::sync::Arc;

        let access_key = "AKIAIOSFODNN7EXAMPLE";
        let secret_key: SecretKey = "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY".into();
        let auth = SimpleAuth::from_single(access_key, secret_key.clone());
        let config: Arc<dyn S3ConfigProvider> = Arc::new(StaticConfigProvider::default());

        let skew = time::Duration::seconds(i64::from(config.snapshot().presigned_url_max_skew_time_secs));
        let request_time = time::OffsetDateTime::now_utc() - skew - time::Duration::minutes(1);
        let amz_date_str = fmt_current_amz_date(request_time);
        let amz_date = AmzDate::parse(&amz_date_str).unwrap();

        // Construct a proper POST policy JSON with the required eq conditions
        let policy_json = format!(
            r#"{{"expiration":"2099-01-01T00:00:00Z","conditions":[{{"x-amz-date":"{amz_date}"}},{{"x-amz-credential":"{access_key}/{date}/us-east-1/s3/aws4_request"}},{{"x-amz-algorithm":"AWS4-HMAC-SHA256"}}]}}"#,
            amz_date = amz_date_str,
            access_key = access_key,
            date = amz_date.fmt_date(),
        );
        let policy_b64 = base64_simd::STANDARD.encode_to_string(&policy_json);
        let signature = sig_v4::calculate_signature(&policy_b64, &secret_key, &amz_date, "us-east-1", "s3");
        let boundary = "boundary123";
        let body = format!(
            concat!(
                "\r\n--{boundary}\r\n",
                "Content-Disposition: form-data; name=\"x-amz-signature\"\r\n\r\n",
                "{signature}\r\n",
                "--{boundary}\r\n",
                "Content-Disposition: form-data; name=\"policy\"\r\n\r\n",
                "{policy_b64}\r\n",
                "--{boundary}\r\n",
                "Content-Disposition: form-data; name=\"x-amz-algorithm\"\r\n\r\n",
                "AWS4-HMAC-SHA256\r\n",
                "--{boundary}\r\n",
                "Content-Disposition: form-data; name=\"x-amz-credential\"\r\n\r\n",
                "{access_key}/{date}/us-east-1/s3/aws4_request\r\n",
                "--{boundary}\r\n",
                "Content-Disposition: form-data; name=\"x-amz-date\"\r\n\r\n",
                "{amz_date}\r\n",
                "--{boundary}\r\n",
                "Content-Disposition: form-data; name=\"file\"; filename=\"a.txt\"\r\n",
                "Content-Type: text/plain\r\n\r\n",
                "hello\r\n",
                "--{boundary}--\r\n"
            ),
            access_key = access_key,
            amz_date = amz_date_str,
            boundary = boundary,
            date = amz_date.fmt_date(),
            policy_b64 = policy_b64,
            signature = signature.as_str(),
        );

        let mime: Mime = format!("multipart/form-data; boundary={boundary}").parse().unwrap();
        let method = Method::POST;
        let uri = Uri::from_static("http://localhost/test-bucket");
        let headers = HeaderMap::new();
        let mut body = Body::from(body);
        let mut cx = SignatureContext {
            auth: Some(&auth),
            config: &config,
            req_version: ::http::Version::HTTP_11,
            req_method: &method,
            req_uri: &uri,
            req_body: &mut body,
            qs: None,
            hs: &headers,
            decoded_uri_path: "/test-bucket",
            raw_uri_path: "/test-bucket",
            vh_bucket: None,
            content_length: None,
            mime: Some(mime),
            decoded_content_length: None,
            transformed_body: None,
            multipart: None,
            trailing_headers: None,
        };

        let err = cx
            .check_post_signature()
            .await
            .expect_err("stale signed POST policy should be rejected");
        assert_eq!(err.code(), &S3ErrorCode::RequestTimeTooSkewed);
    }
}
