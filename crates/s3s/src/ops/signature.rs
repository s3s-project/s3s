use crate::auth::S3Auth;
use crate::auth::SecretKey;
use crate::config::S3ConfigProvider;
use crate::error::*;
use crate::http;
use crate::http::{AwsChunkedStream, Body, Multipart, MultipartLimits};
use crate::http::{OrderedHeaders, OrderedQs};
use crate::protocol::TrailingHeaders;
use crate::sig_v2;
use crate::sig_v2::{AuthorizationV2, PostSignatureV2, PresignedUrlV2};
use crate::sig_v4;
use crate::sig_v4::AmzContentSha256;
use crate::sig_v4::AmzDate;
use crate::sig_v4::UploadStream;
use crate::sig_v4::{AuthorizationV4, CredentialV4, PostSignatureV4, PresignedUrlV4};
use crate::stream::ByteStream as _;
use crate::utils::crypto::hex_sha256;
use crate::utils::is_base64_encoded;

use std::mem;
use std::ops::Not;
use std::sync::Arc;

use base64_simd::STANDARD;
use hyper::Method;
use hyper::Uri;
use mime::Mime;
use tracing::debug;

/// Maximum allowed size for STS request body (8KB should be enough for operations like `AssumeRole`)
const MAX_STS_BODY_SIZE: usize = 8192;

fn extract_amz_content_sha256<'a>(hs: &'_ OrderedHeaders<'a>) -> S3Result<Option<AmzContentSha256<'a>>> {
    let Some(val) = hs.get_unique(crate::header::X_AMZ_CONTENT_SHA256) else { return Ok(None) };
    match AmzContentSha256::parse(val) {
        Ok(x) => Ok(Some(x)),
        Err(e) => {
            // https://docs.aws.amazon.com/IAM/latest/UserGuide/reference_sigv-troubleshooting.html
            let mut err = S3Error::new(S3ErrorCode::SignatureDoesNotMatch);
            err.set_message("invalid header: x-amz-content-sha256");
            err.set_source(Box::new(e));
            Err(err)
        }
    }
}

fn extract_authorization_v4<'a>(hs: &'_ OrderedHeaders<'a>) -> S3Result<Option<AuthorizationV4<'a>>> {
    let Some(val) = hs.get_unique(crate::header::AUTHORIZATION) else { return Ok(None) };
    match AuthorizationV4::parse(val) {
        Ok(x) => Ok(Some(x)),
        Err(e) => Err(invalid_request!(e, "invalid header: authorization")),
    }
}

fn extract_amz_date(hs: &'_ OrderedHeaders<'_>) -> S3Result<Option<AmzDate>> {
    let Some(val) = hs.get_unique(crate::header::X_AMZ_DATE) else { return Ok(None) };
    match AmzDate::parse(val) {
        Ok(x) => Ok(Some(x)),
        Err(e) => Err(invalid_request!(e, "invalid header: x-amz-date")),
    }
}

pub struct SignatureContext<'a> {
    pub auth: Option<&'a dyn S3Auth>,
    pub config: &'a Arc<dyn S3ConfigProvider>,

    pub req_version: ::http::Version,
    pub req_method: &'a Method,
    pub req_uri: &'a Uri,
    pub req_body: &'a mut Body,

    pub qs: Option<&'a OrderedQs>,
    pub hs: OrderedHeaders<'a>,

    pub decoded_uri_path: String,
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

impl SignatureContext<'_> {
    pub async fn check(&mut self) -> S3Result<Option<CredentialsExt>> {
        if self.req_method == Method::POST
            && let Some(ref mime) = self.mime
            && mime.type_() == mime::MULTIPART
            && mime.subtype() == mime::FORM_DATA
        {
            return Ok(Some(self.check_post_signature().await?));
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
    async fn check_post_signature(&mut self) -> S3Result<CredentialsExt> {
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
            http::transform_multipart(body, boundary.as_str().as_bytes(), limits)
                .await
                .map_err(|e| s3_error!(e, MalformedPOSTRequest))?
        };

        debug!(?multipart);

        if multipart.find_field_value("x-amz-signature").is_some() {
            debug!("checking post signature v4");
            return self.v4_check_post_signature(multipart).await;
        }

        if multipart.find_field_value("signature").is_some() {
            debug!("checking post signature v2");
            return self.v2_check_post_signature(multipart).await;
        }

        Err(invalid_request!("unsupported post signature"))
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
        if self.hs.get_unique(crate::header::AUTHORIZATION).is_some() {
            debug!("checking header auth");
            return Some(self.v4_check_header_auth().await);
        }

        None
    }

    pub async fn v4_check_post_signature(&mut self, mut multipart: Multipart) -> S3Result<CredentialsExt> {
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

        let access_key = credential.access_key_id.to_owned();
        let secret_key = auth.get_secret_key(&access_key).await?;

        let region = credential.aws_region;
        let service = credential.aws_service;

        let string_to_sign = info.policy;
        let signature = sig_v4::calculate_signature(string_to_sign, &secret_key, &amz_date, region, service);

        let expected_signature = info.x_amz_signature;
        if signature != expected_signature {
            debug!(?signature, expected=?expected_signature, "signature mismatch");
            return Err(s3_error!(SignatureDoesNotMatch));
        }

        let policy_buf = STANDARD
            .decode_to_vec(info.policy)
            .map_err(|_| invalid_request!("invalid field: policy (base64 decode failed)"))?;

        let policy_json: serde_json::Value = serde_json::from_slice(&policy_buf)
            .map_err(|_| invalid_request!("invalid field: policy (json parse failed)"))?;

        if let Some(expiration_str) = policy_json.get("expiration").and_then(|v| v.as_str()) {
            let parse_format = time::format_description::well_known::Iso8601::DEFAULT;
            let expiration_dt = time::OffsetDateTime::parse(expiration_str, &parse_format)
                .map_err(|_| invalid_request!("invalid field: policy expiration format"))?;
            
            if time::OffsetDateTime::now_utc() > expiration_dt {
                return Err(s3_error!(AccessDenied, "request has expired"));
            }

        } else {
            return Err(invalid_request!("missing field: policy expiration"));
        }

        let mut content_length_min: Option<u64> = None;
        let mut content_length_max: Option<u64> = None;

        if let Some(conditions) = policy_json.get("conditions").and_then(|v| v.as_array()) {
            for condition in conditions {
                if let Some(map) = condition.as_object() {
                    for (k, v) in map {
                        let field_name = k.trim_start_matches('$');
                        let expected_val = v.as_str().unwrap_or("");
                        let actual_val = multipart.find_field_value(field_name).unwrap_or("");
                        
                        if actual_val != expected_val {
                            return Err(s3_error!(AccessDenied, "policy condition failed: {}", field_name));
                        }
                    }
                } else if let Some(arr) = condition.as_array() {
                    if arr.len() < 2 { continue; }
                    let operator = arr[0].as_str().unwrap_or("");
                    
                    match operator {
                        "eq" => {
                            let key = arr[1].as_str().unwrap_or("").trim_start_matches('$');
                            let val = arr[2].as_str().unwrap_or("");
                            
                            let actual_val = multipart.find_field_value(key).unwrap_or("");
                            if actual_val != val {
                                return Err(s3_error!(AccessDenied, "policy condition failed: {} must equal {}", key, val));
                            }
                        },
                        "starts-with" => {
                            let key = arr[1].as_str().unwrap_or("").trim_start_matches('$');
                            let prefix = arr[2].as_str().unwrap_or("");
                            let actual_val = multipart.find_field_value(key).unwrap_or("");
                            if !actual_val.starts_with(prefix) {
                                return Err(s3_error!(AccessDenied, "policy condition failed: {} must start with {}", key, prefix)); 
                            }
                        },
                        "content-length-range" => {
                            content_length_min = Some(arr[1].as_u64().unwrap_or(0));
                            content_length_max = Some(arr[2].as_u64().unwrap_or(u64::MAX));
                        },
                        _ => {}
                    }
                }
            }
        }

        let region = region.to_owned();
        let service = service.to_owned();

        if let (Some(min), Some(max)) = (content_length_min, content_length_max) {
            multipart.policy_limits = Some((min, max));
        }

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

        let presigned_url = PresignedUrlV4::parse(qs).map_err(|err| invalid_request!(err, "missing presigned url v4 fields"))?;

        if presigned_url.algorithm != "AWS4-HMAC-SHA256" {
            return Err(s3_error!(
                NotImplemented,
                "X-Amz-Algorithm other than AWS4-HMAC-SHA256 is not implemented"
            ));
        }

        // ASK: how to use it?
        let _content_sha256: Option<AmzContentSha256<'_>> = extract_amz_content_sha256(&self.hs)?;

        {
            // check expiration
            let now = time::OffsetDateTime::now_utc();

            let date = presigned_url
                .amz_date
                .to_time()
                .ok_or_else(|| invalid_request!("invalid amz date"))?;

            let duration = now - date;

            // Allow requests that are up to max_skew_time_secs in the future.
            // This is to account for clock skew between the client and server.
            // See also https://github.com/minio/minio/blob/b5177993b371817699d3fa25685f54f88d8bfcce/cmd/signature-v4.go#L238-L242

            let config = self.config.snapshot();
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

        let region = presigned_url.credential.aws_region;
        let service = presigned_url.credential.aws_service;

        let signature = {
            let headers = self.hs.find_multiple_with_on_missing(&presigned_url.signed_headers, |name| {
                // HTTP/2 replaces `host` header with `:authority`
                // but `:authority` is not in the request headers
                // so we need to add it back if `host` is in the signed headers
                if name == "host"
                    && matches!(self.req_version, ::http::Version::HTTP_2 | ::http::Version::HTTP_3)
                    && let Some(authority) = self.req_uri.authority()
                {
                    return Some(authority.as_str());
                }
                None
            });

            let method = &self.req_method;
            let uri_path = &self.decoded_uri_path;

            let canonical_request = sig_v4::create_presigned_canonical_request(method, uri_path, qs.as_ref(), &headers);

            let amz_date = &presigned_url.amz_date;
            let string_to_sign = sig_v4::create_string_to_sign(&canonical_request, amz_date, region, service);

            sig_v4::calculate_signature(&string_to_sign, &secret_key, amz_date, region, service)
        };

        let expected_signature = presigned_url.signature;
        if signature != expected_signature {
            debug!(?signature, expected=?expected_signature, "signature mismatch");
            return Err(s3_error!(SignatureDoesNotMatch));
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
            let mut a = extract_authorization_v4(&self.hs)?.unwrap();
            a.signed_headers.sort_unstable();
            a
        };
        let region = authorization.credential.aws_region;
        let service = authorization.credential.aws_service;

        if !matches!(service, "s3" | "sts") {
            return Err(s3_error!(NotImplemented, "unknown service"));
        }

        let auth = require_auth(self.auth)?;

        let amz_content_sha256 = extract_amz_content_sha256(&self.hs)?;

        if service == "s3" && amz_content_sha256.is_none() {
            return Err(invalid_request!("missing header: x-amz-content-sha256"));
        }

        let access_key = authorization.credential.access_key_id;
        let secret_key = auth.get_secret_key(access_key).await?;

        let amz_date = extract_amz_date(&self.hs)?.ok_or_else(|| invalid_request!("missing header: x-amz-date"))?;

        let is_stream = amz_content_sha256.is_some_and(|v| v.is_streaming());

        let signature = {
            let method = &self.req_method;
            let uri_path = &self.decoded_uri_path;
            let query_strings: &[(String, String)] = self.qs.as_ref().map_or(&[], AsRef::as_ref);

            // FIXME: throw error if any signed header is not in the request
            // `host` header need to be special handled

            // here requires that `auth.signed_headers` is sorted
            let headers = self.hs.find_multiple_with_on_missing(&authorization.signed_headers, |name| {
                // HTTP/2 replaces `host` header with `:authority`
                // but `:authority` is not in the request headers
                // so we need to add it back if `host` is in the signed headers
                if name == "host"
                    && self.req_version == ::http::Version::HTTP_2
                    && let Some(authority) = self.req_uri.authority()
                {
                    return Some(authority.as_str());
                }
                None
            });

            let canonical_request = match amz_content_sha256 {
                Some(AmzContentSha256::StreamingAws4HmacSha256Payload) => {
                    sig_v4::create_canonical_request(method, uri_path, query_strings, &headers, sig_v4::Payload::MultipleChunks)
                }
                Some(AmzContentSha256::StreamingAws4HmacSha256PayloadTrailer) => sig_v4::create_canonical_request(
                    method,
                    uri_path,
                    query_strings,
                    &headers,
                    sig_v4::Payload::MultipleChunksWithTrailer,
                ),
                Some(AmzContentSha256::UnsignedPayload) => {
                    sig_v4::create_canonical_request(method, uri_path, query_strings, &headers, sig_v4::Payload::Unsigned)
                }
                Some(AmzContentSha256::StreamingUnsignedPayloadTrailer) => sig_v4::create_canonical_request(
                    method,
                    uri_path,
                    query_strings,
                    &headers,
                    sig_v4::Payload::UnsignedMultipleChunksWithTrailer,
                ),
                Some(AmzContentSha256::SingleChunk(payload_checksum)) => sig_v4::create_canonical_request(
                    method,
                    uri_path,
                    query_strings,
                    &headers,
                    sig_v4::Payload::SingleChunk(payload_checksum),
                ),
                Some(
                    AmzContentSha256::StreamingAws4EcdsaP256Sha256Payload
                    | AmzContentSha256::StreamingAws4EcdsaP256Sha256PayloadTrailer,
                ) => {
                    return Err(s3_error!(NotImplemented, "AWS4-ECDSA-P256-SHA256 signing method is not implemented yet"));
                }
                None => {
                    // For STS requests, x-amz-content-sha256 header is not required
                    // For S3 requests, this case should have been caught earlier (see lines 325-327)
                    if service == "sts" {
                        // STS requests require computing the payload hash from the body
                        // Read the body (it's small for STS requests like AssumeRole)
                        let body_bytes = self
                            .req_body
                            .store_all_limited(MAX_STS_BODY_SIZE)
                            .await
                            .map_err(|e| invalid_request!("failed to read STS request body: {}", e))?;

                        // Compute SHA256 hash and convert to hex
                        let hash = hex_sha256(&body_bytes, str::to_owned);

                        // Create canonical request with the computed hash
                        sig_v4::create_canonical_request(
                            method,
                            uri_path,
                            query_strings,
                            &headers,
                            sig_v4::Payload::SingleChunk(&hash),
                        )
                    } else {
                        // According to AWS S3 protocol, x-amz-content-sha256 header is required for
                        // all S3 requests authenticated with Signature V4. Reject if missing.
                        return Err(invalid_request!("missing header: x-amz-content-sha256"));
                    }
                }
            };
            let string_to_sign = sig_v4::create_string_to_sign(&canonical_request, &amz_date, region, service);
            sig_v4::calculate_signature(&string_to_sign, &secret_key, &amz_date, region, service)
        };

        let expected_signature = authorization.signature;
        if signature != expected_signature {
            debug!(?signature, expected=?expected_signature, "signature mismatch");
            return Err(s3_error!(SignatureDoesNotMatch));
        }

        if is_stream {
            // For streaming with trailers, AWS requires x-amz-trailer header present.
            let has_trailer = amz_content_sha256.is_some_and(|v| v.has_trailer());
            if has_trailer && self.hs.get_unique("x-amz-trailer").is_none() {
                return Err(invalid_request!("missing header: x-amz-trailer"));
            }
            let decoded_content_length = self
                .decoded_content_length
                .ok_or_else(|| s3_error!(MissingContentLength, "missing header: x-amz-decoded-content-length"))?;

            let unsigned = matches!(amz_content_sha256, Some(AmzContentSha256::StreamingUnsignedPayloadTrailer));
            let stream = AwsChunkedStream::new(
                mem::take(self.req_body),
                signature.into(),
                amz_date,
                authorization.credential.aws_region.into(),
                authorization.credential.aws_service.into(),
                secret_key.clone(),
                decoded_content_length,
                unsigned,
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
            let stream = UploadStream::new(body, length, expected_checksum)
                .map_err(|_| invalid_request!("invalid header: x-amz-content-sha256"))?;
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
    pub async fn v2_check(&mut self) -> Option<S3Result<CredentialsExt>> {
        // query auth
        if let Some(qs) = self.qs
            && qs.has("Signature")
        {
            debug!("checking presigned url");
            return Some(self.v2_check_presigned_url().await);
        }

        // header auth
        if let Some(auth) = self.hs.get_unique(crate::header::AUTHORIZATION)
            && let Ok(auth) = AuthorizationV2::parse(auth)
        {
            debug!("checking header auth");
            return Some(self.v2_check_header_auth(auth).await);
        }

        None
    }

    pub async fn v2_check_header_auth(&mut self, auth_v2: AuthorizationV2<'_>) -> S3Result<CredentialsExt> {
        let method = &self.req_method;

        let date = self.hs.get_unique("date").or_else(|| self.hs.get_unique("x-amz-date"));
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
            &self.hs,
            self.vh_bucket,
        );
        let signature = sig_v2::calculate_signature(&secret_key, &string_to_sign);

        debug!(?string_to_sign, "sig_v2 header_auth");

        let expected_signature = auth_v2.signature;
        if signature != expected_signature {
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

        let expected_signature = info.signature;
        if signature != expected_signature {
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
            &self.hs,
            self.vh_bucket,
        );
        let signature = sig_v2::calculate_signature(&secret_key, &string_to_sign);

        let expected_signature = presigned_url.signature;
        if signature != expected_signature {
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

    #[test]
    fn test_extract_amz_content_sha256_missing() {
        // Test that extract_amz_content_sha256 returns None when header is missing
        let headers =
            OrderedHeaders::from_slice_unchecked(&[("host", "example.s3.amazonaws.com"), ("x-amz-date", "20130524T000000Z")]);
        let result = extract_amz_content_sha256(&headers).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn test_extract_amz_content_sha256_present() {
        // Test that extract_amz_content_sha256 returns Some when header is present
        let headers = OrderedHeaders::from_slice_unchecked(&[
            ("host", "example.s3.amazonaws.com"),
            ("x-amz-content-sha256", "UNSIGNED-PAYLOAD"),
            ("x-amz-date", "20130524T000000Z"),
        ]);
        let result = extract_amz_content_sha256(&headers).unwrap();
        assert!(result.is_some());
        assert!(matches!(result.unwrap(), AmzContentSha256::UnsignedPayload));
    }

    #[test]
    fn test_extract_amz_content_sha256_invalid() {
        // Test that extract_amz_content_sha256 returns error for invalid header value
        let headers = OrderedHeaders::from_slice_unchecked(&[
            ("host", "example.s3.amazonaws.com"),
            ("x-amz-content-sha256", "INVALID-VALUE"),
            ("x-amz-date", "20130524T000000Z"),
        ]);
        let result = extract_amz_content_sha256(&headers);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.message().unwrap().contains("x-amz-content-sha256"));
    }

    #[tokio::test]
    async fn test_sts_body_hash_computation() {
        // Test that STS request body hash is computed correctly
        use crate::utils::crypto::hex_sha256;

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
}
