// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

//! Internal S3 operation dispatch, HTTP serialization, and deserialization.
//!
//! This module converts incoming HTTP requests into typed operation inputs,
//! invokes the user-provided [`S3`](crate::S3) implementation, and converts
//! the resulting outputs or errors back into HTTP responses.

mod generated;

pub use self::generated::*;

mod signature;
use self::signature::{CredentialsExt, SignatureContext};

mod get_object;
mod multipart;
mod payload_length;

#[cfg(test)]
mod tests;

#[cfg(test)]
mod benches;

use crate::access::{S3Access, S3AccessContext};
use crate::auth::{Credentials, S3Auth};
use crate::config::{S3Config, S3ConfigProvider};
use crate::error::*;
use crate::header;
use crate::host::{S3Host, VirtualHost};
use crate::http::Body;
use crate::http::OrderedQs;
use crate::http::QsLookup;
use crate::http::{self, BodySizeLimitExceeded};
use crate::http::{Request, Response};
use crate::path::{ParseS3PathError, S3Path};
use crate::post_policy::PostPolicy;
use crate::protocol::S3Request;
use crate::route::S3Route;
use crate::s3_trait::S3;
use crate::stream::ByteStream as _;
use crate::stream::aws_chunked_stream::AwsChunkedStream;
use crate::validation::{AwsNameValidation, NameValidation};

use std::mem;
use std::net::{IpAddr, SocketAddr};
use std::ops::Not;
use std::sync::Arc;

use bytes::Bytes;
use hyper::HeaderMap;
use hyper::Method;
use hyper::StatusCode;
use hyper::Uri;
use mime::Mime;
use tracing::{debug, error, warn};

#[async_trait::async_trait]
pub trait Operation: Send + Sync + 'static {
    fn name(&self) -> &'static str;

    /// Whether this operation requires the request body to be fully read
    /// before being dispatched to the user-implemented [`S3`] handler.
    ///
    /// `true` for XML-payload operations (e.g. `DeleteObjects`,
    /// `CompleteMultipartUpload`) and PUT configuration operations.
    fn needs_full_body(&self) -> bool;

    /// Whether this operation consumes a request payload.
    ///
    /// `true` for XML-payload, streaming, and policy operations (e.g.
    /// `DeleteObjects`, `PutObject`, `PutBucketPolicy`), `false` for
    /// bodyless operations such as `GetObject` and `DeleteObject`.
    fn has_request_payload(&self) -> bool;

    /// Whether this operation streams the request body directly to the
    /// user-implemented [`S3`] handler without buffering.
    ///
    /// `true` for operations whose input carries a `StreamingBlob` payload
    /// (e.g. `PutObject`, `UploadPart`). These bodies are not bounded by
    /// [`S3Config::xml_max_body_size`]; see
    /// [`S3Config::put_object_max_size`].
    fn has_streaming_body(&self) -> bool;

    async fn call(&self, ccx: &CallContext<'_>, req: &mut Request) -> S3Result<Response>;
}

pub struct CallContext<'a> {
    pub s3: &'a Arc<dyn S3>,
    pub config: &'a Arc<dyn S3ConfigProvider>,
    pub host: Option<&'a dyn S3Host>,
    pub auth: Option<&'a dyn S3Auth>,
    pub access: Option<&'a dyn S3Access>,
    pub route: Option<&'a dyn S3Route>,
    pub validation: Option<&'a dyn NameValidation>,
}

fn build_s3_request<T>(input: T, req: &mut Request) -> S3Request<T> {
    let method = req.method.clone();
    let uri = mem::take(&mut req.uri);
    let headers = mem::take(&mut req.headers);
    let extensions = mem::take(&mut req.extensions);
    let credentials = req.s3ext.credentials.take();
    let region = req.s3ext.region.take();
    let service = req.s3ext.service.take();
    let trailing_headers = req.s3ext.trailing_headers.take();

    S3Request {
        input,
        method,
        uri,
        headers,
        extensions,
        credentials,
        region,
        service,
        trailing_headers,
    }
}

pub(crate) fn serialize_error(mut e: S3Error, no_decl: bool) -> S3Result<Response> {
    let status = e.status_code().unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    let mut res = Response::with_status(status);
    let bodyless = http::is_bodyless_status(status);
    if !bodyless {
        if no_decl {
            http::set_xml_body_no_decl(&mut res, &e)?;
        } else {
            http::set_xml_body(&mut res, &e)?;
        }
    }
    if let Some(headers) = e.take_headers() {
        res.headers = headers;
    }
    if bodyless {
        // RFC 9110 §6.4.1: 1xx/204/205/304 responses MUST NOT carry a body. The
        // XML body is skipped above; drop any body-describing headers too
        // (e.g. `content-type` from a custom error or the error's own headers).
        http::strip_bodyless(&mut res);
    }
    drop(e);
    Ok(res)
}
/// Serializes an error response for the given request method. `HEAD` responses
/// must not carry a body (RFC 9110 §9.3.2): the body is stripped while the
/// metadata headers are preserved.
pub(crate) fn serialize_error_for_method(method: &Method, e: S3Error, no_decl: bool) -> S3Result<Response> {
    let mut res = serialize_error(e, no_decl)?;
    if *method == Method::HEAD {
        http::strip_body(&mut res);
    }
    Ok(res)
}

const VIRTUAL_HOSTED_STYLE_HINT: &str = "\
The request appears to use virtual-hosted-style addressing \
(e.g., Host: bucket.domain) which may not be supported by this endpoint. \
If so, try path-style requests instead \
(e.g., /<bucket> rather than / with host bucket.domain).";

fn unknown_operation() -> S3Error {
    S3Error::with_message(S3ErrorCode::NotImplemented, "Unknown operation")
}

fn extract_http2_authority(req: &Request) -> Option<&str> {
    if matches!(req.version, ::http::Version::HTTP_2 | ::http::Version::HTTP_3)
        && let Some(authority) = req.uri.authority()
    {
        return Some(authority.as_str());
    }
    None
}

fn extract_host(req: &Request) -> S3Result<Option<String>> {
    // First try to get from Host header. Repeated Host lines are rejected
    // instead of silently picking the first value: signature verification
    // signs every value of a repeated header, so accepting only one here
    // would let routing and the signature disagree about the host.
    let mut iter = req.headers.get_all(crate::header::HOST).iter();
    if let Some(val) = iter.next() {
        if iter.next().is_some() {
            // AWS answers a repeated Host with 400 `DuplicateHeaderName` and
            // "Your request contains duplicate headers."; the code lives in
            // `data/s3_extra_error_codes.json` because the official error table does
            // not list it.
            return Err(s3_error!(DuplicateHeaderName));
        }
        let on_err = |e| s3_error!(e, InvalidRequest, "invalid header: Host: {val:?}");
        let host = val.to_str().map_err(on_err)?;
        return Ok(Some(host.into()));
    }

    // For HTTP/2 and HTTP/3, the Host header is replaced by :authority pseudo-header.
    // https://github.com/hyperium/hyper/discussions/2435
    if let Some(authority) = extract_http2_authority(req) {
        return Ok(Some(authority.into()));
    }

    Ok(None)
}

fn is_socket_addr_or_ip_addr(host: &str) -> bool {
    host.parse::<SocketAddr>().is_ok() || host.parse::<IpAddr>().is_ok()
}

fn looks_like_virtual_hosted_style(host: &str) -> bool {
    // Strip trailing port (e.g. ":9000").
    let host_part = match host.rsplit_once(':') {
        Some((h, port)) if port.bytes().all(|b| b.is_ascii_digit()) => h,
        _ => host,
    };
    // Strip brackets from IPv6 literals (e.g. "[::1]" → "::1").
    // This also covers IPv4-mapped IPv6 like "[::ffff:127.0.0.1]"
    // whose embedded dots could otherwise look like labels.
    let host_no_bracket = host_part
        .strip_prefix('[')
        .and_then(|s| s.strip_suffix(']'))
        .unwrap_or(host_part);
    if host_no_bracket.parse::<std::net::IpAddr>().is_ok() {
        return false;
    }
    // Virtual-hosted-style addresses bucket as a subdomain, so there are
    // at least three dot-separated labels: bucket.base.domain.
    // Two-label hosts (base.domain) and bare hostnames are excluded.
    // Empty segments are filtered to handle trailing-dot FQDN forms.
    host_no_bracket.split('.').filter(|s| !s.is_empty()).count() >= 3
}

fn convert_parse_s3_path_error(err: &ParseS3PathError) -> S3Error {
    match err {
        ParseS3PathError::InvalidPath => s3_error!(InvalidURI),
        ParseS3PathError::InvalidBucketName => s3_error!(InvalidBucketName),
        ParseS3PathError::KeyTooLong => s3_error!(KeyTooLongError),
    }
}

fn extract_qs(req_uri: &Uri) -> S3Result<Option<OrderedQs>> {
    let Some(query) = req_uri.query() else { return Ok(None) };
    match OrderedQs::parse(query) {
        Ok(ans) => Ok(Some(ans)),
        Err(source) => Err(S3Error::with_source(S3ErrorCode::InvalidURI, Box::new(source))),
    }
}

fn extract_mime(headers: &HeaderMap) -> Option<Mime> {
    let content_type = http::get_unique_header_str(headers, crate::header::CONTENT_TYPE.as_str())?;

    // https://github.com/s3s-project/s3s/issues/361
    if content_type.is_empty() {
        return None;
    }

    content_type.parse::<Mime>().ok()
}

fn invalid_content_length(val: &hyper::header::HeaderValue) -> S3Error {
    s3_error!(InvalidArgument, "invalid header: content-length: {val:?}")
}

fn extract_content_length(req: &Request) -> S3Result<Option<u64>> {
    let mut iter = req.headers.get_all(hyper::header::CONTENT_LENGTH).iter();
    let Some(val) = iter.next() else { return Ok(None) };
    if iter.next().is_some() {
        return Err(invalid_request!("duplicate header: content-length"));
    }

    let raw = val.to_str().map_err(|_| invalid_content_length(val))?;
    if raw.is_empty() || raw.bytes().any(|b| !b.is_ascii_digit()) {
        return Err(invalid_content_length(val));
    }

    raw.parse::<u64>().map(Some).map_err(|_| invalid_content_length(val))
}

fn signature_content_length(req: &Request, content_length: Option<u64>, request_has_payload: bool) -> Option<u64> {
    if content_length.is_none()
        && !request_has_payload
        && http::get_unique_header_str(&req.headers, header::X_AMZ_CONTENT_SHA256.as_str())
            .is_some_and(|value| value == s3s_sigv4::EMPTY_STRING_SHA256_HASH || value == "UNSIGNED-PAYLOAD")
    {
        Some(0)
    } else {
        content_length
    }
}

fn is_multipart_post(req: &Request) -> bool {
    req.method == Method::POST
        && extract_mime(&req.headers).is_some_and(|mime| mime.type_() == mime::MULTIPART && mime.subtype() == mime::FORM_DATA)
}

fn extract_decoded_content_length(headers: &'_ HeaderMap) -> S3Result<Option<usize>> {
    let Some(val) = http::get_unique_header_str(headers, crate::header::X_AMZ_DECODED_CONTENT_LENGTH.as_str()) else {
        return Ok(None);
    };
    let x = atoi::atoi::<u64>(val.as_bytes())
        .and_then(|n| usize::try_from(n).ok())
        .ok_or_else(|| invalid_request!("invalid header: x-amz-decoded-content-length"))?;
    Ok(Some(x))
}

async fn extract_full_body(content_length: Option<u64>, body: &mut Body, max_body_size: usize) -> S3Result<Bytes> {
    if let Some(bytes) = body.bytes() {
        return Ok(bytes);
    }

    let bytes = body.store_all_limited(max_body_size).await.map_err(|e| {
        if e.is::<BodySizeLimitExceeded>() {
            S3Error::with_source(S3ErrorCode::MaxMessageLengthExceeded, e)
        } else {
            S3Error::with_source(S3ErrorCode::InternalError, e)
        }
    })?;

    if bytes.is_empty().not() {
        let content_length = content_length.ok_or(S3ErrorCode::MissingContentLength)?;
        if bytes.len() as u64 != content_length {
            return Err(s3_error!(IncompleteBody));
        }
    }

    Ok(bytes)
}

/// Installs the aws-chunked decoder for anonymous STREAMING-UNSIGNED-PAYLOAD-TRAILER requests.
///
/// [`SignatureContext::v4_check_header_auth`] installs the decoder for signature
/// authenticated requests, where the chunk signatures have to be verified. An
/// unsigned declaration carries no signing context, so it needs no credentials:
/// AWS decodes the body and stores the decoded payload for anonymous requests as
/// well, and the framing must not reach the `S3` implementation.
fn install_anonymous_aws_chunked_body(req: &mut Request, config: &S3Config) -> S3Result {
    if req.s3ext.credentials.is_some() {
        return Ok(());
    }

    let Some(value) = http::get_unique_header_str(&req.headers, crate::header::X_AMZ_CONTENT_SHA256.as_str()) else {
        return Ok(());
    };
    // An anonymous request whose payload-hash declaration cannot be parsed is answered with
    // 400 `InvalidRequest`, which is the shape the service answers with 400. The SigV4 paths
    // answer `SignatureDoesNotMatch` for the same header because there the declaration is part
    // of the signed request.
    let amz_content_sha256 =
        s3s_sigv4::AmzContentSha256::parse(value).map_err(|_| invalid_request!("invalid header: x-amz-content-sha256"))?;
    if !matches!(amz_content_sha256, s3s_sigv4::AmzContentSha256::StreamingUnsignedPayloadTrailer) {
        return Ok(());
    }

    // Extracted and validated once by `verify_signature`; reading the header again here would
    // repeat that work for every anonymous streaming request.
    let decoded_content_length = req
        .s3ext
        .decoded_content_length
        .ok_or_else(|| s3_error!(MissingContentLength, "missing header: x-amz-decoded-content-length"))?;

    // A request that announced trailing headers has to carry the trailer block
    // it promised. A request that announced none may end at the completion
    // chunk, which is what AWS accepts for an unsigned streaming upload.
    let declared_trailer = http::get_unique_header_str(&req.headers, "x-amz-trailer").is_some();
    let mut stream =
        AwsChunkedStream::unsigned(mem::take(&mut req.body), decoded_content_length, config.aws_chunked_stream_max_chunk_size);
    if declared_trailer {
        stream.require_trailers(true);
    }

    let trailers = stream.trailing_headers_handle();
    req.s3ext.trailing_headers = Some(trailers);
    req.body = Body::from(stream.into_byte_stream());
    // The decoded body replaces the framing, so the declared length becomes the
    // plaintext length; the signature path rewrites it the same way.
    if let Some(val) = req.headers.get_mut(header::CONTENT_LENGTH) {
        *val = fmt_content_length(decoded_content_length);
    }

    Ok(())
}

fn prepare_streaming_body(req: &mut Request, config: &S3Config) -> S3Result {
    // Signature verification has already replaced aws-chunked bodies and
    // their Content-Length with the decoded payload length, when present;
    // anonymous unsigned declarations are decoded here.
    // An unrelated decoded-length header must not override an ordinary body.
    install_anonymous_aws_chunked_body(req, config)?;
    let content_length = extract_content_length(req)?;
    let known_length = content_length.or_else(|| req.body.remaining_length().exact().map(|x| x as u64));
    // The `x-s3s-payload-length` extension declares the bytes the sender delivers, so it has to
    // agree with the length the framing carries. `POST Object` is excluded: its declaration lives
    // in the form as a field, and an HTTP header there is an unsigned header the POST path rejects.
    if req.method != Method::POST {
        payload_length::enforce_declaration(&req.headers, known_length)?;
    }
    if let (Some(size), Some(limit)) = (known_length, config.put_object_max_size)
        && size > limit
    {
        return Err(s3_error!(EntityTooLarge, "Request body exceeds the configured maximum object size."));
    }
    req.body.set_limit(config.put_object_max_size);
    // A streaming upload has to declare its length: the service refuses a
    // request that carries neither `Content-Length` nor `Transfer-Encoding`
    // with 411 MissingContentLength, and such a request declares no body
    // length at all (RFC 9112 §6.3 reads it as zero bytes). An aws-chunked
    // request declares its length through `x-amz-decoded-content-length`, so
    // it keeps its decoded body. The size limit above keeps its precedence.
    if config.require_content_length
        && content_length.is_none()
        && req.s3ext.decoded_content_length.is_none()
        && !req.headers.contains_key(hyper::header::TRANSFER_ENCODING)
    {
        return Err(s3_error!(MissingContentLength));
    }
    // Backfill a known request-body length so that the `S3`
    // implementation never sees an ambiguous missing `Content-Length`.
    // Use the transformed body's length (aws-chunked uploads), or an
    // exact remaining length. Unknown-length bodies (chunked
    // transfer-encoding without aws-chunked) stay untouched.
    if config.normalize_content_length
        && content_length.is_none()
        && let Some(known) = known_length
    {
        req.headers
            .insert(hyper::header::CONTENT_LENGTH, hyper::header::HeaderValue::from(known));
    }
    Ok(())
}

fn reject_custom_route_body_too_large(content_length: Option<u64>, max_body_size: Option<u64>) -> S3Result {
    let Some(max_body_size) = max_body_size else {
        return Ok(());
    };
    let Some(content_length) = content_length else {
        return Ok(());
    };
    if content_length > max_body_size {
        return Err(s3_error!(
            EntityTooLarge,
            "Custom route request body exceeds the configured maximum size."
        ));
    }

    Ok(())
}

#[allow(clippy::declare_interior_mutable_const)]
fn fmt_content_length(len: usize) -> http::HeaderValue {
    const ZERO: http::HeaderValue = http::HeaderValue::from_static("0");
    if len > 0 {
        crate::utils::format::fmt_usize(len, |s| http::HeaderValue::try_from(s).unwrap())
    } else {
        ZERO
    }
}

pub async fn call(req: &mut Request, ccx: &CallContext<'_>) -> S3Result<Response> {
    let prep = match prepare(req, ccx).await {
        Ok(op) => op,
        Err(err) => {
            error!(?err, "failed to prepare");
            return serialize_error_for_method(&req.method, err, false);
        }
    };

    match prep {
        Prepare::S3(op) => {
            match op.call(ccx, req).await {
                Ok(resp) => {
                    Ok(resp) //
                }
                Err(err) => {
                    error!(op = %op.name(), ?err, "op returns error");
                    serialize_error_for_method(&req.method, err, false)
                }
            }
        }
        Prepare::CustomRoute => {
            let max_body_size = ccx.config.snapshot().custom_route_max_body_size;
            let result = reject_custom_route_body_too_large(extract_content_length(req)?, max_body_size);
            if let Err(err) = result {
                error!(?err, "custom route request body is too large");
                return serialize_error_for_method(&req.method, err, false);
            }

            let mut body = mem::take(&mut req.body);
            body.set_limit(max_body_size);
            let mut s3_req = build_s3_request(body, req);
            let route = ccx.route.unwrap();

            let result = async {
                route.check_access(&mut s3_req).await?;
                route.call(s3_req).await
            }
            .await;

            match result {
                Ok(s3_resp) => Ok(Response {
                    status: s3_resp.status.unwrap_or_default(),
                    headers: s3_resp.headers,
                    body: s3_resp.output,
                    extensions: s3_resp.extensions,
                }),
                Err(err) => {
                    error!(?err, "custom route returns error");
                    serialize_error_for_method(&req.method, err, false)
                }
            }
        }
    }
}

enum Prepare {
    S3(&'static dyn Operation),
    CustomRoute,
}

fn inject_host_header(req: &mut Request) {
    // HTTP/2 and HTTP/3 replace the Host header with the :authority pseudo-header.
    // hyper exposes :authority via uri.authority() but does not insert a Host entry
    // into the header map. For SigV4 (including presigned SigV4), the `host` header
    // is part of the canonical request, so inject it here for uniform handling.
    // This is primarily needed for SigV4 header canonicalization; SigV2 does not
    // include `Host` in its string-to-sign. Only do this for HTTP/2+ to avoid
    // synthesizing a Host header for HTTP/1.x requests that happen to use
    // absolute-form URIs.
    if !req.headers.contains_key(hyper::header::HOST)
        && let Some(authority) = extract_http2_authority(req)
        && let Ok(val) = hyper::header::HeaderValue::from_str(authority)
    {
        req.headers.insert(hyper::header::HOST, val);
    }
}

fn parse_request_host<'a>(
    ccx: &CallContext<'a>,
    host_header: Option<&'a str>,
) -> S3Result<(Option<VirtualHost<'a>>, Option<String>)> {
    // Virtual-host context feeds signature verification for both custom-route
    // and S3 traffic, so it is always resolved — and malformed hosts are
    // transport-level malformations rejected immediately (not a bucket/key
    // concern; see the routing step in [`prepare`]).
    if let (Some(host_header), Some(s3_host)) = (host_header, ccx.host)
        && !is_socket_addr_or_ip_addr(host_header)
    {
        let vh = s3_host.parse_host_header(host_header)?;
        debug!(?vh);
        let region = vh.region().map(str::to_owned);
        Ok((Some(vh), region))
    } else {
        Ok((None, None))
    }
}

/// Classifies the request path.
///
/// Matched custom routes skip this entirely: their paths are not bucket/key
/// paths, so no `S3Path` is materialized (`None` returned). Unmatched requests
/// run the combined parse+validate pipeline, raising legacy errors in the
/// legacy position.
///
/// Note: percent-decoding cannot be skipped in any branch — signature
/// verification decodes the path itself as a canonical-request input.
fn classify_request_path(
    decoded_uri_path: &str,
    ccx: &CallContext<'_>,
    vh_bucket: Option<&str>,
    custom_route_hit: bool,
    config: &S3Config,
) -> S3Result<Option<S3Path>> {
    if custom_route_hit {
        return Ok(None);
    }

    let default_validation = &const { AwsNameValidation::new() };
    let validation = ccx.validation.unwrap_or(default_validation);
    let normalize_path = config.normalize_forward_slash_path;

    let path = crate::path::parse_virtual_hosted_style_with_validation_and_normalization(
        vh_bucket,
        decoded_uri_path,
        validation,
        normalize_path,
    )
    .map_err(|err| convert_parse_s3_path_error(&err))?;

    Ok(Some(path))
}

fn resolve_operation(
    req: &Request,
    s3_path: &S3Path,
    host_header: Option<&str>,
    ccx: &CallContext<'_>,
) -> S3Result<&'static dyn Operation> {
    let op = match resolve_route(req, s3_path, req.s3ext.qs.as_ref()) {
        Ok(result) => result,
        Err(err) => {
            // When S3Host is absent and the host looks virtual-hosted-style,
            // bucket names in the Host header are lost — any routing failure
            // is likely caused by this mismatch.  Give an actionable error.
            if err.code() == &S3ErrorCode::NotImplemented
                && ccx.host.is_none()
                && let Some(host_header) = host_header
                && looks_like_virtual_hosted_style(host_header)
            {
                warn!(
                    ?host_header,
                    ?s3_path,
                    "request may be using virtual-hosted-style addressing; \
                     no S3 host parser is configured. \
                     Consider enabling an S3Host implementation if virtual-hosted-style \
                     requests need to be handled by this endpoint."
                );

                return Err(s3_error!(err, NotImplemented, "{}", VIRTUAL_HOSTED_STYLE_HINT));
            }
            // Not a virtual-hosted-style issue — propagate original error.
            return Err(err);
        }
    };

    Ok(op)
}

#[allow(clippy::too_many_arguments)]
async fn authorize(
    ccx: &CallContext<'_>,
    op_name: &'static str,
    credentials: Option<&Credentials>,
    s3_path: &S3Path,
    method: &Method,
    uri: &Uri,
    headers: &HeaderMap,
    extensions: &mut hyper::http::Extensions,
) -> S3Result<()> {
    if ccx.auth.is_none() {
        return Ok(());
    }

    let mut acx = S3AccessContext {
        credentials,
        s3_path,
        s3_op: &crate::S3Operation { name: op_name },
        method,
        uri,
        headers,
        extensions,
    };

    match ccx.access {
        Some(access) => access.check(&mut acx).await?,
        None => crate::access::default_check(&mut acx)?,
    }

    Ok(())
}

fn parse_post_policy(multipart: &crate::http::Multipart) -> S3Result<Option<PostPolicy>> {
    // Parse POST policy BEFORE reading file stream to prevent resource exhaustion
    // See https://docs.aws.amazon.com/AmazonS3/latest/API/sigv4-HTTPPOSTConstructPolicy.html
    let Some(policy_b64) = multipart.find_field_value("policy") else {
        return Ok(None);
    };
    let policy =
        PostPolicy::from_base64(policy_b64).map_err(|e| s3_error!(e, InvalidPolicyDocument, "failed to parse POST policy"))?;

    // Check policy expiration early to avoid reading file if policy is expired
    let now = crate::dto::Timestamp::from(std::time::SystemTime::now());
    if now >= policy.expiration {
        return Err(S3Error::with_message(S3ErrorCode::AccessDenied, "Request has expired"));
    }

    Ok(Some(policy))
}

/// Returns the byte range a POST object file part may have.
///
/// The policy's `content-length-range` carries the minimum and the maximum the
/// credential holder asked for; the configured maximum bounds the resource a
/// single POST can claim, so the tighter of the two maximums applies. Both ends
/// are enforced on the file stream while it is read.
///
/// A minimum above the effective maximum describes a range no body can satisfy:
/// that is the client's error, and it is answered before the stream is built so
/// the wrapper never sees inverted bounds.
fn post_object_file_size_range(policy: Option<&PostPolicy>, config_max: u64) -> S3Result<(u64, u64)> {
    let (min, max) = match policy.and_then(PostPolicy::content_length_range) {
        Some((min, max)) => (min, std::cmp::min(max, config_max)),
        None => (0, config_max),
    };
    if min > max {
        return Err(S3ErrorCode::EntityTooSmall.into());
    }
    Ok((min, max))
}

fn resolve_post_object(
    bucket: &str,
    multipart: &mut crate::http::Multipart,
    config: &S3Config,
) -> S3Result<(crate::stream::DynByteStream, Option<PostPolicy>)> {
    debug!(?multipart);

    // Substitute `${filename}` in the key field before the policy conditions
    // are evaluated, so `$key` constraints apply to the final key.
    multipart.substitute_key_filename();

    let policy = parse_post_policy(multipart)?;
    let (policy_min, policy_max) = post_object_file_size_range(policy.as_ref(), config.post_object_max_file_size)?;
    // The declaration of a form upload is a field, not a header, because a presigned POST signs
    // the policy document rather than the headers. It narrows the range the file stream enforces
    // instead of replacing it: the stream still counts the bytes actually delivered.
    let (min_file_size, max_file_size) = match payload_length::declared_form_field(multipart.fields())? {
        Some(declared) if policy_min.max(declared) > policy_max.min(declared) => {
            return Err(s3_error!(
                EntityTooSmall,
                "The declared payload length {declared} is outside the range the policy allows."
            ));
        }
        Some(declared) => (policy_min.max(declared), policy_max.min(declared)),
        None => (policy_min, policy_max),
    };

    // Prepare the file stream for the operation: the file part is forwarded as
    // a stream whose exact length is not known before the body is read — how
    // many bytes the closing trailer takes depends on the closing form the
    // client used, so a length derived from the request `Content-Length` is
    // exact for one closing form only. The range the policy requires is
    // enforced while the stream is read instead.
    let mut file_stream = multipart.take_file_stream().expect("missing file stream");
    file_stream.set_expected_range(min_file_size, max_file_size);
    let post_stream = crate::stream::into_dyn(file_stream);

    // Validate the policy conditions (if policy exists)
    // Note: expiration was already checked above before reading the file, and
    // `content-length-range` moved to the file stream, which is the only place
    // that sees the size. Pass the URL bucket so that the "bucket" condition
    // can be validated even when clients (like boto3) don't include it in form
    // fields.
    let mut policy_out = None;
    if let Some(policy) = policy {
        policy.validate_conditions_only(multipart, Some(bucket))?;
        policy_out = Some(policy);
    }

    Ok((post_stream, policy_out))
}

async fn verify_signature(
    req: &mut Request,
    ccx: &CallContext<'_>,
    vh_bucket: Option<&str>,
    vh_region: Option<&str>,
    mut content_length: Option<u64>,
    path_encoding: crate::auth::SigV4PathEncoding,
) -> S3Result<Option<u64>> {
    let decoded_uri_path = urlencoding::decode(req.uri.path()).map_err(|_| S3ErrorCode::InvalidURI)?;

    let mime = extract_mime(&req.headers);
    let decoded_content_length = extract_decoded_content_length(&req.headers)?;
    req.s3ext.decoded_content_length = decoded_content_length;

    let mut scx = SignatureContext {
        auth: ccx.auth,
        config: ccx.config,

        req_version: req.version,
        req_method: &req.method,
        req_uri: &req.uri,
        req_body: &mut req.body,

        qs: req.s3ext.qs.as_ref(),
        hs: &req.headers,

        decoded_uri_path: &decoded_uri_path,
        raw_uri_path: req.uri.path(),
        path_encoding,
        vh_bucket,

        content_length,
        decoded_content_length,
        mime,

        multipart: None,
        transformed_body: None,
        trailing_headers: None,
    };

    let credentials = scx.check().await?;

    // Harvest the outputs to release all borrows of `req` held by `scx`
    // before mutating its fields below.
    let transformed_body = scx.transformed_body;
    let multipart = scx.multipart;
    let trailing_headers = scx.trailing_headers;

    req.s3ext.multipart = multipart;
    req.s3ext.trailing_headers = trailing_headers;

    apply_credentials(req, credentials, vh_region)?;

    let body_changed = transformed_body.is_some() || req.s3ext.multipart.is_some();

    if body_changed {
        // invalidate the original content length
        if let Some(val) = req.headers.get_mut(header::CONTENT_LENGTH) {
            *val = fmt_content_length(decoded_content_length.unwrap_or(0));
        }
        content_length = content_length.map(|_| 0);
    }
    if let Some(body) = transformed_body {
        req.body = body;
    }

    debug!(?body_changed, ?decoded_content_length, has_multipart = req.s3ext.multipart.is_some());

    Ok(content_length)
}

fn apply_credentials(req: &mut Request, credentials: Option<CredentialsExt>, vh_region: Option<&str>) -> S3Result<()> {
    match credentials {
        Some(cred) => {
            req.s3ext.credentials = Some(Credentials {
                access_key: cred.access_key,
                secret_key: cred.secret_key,
            });

            let cred_region = cred
                .region
                .filter(|s| !s.is_empty())
                .map(|s| crate::region::Region::new(s.into()))
                .transpose()
                .map_err(|e| invalid_request!("invalid credential region: {e}"))?;

            // When both the signature credential and S3Host supply a region,
            // the credential region is authoritative (it was verified by the
            // signature check). Log a debug warning if they disagree so that
            // misconfigured clients or hosts are visible in traces.
            if let Some(cred_region) = &cred_region
                && let Some(host_region) = vh_region
                && cred_region.as_str() != host_region
            {
                debug!(
                    cred_region = %cred_region,
                    host_region = %host_region,
                    "credential region and virtual-host region differ; \
                     using credential region"
                );
            }

            req.s3ext.region = cred_region;
            req.s3ext.service = cred.service;
        }
        None => {
            req.s3ext.credentials = None;
            req.s3ext.region = None;
            req.s3ext.service = None;
        }
    }

    // Fallback: if no region was determined from the signature credential
    // (anonymous requests, SigV2), use the region provided by S3Host.
    if req.s3ext.region.is_none() {
        req.s3ext.region = vh_region
            .filter(|s| !s.is_empty())
            .map(|s| crate::region::Region::new(s.into()))
            .transpose()
            .map_err(|e| invalid_request!("invalid host region: {e}"))?;
    }

    Ok(())
}

/// Resolves the client-declared operation intent from the `x-id` query
/// parameter (signed under `SigV4` and sent by official SDKs). The former
/// `x-s3s-operation-id` header extension was removed: it was a redundant,
/// unsigned carrier with no confirmed benefit, and checking for a present
/// header costs ~12 ns/op in the hot path.
///
/// The lookup is partitioned by (HTTP method, path shape) and each partition
/// resolves the declared name through a generated `match` over the official
/// operation names. The declaration is authoritative: required query
/// strings/headers and query tags are validated by the operation's
/// `deserialize_http` step rather than here.
///
/// Returns `Ok(None)` when no signal is present or the feature is disabled
/// (the caller falls back to the full router); `Ok(Some(op))` when the
/// declared operation is resolved; `Err` when the declaration is invalid or
/// does not match the request shape. Errors must be deferred until after
/// signature verification (see [`prepare`]).
///
/// `config` is the caller's request-level snapshot (taken once in
/// [`prepare`]); passing it avoids a second ~10 ns `snapshot()` per request.
fn resolve_oir(req: &Request, config: &S3Config) -> S3Result<Option<&'static dyn Operation>> {
    if !config.operation_id_routing {
        return Ok(None);
    }

    // `x-id` query parameter: duplicate keys are invalid (fail-closed).
    let signal = match req.s3ext.qs.as_ref().map(|qs| qs.lookup("x-id")) {
        Some(QsLookup::Duplicate) => return Err(invalid_request!("duplicate x-id")),
        Some(QsLookup::Single(v)) => Some(v),
        _ => None,
    };
    let Some(signal) = signal else {
        return Ok(None);
    };

    let s3_path = req.s3ext.s3_path.as_ref().expect("path classified before OIR");
    let Some(op) = generated::resolve_operation_by_id(req.method.as_str(), s3_path, signal) else {
        // The lookup is partitioned by (method, path shape), so a miss covers
        // both an unknown id and a known id that does not match the request
        // shape; both are InvalidRequest.
        return Err(s3_error!(
            InvalidRequest,
            "operation id {signal} is unknown or does not match this request"
        ));
    };

    Ok(Some(op))
}

#[tracing::instrument(level = "debug", skip_all, err)]
async fn prepare(req: &mut Request, ccx: &CallContext<'_>) -> S3Result<Prepare> {
    // Take one config snapshot for the whole request: `snapshot()` costs
    // ~10 ns (an `Arc` clone), and routing/body handling below read several
    // config fields. A single snapshot also keeps the request internally
    // consistent.
    let config = ccx.config.snapshot();
    let mut content_length;

    inject_host_header(req);
    let host_header = extract_host(req)?;

    // Percent-decode stays ahead of routing: it is a signature-verification
    // input anyway, and decoding here keeps the legacy error precedence for
    // malformed paths regardless of route configuration.
    let decoded_uri_path = urlencoding::decode(req.uri.path()).map_err(|_| S3ErrorCode::InvalidURI)?;
    debug!(?decoded_uri_path, "parsing request path");

    let (vh, vh_region) = parse_request_host(ccx, host_header.as_deref())?;

    // Custom routes claim requests before S3 naming semantics are enforced:
    // no `S3Path` is materialized for them. The predicate may observe any
    // request that reaches the service; authentication is enforced later via
    // `check_access` / `authorize`. Virtual-host resolution above stays
    // unconditional — it feeds signature verification with full context.
    let custom_route_hit = ccx
        .route
        .is_some_and(|route| route.is_match(&req.method, &req.uri, &req.headers, &mut req.extensions));

    // Matched routes skip path classification; unmatched requests run the
    // legacy combined parse+validate here, preserving error codes/ordering.
    let vh_bucket = vh.as_ref().and_then(VirtualHost::bucket);
    req.s3ext.s3_path = classify_request_path(&decoded_uri_path, ccx, vh_bucket, custom_route_hit, &config)?;

    req.s3ext.qs = extract_qs(&req.uri)?;
    content_length = extract_content_length(req)?;

    // Resolve the operation early (tolerantly) to decide whether the request
    // carries a payload: signature verification rejects missing
    // `Content-Length` for payload-consuming operations. The result is cached
    // and reused by the real resolution below, so `resolve_route` runs exactly
    // once on the success path. Errors are swallowed here and reported by the
    // real resolution, preserving error precedence. Custom routes and
    // multipart POST requests are skipped (conservatively treated as having a
    // payload) because their routing depends on state parsed during signature
    // verification.
    // OIR operation resolution from client-declared intent: the `x-id`
    // query parameter (signed under SigV4, sent by official SDKs) is the sole
    // signal. On success the confirmed operation is cached and reused below,
    // so `resolve_route` does not run on the OIR path. On a declaration
    // error (duplicate / unknown / not matching) only a one-byte error kind
    // is kept, deferred until after signature verification so that
    // authentication errors take precedence, mirroring the tolerant
    // full-router resolve below. Custom routes and multipart POST requests
    // are skipped (their routing depends on other state).
    let mut oir_error: Option<S3Error> = None;
    let resolved_op = if custom_route_hit || is_multipart_post(req) {
        None
    } else {
        match resolve_oir(req, &config) {
            Ok(Some(op)) => Some(op),
            Ok(None) => req
                .s3ext
                .s3_path
                .as_ref()
                .and_then(|s3_path| generated::resolve_route(req, s3_path, req.s3ext.qs.as_ref()).ok()),
            Err(err) => {
                oir_error = Some(err);
                None
            }
        }
    };
    let request_has_payload = if custom_route_hit {
        // Custom routes have no modeled operation to declare whether they
        // consume a payload; the `x-amz-content-sha256` header is
        // authoritative instead. The empty-string hash marks a bodyless
        // request — `mc admin` sends bodyless PUTs (e.g. `set-user-status`)
        // without `Content-Length`, and treating them as payload-bearing here
        // would make signature verification demand a `Content-Length` (411).
        http::get_unique_header_str(&req.headers, header::X_AMZ_CONTENT_SHA256.as_str())
            != Some(s3s_sigv4::EMPTY_STRING_SHA256_HASH)
    } else {
        resolved_op.as_ref().is_none_or(|op| op.has_request_payload())
    };
    let content_length_for_signature = signature_content_length(req, content_length, request_has_payload);
    let path_encoding = if custom_route_hit {
        req.extensions
            .get::<crate::auth::SigV4PathEncoding>()
            .copied()
            .unwrap_or_default()
    } else {
        crate::auth::SigV4PathEncoding::S3
    };
    content_length =
        verify_signature(req, ccx, vh_bucket, vh_region.as_deref(), content_length_for_signature, path_encoding).await?;

    if custom_route_hit {
        return Ok(Prepare::CustomRoute);
    }

    // Deferred OIR errors surface only after authentication succeeded.
    if let Some(err) = oir_error {
        return Err(err);
    }

    let op = if let Some(op) = resolved_op {
        op
    } else {
        'resolve: {
            let s3_path = req.s3ext.s3_path.as_ref().expect("classified above");
            if let Some(multipart) = &mut req.s3ext.multipart
                && req.method == Method::POST
            {
                match s3_path {
                    S3Path::Root => return Err(unknown_operation()),
                    S3Path::Bucket { bucket } => {
                        let (stream, policy) = resolve_post_object(bucket, multipart, &config)?;
                        req.s3ext.post_object_stream = Some(stream);
                        req.s3ext.post_policy = policy;
                        break 'resolve &PostObject as &'static dyn Operation;
                    }
                    // A multipart POST whose path names an object is not a modeled S3
                    // operation: `PostObject` binds to `/{Bucket}` only — the key is
                    // carried by the `key` form field, never the URL path. AWS and
                    // MinIO reject such requests with `MethodNotAllowed`; keep that
                    // behavior.
                    S3Path::Object { .. } => return Err(s3_error!(MethodNotAllowed)),
                }
            }
            resolve_operation(req, s3_path, host_header.as_deref(), ccx)?
        }
    };

    let s3_path = req.s3ext.s3_path.as_ref().unwrap();
    debug!(op = %op.name(), ?s3_path, "resolved route");

    // Header validity is decided before the access decision: AWS answers these
    // malformed-header errors even for callers that lack the operation
    // permission.
    http::validate_checksum_headers(req)?;
    http::validate_expected_bucket_owner(req)?;

    authorize(
        ccx,
        op.name(),
        req.s3ext.credentials.as_ref(),
        s3_path,
        &req.method,
        &req.uri,
        &req.headers,
        &mut req.extensions,
    )
    .await?;

    debug!(op = %op.name(), ?s3_path, "checked access");

    if op.needs_full_body() {
        extract_full_body(content_length, &mut req.body, config.xml_max_body_size).await?;
    } else if op.has_streaming_body() {
        prepare_streaming_body(req, &config)?;
    }

    Ok(Prepare::S3(op))
}
