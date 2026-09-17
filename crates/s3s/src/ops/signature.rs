// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

#![deny(
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::unreachable,
    clippy::unwrap_used
)]
use crate::auth::S3Auth;
use crate::auth::SecretKey;
use crate::auth::signature::Signature;
use crate::config::{S3Config, S3ConfigProvider};
use crate::error::*;
use crate::header::X_AMZ_CONTENT_SHA256;
use crate::http::{self, OrderedQs};
use crate::http::{Body, Multipart, MultipartLimits};
use crate::post_policy::PostPolicy;
use crate::protocol::TrailingHeaders;
use crate::stream::ByteStream as _;
use crate::stream::aws_chunked_stream::AwsChunkedStream;
use crate::stream::upload_stream::UploadStream;
use crate::utils::crypto::Sha256Sum;
use crate::utils::crypto::hex_bytes32;
use crate::utils::crypto::hex_sha256;
use crate::utils::is_base64_encoded;
use s3s_sigv2::AuthorizationV2;
use s3s_sigv2::PostSignatureV2;
use s3s_sigv2::PresignedUrlV2;
use s3s_sigv4::AmzContentSha256;
use s3s_sigv4::AmzDate;
use s3s_sigv4::PostSignatureV4;
use s3s_sigv4::PresignedUrlV4;
use s3s_sigv4::{AuthorizationV4, CredentialV4, ParseAuthorizationError};

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
pub(super) const MAX_STS_BODY_SIZE: usize = 8192;

type SignedHeaderPairs<'a> = SmallVec<[(&'a str, &'a str); 16]>;

pub(super) fn extract_amz_content_sha256(hs: &HeaderMap) -> S3Result<Option<AmzContentSha256>> {
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

pub(super) fn extract_authorization_v4(hs: &HeaderMap) -> S3Result<Option<AuthorizationV4<'_>>> {
    let Some(val) = http::get_unique_header_str(hs, crate::header::AUTHORIZATION.as_str()) else {
        return Ok(None);
    };
    match AuthorizationV4::parse(val) {
        Ok(x) => Ok(Some(x)),
        // A structurally valid header with a non-canonical signature is a
        // signature problem, not a malformed header: keep the `SignatureDoesNotMatch`
        // error that the signature-comparison step used to produce.
        Err(ParseAuthorizationError::InvalidSignature) => {
            Err(s3_error!(SignatureDoesNotMatch, "invalid header: authorization: invalid signature"))
        }
        Err(e) => Err(invalid_request!(e, "invalid header: authorization")),
    }
}

/// Rejects `x-amz-*` request headers that are present but absent from `SignedHeaders` /
/// `X-Amz-SignedHeaders`.
///
/// `collect_signed_headers` builds the canonical request from the header names the client declared,
/// so a header outside that list is never verified. Routing and input parsing still read the
/// arriving headers, which lets the holder of a signed request (for example a presigned `PUT` URL
/// handed to an untrusted uploader) change what the request does: adding `x-amz-copy-source` turns
/// the upload into a copy.
///
/// The signed-header list is client-supplied and not lowercased, so comparisons are
/// case-insensitive; [`HeaderName`] is already lowercase. Headers listed in
/// [`S3Config::unsigned_amz_header_allowlist`] are exempt.
fn reject_unsigned_amz_headers(config: &S3Config, hs: &HeaderMap, signed_names: &[&str]) -> S3Result<()> {
    // S3 treats x-amz-content-sha256 as the request's payload-hash input rather than ordinary request metadata.
    // Every other exception belongs in the configurable `S3Config::unsigned_amz_header_allowlist`.
    for name in hs.keys() {
        let name = name.as_str();
        if !name.starts_with("x-amz-")
            || name == X_AMZ_CONTENT_SHA256.as_str()
            || config.unsigned_amz_header_allowlist.iter().any(|allow| allow == name)
        {
            continue;
        }
        if !signed_names.iter().any(|signed| signed.eq_ignore_ascii_case(name)) {
            return Err(s3_error!(AccessDenied, "There were headers present in the request which were not signed"));
        }
    }
    Ok(())
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

pub(super) fn collect_signed_headers<'a>(
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

/// Collects header pairs for `s3s_sigv2::create_string_to_sign`.
///
/// `&str` cannot represent non-UTF-8 values, so the rejection that used to
/// happen inside `SigV2` canonicalization is handled here: a non-UTF-8
/// `x-amz-*` header value fails the signature check with the same message;
/// non-UTF-8 values of other headers are skipped, as they are not signed.
pub(super) fn sig_v2_headers(hs: &HeaderMap) -> S3Result<SignedHeaderPairs<'_>> {
    let mut headers = SignedHeaderPairs::new();

    for (name, value) in hs {
        let name = name.as_str();
        let Some(value) = http::header_value_to_str(value) else {
            if name.starts_with("x-amz-") {
                return Err(s3_error!(SignatureDoesNotMatch, "invalid header: {name}"));
            }
            continue;
        };
        headers.push((name, value));
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

pub(super) struct SignatureVerificationContext<'a> {
    expected_signature: Signature,
    raw_uri_path: &'a str,
    secret_key: &'a SecretKey,
    amz_date: &'a AmzDate,
    region: &'a str,
    service: &'a str,
}

#[cfg(test)]
impl<'a> SignatureVerificationContext<'a> {
    /// Test-only constructor: the fields stay private and the unit tests in
    /// `ops/tests/signature_*.rs` build the context through this.
    pub(super) fn new(
        expected_signature: Signature,
        raw_uri_path: &'a str,
        secret_key: &'a SecretKey,
        amz_date: &'a AmzDate,
        region: &'a str,
        service: &'a str,
    ) -> Self {
        Self {
            expected_signature,
            raw_uri_path,
            secret_key,
            amz_date,
            region,
            service,
        }
    }
}

fn validate_sig_v4_clock_skew(amz_date: &AmzDate, now: jiff::Timestamp, config: &S3Config) -> S3Result<()> {
    let request_time = amz_date.to_time().ok_or_else(|| invalid_request!("invalid amz date"))?;
    validate_clock_skew(request_time, now, config)
}

/// Rejects requests whose `date` / `x-amz-date` is farther from the server
/// time than the configured skew allowance. Shared by `SigV4` and `SigV2`
/// header authentication.
fn validate_clock_skew(request_time: jiff::Timestamp, now: jiff::Timestamp, config: &S3Config) -> S3Result<()> {
    let duration = (now - request_time)
        .to_duration(jiff::SpanRelativeTo::days_are_24_hours())
        .map_err(|_| invalid_request!("invalid request time"))?;
    let max_skew_time = jiff::SignedDuration::from_secs(i64::from(config.presigned_url_max_skew_time_secs));

    if duration.abs() > max_skew_time {
        return Err(s3_error!(RequestTimeTooSkewed, "request time is too far from server time"));
    }

    Ok(())
}

pub(super) fn validate_sig_v4_region(region: &str, config: &S3Config) -> S3Result<()> {
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

pub(super) fn validate_sig_v4_service(service: &str, config: &S3Config) -> S3Result<()> {
    if !config
        .sig_v4_allowed_services
        .iter()
        .any(|allowed| allowed.as_str() == service)
    {
        return Err(s3_error!(
            NotImplemented,
            "unknown service '{}' in credential scope; expected one of: {}",
            service,
            config.sig_v4_allowed_services.join(", "),
        ));
    }

    Ok(())
}

impl SignatureVerificationContext<'_> {
    pub(super) fn verify_with_raw_path_fallback(
        &self,
        canonical_request: &str,
        raw_canonical_request: impl FnOnce() -> String,
    ) -> S3Result<Signature> {
        let string_to_sign = s3s_sigv4::create_string_to_sign(canonical_request, self.amz_date, self.region, self.service);
        let signature = Signature::from_computed(s3s_sigv4::calculate_signature(
            &string_to_sign,
            self.secret_key.expose(),
            self.amz_date,
            self.region,
            self.service,
        ));

        if Signature::compare(&signature, &self.expected_signature) {
            return Ok(signature);
        }

        if !has_unencoded_reserved_path_char(self.raw_uri_path) {
            debug!(?signature, expected=?self.expected_signature, "signature mismatch");
            return Err(s3_error!(SignatureDoesNotMatch));
        }

        let canonical_request = raw_canonical_request();
        let string_to_sign = s3s_sigv4::create_string_to_sign(&canonical_request, self.amz_date, self.region, self.service);
        let raw_signature = Signature::from_computed(s3s_sigv4::calculate_signature(
            &string_to_sign,
            self.secret_key.expose(),
            self.amz_date,
            self.region,
            self.service,
        ));

        if !Signature::compare(&raw_signature, &self.expected_signature) {
            debug!(?signature, ?raw_signature, expected=?self.expected_signature, "signature mismatch");
            return Err(s3_error!(SignatureDoesNotMatch));
        }

        Ok(raw_signature)
    }
}

impl<'a> SignatureContext<'a> {
    fn query_pairs(&self) -> &'a [(String, String)] {
        // `Option<&'a OrderedQs>` is Copy: extract the reference without
        // borrowing `self`, so the returned slice lives as long as `'a`.
        let qs: Option<&'a OrderedQs> = self.qs;
        qs.map_or(&[], AsRef::as_ref)
    }

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
    /// `SigV2` is disabled by default for security. When disabled, requests that are
    /// successfully recognized as `SigV2` are rejected with `AccessDenied` (fail-closed)
    /// rather than being treated as anonymous.
    fn ensure_v2_enabled(&self) -> S3Result<()> {
        let config = self.config.snapshot();
        if !config.enable_sig_v2 {
            return Err(s3_error!(AccessDenied, "Signature Version 2 is disabled by server configuration"));
        }
        Ok(())
    }

    /// Rejects presigned URL requests when `allow_presigned_url` is off.
    ///
    /// A presigned request is recognized by its query signature — `X-Amz-Signature`
    /// (`SigV4`) or `Signature` (`SigV2`). When disabled, such a request is rejected with
    /// `AccessDenied` (fail-closed) rather than being treated as anonymous or retried as
    /// header authentication.
    fn ensure_presigned_url_enabled(&self) -> S3Result<()> {
        let config = self.config.snapshot();
        if !config.allow_presigned_url {
            return Err(s3_error!(
                AccessDenied,
                "Presigned URL authentication is disabled by server configuration"
            ));
        }
        Ok(())
    }

    /// Rejects `POST` form signature requests when `allow_post_signature` is off.
    ///
    /// Only signed forms are affected: a form without a signature is an anonymous request that the
    /// configured access policy decides on, exactly as before.
    fn ensure_post_signature_enabled(&self) -> S3Result<()> {
        let config = self.config.snapshot();
        if !config.allow_post_signature {
            return Err(s3_error!(
                AccessDenied,
                "POST signature authentication is disabled by server configuration"
            ));
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
    pub(super) async fn check_post_signature(&mut self) -> S3Result<Option<CredentialsExt>> {
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
            // The parser carries its state across the awaits of the POST form
            // parse. The state is boxed inside `FileStream`; boxing the future
            // as well keeps the remaining parse frame out of the dispatch
            // futures that await this check (see the future-size budget test).
            Box::pin(http::transform_multipart(body, boundary.as_str().as_bytes(), limits, self.content_length))
                .await
                .map_err(|e| s3_error!(e, MalformedPOSTRequest))?
        };

        debug!(?multipart);

        // Only signed forms are gated. A form without a signature stays an anonymous request, so
        // the switch is applied here — after the form is parsed, and only for the two signed
        // variants — rather than at the `POST` + `multipart/form-data` dispatch, which would also
        // reject anonymous forms.
        if multipart.find_field_value("x-amz-signature").is_some() {
            self.ensure_post_signature_enabled()?;
            debug!("checking post signature v4");
            return Ok(Some(self.v4_check_post_signature(multipart).await?));
        }

        if multipart.find_field_value("signature").is_some() {
            self.ensure_post_signature_enabled()?;
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
            if let Err(error) = self.ensure_presigned_url_enabled() {
                return Some(Err(error));
            }
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

        let info =
            PostSignatureV4::extract(multipart.fields()).map_err(|e| invalid_request!(e, "invalid multipart fields: {e}"))?;

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
        let config = self.config.snapshot();

        validate_sig_v4_region(region, &config)?;
        validate_sig_v4_clock_skew(&amz_date, jiff::Timestamp::now(), &config)?;

        let access_key = credential.access_key_id.to_owned();
        let secret_key = auth.get_secret_key(&access_key).await?;

        let service = credential.aws_service;

        validate_sig_v4_service(service, &config)?;

        let string_to_sign = info.policy;
        let signature = Signature::from_computed(s3s_sigv4::calculate_signature(
            string_to_sign,
            secret_key.expose(),
            &amz_date,
            region,
            service,
        ));

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
        let config = self.config.snapshot();

        let presigned_url = PresignedUrlV4::parse(self.query_pairs(), config.presigned_url_max_expires_secs).map_err(|err| {
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

            let now = jiff::Timestamp::now();

            let date = presigned_url
                .amz_date
                .to_time()
                .ok_or_else(|| invalid_request!("invalid amz date"))?;

            let duration = (now - date)
                .to_duration(jiff::SpanRelativeTo::days_are_24_hours())
                .map_err(|_| invalid_request!("invalid amz date"))?;

            // Allow requests that are up to max_skew_time_secs in the future.
            // This is to account for clock skew between the client and server.
            // See also https://github.com/minio/minio/blob/b5177993b371817699d3fa25685f54f88d8bfcce/cmd/signature-v4.go#L238-L242

            let max_skew_time = jiff::SignedDuration::from_secs(i64::from(config.presigned_url_max_skew_time_secs));
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

        validate_sig_v4_service(service, &config)?;

        let expected_signature = Signature::from_hex(presigned_url.signature).ok_or_else(|| s3_error!(SignatureDoesNotMatch))?;
        let headers = collect_signed_headers(self.hs, &presigned_url.signed_headers, |name| self.signed_host_fallback(name))?;
        reject_unsigned_amz_headers(&config, self.hs, &presigned_url.signed_headers)?;

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
        let canonical_request =
            s3s_sigv4::create_presigned_canonical_request(method.as_str(), self.decoded_uri_path, self.query_pairs(), &headers);
        verifier.verify_with_raw_path_fallback(&canonical_request, || {
            s3s_sigv4::create_presigned_canonical_request_with_raw_uri_path(
                method.as_str(),
                self.raw_uri_path,
                self.query_pairs(),
                &headers,
            )
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

            let stream = UploadStream::new(mem::take(self.req_body), length, Sha256Sum::from_bytes(expected_checksum));
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
        let authorization: AuthorizationV4<'_> =
            extract_authorization_v4(self.hs)?.ok_or_else(|| s3_error!(MissingSecurityHeader))?;

        // The parser accepts any algorithm token, but the string to sign is
        // always built as AWS4-HMAC-SHA256. Reject other tokens, as the
        // presigned and POST paths already do, so a request is never verified
        // under an algorithm it did not claim.
        if authorization.algorithm != "AWS4-HMAC-SHA256" {
            return Err(s3_error!(
                NotImplemented,
                "Authorization algorithm other than AWS4-HMAC-SHA256 is not implemented"
            ));
        }

        let region = authorization.credential.aws_region;
        let service = authorization.credential.aws_service;
        let config = self.config.snapshot();

        validate_sig_v4_service(service, &config)?;

        let auth = require_auth(self.auth)?;

        // Reject stale requests before doing I/O work (secret key lookup).
        let amz_date = extract_amz_date(self.hs)?.ok_or_else(|| invalid_request!("missing header: x-amz-date"))?;

        // Per AWS SigV4 spec, the credential scope date must match the x-amz-date date.
        if authorization.credential.date != amz_date.fmt_date().as_str() {
            return Err(s3_error!(SignatureDoesNotMatch, "credential scope date does not match x-amz-date"));
        }

        validate_sig_v4_region(region, &config)?;
        validate_sig_v4_clock_skew(&amz_date, jiff::Timestamp::now(), &config)?;

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
            Some(AmzContentSha256::StreamingAws4HmacSha256Payload) => s3s_sigv4::Payload::MultipleChunks,
            Some(AmzContentSha256::StreamingAws4HmacSha256PayloadTrailer) => s3s_sigv4::Payload::MultipleChunksWithTrailer,
            Some(AmzContentSha256::UnsignedPayload) => s3s_sigv4::Payload::Unsigned,
            Some(AmzContentSha256::StreamingUnsignedPayloadTrailer) => s3s_sigv4::Payload::UnsignedMultipleChunksWithTrailer,
            Some(AmzContentSha256::SingleChunk(checksum)) => {
                payload_hash = hex_bytes32(&checksum, str::to_owned);
                s3s_sigv4::Payload::SingleChunk(&payload_hash)
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
                    s3s_sigv4::Payload::SingleChunk(&payload_hash)
                } else {
                    // According to AWS S3 protocol, x-amz-content-sha256 header is required for
                    // all S3 requests authenticated with Signature V4. Reject if missing.
                    return Err(invalid_request!("missing header: x-amz-content-sha256"));
                }
            }
        };

        let headers = collect_signed_headers(self.hs, &authorization.signed_headers, |name| self.signed_host_fallback(name))?;
        reject_unsigned_amz_headers(&config, self.hs, &authorization.signed_headers)?;

        let verifier = SignatureVerificationContext {
            expected_signature,
            raw_uri_path: self.raw_uri_path,
            secret_key: &secret_key,
            amz_date: &amz_date,
            region,
            service,
        };
        let canonical_request =
            s3s_sigv4::create_canonical_request(method.as_str(), self.decoded_uri_path, query_strings, &headers, payload);
        let signature = verifier.verify_with_raw_path_fallback(&canonical_request, || {
            s3s_sigv4::create_canonical_request_with_raw_uri_path(
                method.as_str(),
                self.raw_uri_path,
                query_strings,
                &headers,
                payload,
            )
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
            let stream = UploadStream::new(body, length, Sha256Sum::from_bytes(expected_checksum));
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
            if let Err(error) = self.ensure_presigned_url_enabled() {
                return Some(Err(error));
            }
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
        let Some(date) = date else {
            return Err(invalid_request!("missing date"));
        };

        // Reject stale requests before doing any signing work: a repeated
        // `x-amz-date` carries the authoritative timestamp (when present, the
        // `Date` header is not signed), otherwise the `Date` header must be a
        // fresh RFC 1123 date. Without this check, captured `SigV2` requests
        // could be replayed indefinitely.
        let config = self.config.snapshot();
        let request_time = if let Some(x) = http::get_unique_header_str(self.hs, "x-amz-date") {
            AmzDate::parse(x)
                .map_err(|_| invalid_request!("invalid x-amz-date"))?
                .to_time()
                .ok_or_else(|| invalid_request!("invalid x-amz-date"))?
        } else {
            let ts = crate::dto::Timestamp::parse(crate::dto::TimestampFormat::HttpDate, date)
                .map_err(|_| invalid_request!("invalid date"))?;
            let odt: time::OffsetDateTime = ts.into();
            jiff::Timestamp::from_second(odt.unix_timestamp()).map_err(|_| invalid_request!("invalid date"))?
        };
        validate_clock_skew(request_time, jiff::Timestamp::now(), &config)?;

        let auth = require_auth(self.auth)?;
        let access_key = auth_v2.access_key;
        let secret_key = auth.get_secret_key(access_key).await?;

        let string_to_sign = s3s_sigv2::create_string_to_sign(
            s3s_sigv2::Mode::HeaderAuth,
            method.as_str(),
            self.req_uri.path(),
            self.qs.map(AsRef::as_ref),
            &sig_v2_headers(self.hs)?,
            self.vh_bucket,
        );
        let signature = Signature::from_computed(s3s_sigv2::calculate_signature(secret_key.expose(), &string_to_sign));

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

        let info =
            PostSignatureV2::extract(multipart.fields()).map_err(|e| invalid_request!(e, "invalid multipart fields: {e}"))?;

        if is_base64_encoded(info.policy.as_bytes()).not() {
            return Err(invalid_request!("invalid field: policy"));
        }

        let access_key = info.access_key_id.to_owned();
        let secret_key = auth.get_secret_key(&access_key).await?;

        // For v2 POST signature, the string to sign is the base64-encoded policy
        let string_to_sign = info.policy;
        let signature = Signature::from_computed(s3s_sigv2::calculate_signature(secret_key.expose(), string_to_sign));

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

        let presigned_url =
            PresignedUrlV2::parse(self.query_pairs()).map_err(|err| invalid_request!(err, "missing presigned url v2 fields"))?;

        if jiff::Timestamp::now() > presigned_url.expires_time {
            return Err(s3_error!(AccessDenied, "Request has expired"));
        }

        let auth = require_auth(self.auth)?;
        let access_key = presigned_url.access_key;
        let secret_key = auth.get_secret_key(access_key).await?;

        let string_to_sign = s3s_sigv2::create_string_to_sign(
            s3s_sigv2::Mode::PresignedUrl,
            self.req_method.as_str(),
            self.req_uri.path(),
            self.qs.map(AsRef::as_ref),
            &sig_v2_headers(self.hs)?,
            self.vh_bucket,
        );
        let signature = Signature::from_computed(s3s_sigv2::calculate_signature(secret_key.expose(), &string_to_sign));

        let expected_signature =
            Signature::from_base64(presigned_url.signature).ok_or_else(|| s3_error!(SignatureDoesNotMatch))?;
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
