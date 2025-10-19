use crate::auth::S3Auth;
use crate::auth::SecretKey;
use crate::error::*;
use crate::http;
use crate::http::{AwsChunkedStream, Body, Multipart};
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
use crate::utils::crypto::hex_sha256_string;
use crate::utils::is_base64_encoded;

use std::mem;
use std::ops::Not;

use bytestring::ByteString;
use hyper::Method;
use hyper::Uri;
use mime::Mime;
use tracing::debug;

fn extract_amz_content_sha256<'a>(hs: &'_ OrderedHeaders<'a>) -> S3Result<Option<AmzContentSha256<'a>>> {
    let Some(val) = hs.get_unique(crate::header::X_AMZ_CONTENT_SHA256) else { return Ok(None) };
    match AmzContentSha256::parse(val) {
        Ok(x) => Ok(Some(x)),
        Err(e) => {
            let mut err: S3Error = S3ErrorCode::Custom(ByteString::from_static("XAmzContentSHA256Mismatch")).into();
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
        if self.req_method == Method::POST {
            if let Some(ref mime) = self.mime {
                if mime.type_() == mime::MULTIPART && mime.subtype() == mime::FORM_DATA {
                    return Ok(Some(self.check_post_signature().await?));
                }
            }
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
            let mime = self.mime.as_ref().unwrap(); // assume: multipart

            let boundary = mime
                .get_param(mime::BOUNDARY)
                .ok_or_else(|| invalid_request!("missing boundary"))?;

            let body = mem::take(self.req_body);
            http::transform_multipart(body, boundary.as_str().as_bytes())
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
        if let Some(qs) = self.qs {
            if qs.has("X-Amz-Signature") {
                debug!("checking presigned url");
                return Some(self.v4_check_presigned_url().await);
            }
        }

        // header auth
        if self.hs.get_unique(crate::header::AUTHORIZATION).is_some() {
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

            // Allow requests that are up to 15 minutes in the future.
            // This is to account for clock skew between the client and server.
            // See also https://github.com/minio/minio/blob/b5177993b371817699d3fa25685f54f88d8bfcce/cmd/signature-v4.go#L238-L242

            // TODO: configurable max_skew_time

            let max_skew_time = time::Duration::seconds(15 * 60);
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
                if name == "host" && self.req_version == ::http::Version::HTTP_2 {
                    if let Some(authority) = self.req_uri.authority() {
                        return Some(authority.as_str());
                    }
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
                if name == "host" && self.req_version == ::http::Version::HTTP_2 {
                    if let Some(authority) = self.req_uri.authority() {
                        return Some(authority.as_str());
                    }
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
                    if matches!(*self.req_method, Method::GET | Method::HEAD) {
                        sig_v4::create_canonical_request(method, uri_path, query_strings, &headers, sig_v4::Payload::Empty)
                    } else {
                        let bytes = super::extract_full_body(self.content_length, self.req_body).await?;
                        if bytes.is_empty() {
                            sig_v4::create_canonical_request(method, uri_path, query_strings, &headers, sig_v4::Payload::Empty)
                        } else {
                            let payload_checksum = hex_sha256_string(&bytes);
                            sig_v4::create_canonical_request(
                                method,
                                uri_path,
                                query_strings,
                                &headers,
                                sig_v4::Payload::SingleChunk(&payload_checksum),
                            )
                        }
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
        if let Some(qs) = self.qs {
            if qs.has("Signature") {
                debug!("checking presigned url");
                return Some(self.v2_check_presigned_url().await);
            }
        }

        // header auth
        if let Some(auth) = self.hs.get_unique(crate::header::AUTHORIZATION) {
            if let Ok(auth) = AuthorizationV2::parse(auth) {
                debug!("checking header auth");
                return Some(self.v2_check_header_auth(auth).await);
            }
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
