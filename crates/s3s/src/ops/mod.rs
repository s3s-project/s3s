// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

//! Internal S3 operation dispatch, HTTP serialization, and deserialization.
//!
//! This module converts incoming HTTP requests into typed operation inputs,
//! invokes the user-provided [`S3`](crate::S3) implementation, and converts
//! the resulting outputs or errors back into HTTP responses.

cfg_if::cfg_if! {
    if #[cfg(feature = "minio")] {
        mod generated_minio;
        use self::generated_minio as generated;
    } else {
        mod generated;
    }
}

pub use self::generated::*;

mod signature;
use self::signature::{CredentialsExt, SignatureContext};

mod get_object;
mod multipart;

#[cfg(test)]
mod tests;

#[cfg(test)]
mod route_skip_validation_tests;

#[cfg(test)]
mod route_bench;

#[cfg(test)]
mod route_fixture_check;

use crate::access::{S3Access, S3AccessContext};
use crate::auth::{Credentials, S3Auth};
use crate::config::S3ConfigProvider;
use crate::error::*;
use crate::header;
use crate::host::{S3Host, VirtualHost};
use crate::http::Body;
use crate::http::OrderedQs;
use crate::http::{self, BodySizeLimitExceeded};
use crate::http::{Request, Response};
use crate::path::{ParseS3PathError, S3Path};
use crate::post_policy::PostPolicy;
use crate::protocol::S3Request;
use crate::route::S3Route;
use crate::s3_trait::S3;
use crate::stream::ByteStream as _;
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
    fn has_streaming_body(&self) -> bool {
        false
    }

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
    if no_decl {
        http::set_xml_body_no_decl(&mut res, &e)?;
    } else {
        http::set_xml_body(&mut res, &e)?;
    }
    if let Some(headers) = e.take_headers() {
        res.headers = headers;
    }
    drop(e);
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
            return Err(invalid_request!("duplicate header: Host"));
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

fn check_query_pattern(qs: &OrderedQs, name: &str, val: &str) -> bool {
    match qs.get_unique(name) {
        Some(v) => v == val,
        None => false,
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
            == Some(s3s_sigv4::EMPTY_STRING_SHA256_HASH)
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

/// Prepares the POST object file stream for the operation.
///
/// The file part is the last part of a POST object form. When the request
/// carries a `Content-Length` header, the multipart parser derives the exact
/// file length from it (see `http::transform_multipart`), so the file can be
/// forwarded to the operation as a stream without aggregation. Chunked
/// requests (or forms without a known length) fall back to aggregating the
/// file into memory to obtain the size.
///
/// # Errors
/// Returns an error if the file exceeds `max_file_size` or the file stream
/// cannot be read.
async fn prepare_post_object_stream(
    file_stream: http::FileStream,
    max_file_size: u64,
) -> S3Result<(crate::stream::DynByteStream, u64)> {
    match file_stream.content_len() {
        Some(content_len) => {
            // The derived length is exact: enforce the size limit before
            // dispatch without buffering the file. The file stream itself
            // tracks the remaining length and checks the canonical trailer
            // while being consumed.
            if content_len > max_file_size {
                return Err(s3_error!(EntityTooLarge, "Your proposed upload exceeds the maximum allowed object size."));
            }
            Ok((crate::stream::into_dyn(file_stream), content_len))
        }
        None => {
            // Aggregate file stream with size limit to get known length
            // This is required because downstream handlers (like s3s-proxy) need content-length
            let vec_bytes = http::aggregate_file_stream_limited(file_stream, max_file_size)
                .await
                .map_err(|e| match e {
                    http::MultipartError::FileTooLarge(..) => {
                        s3_error!(EntityTooLarge, "Your proposed upload exceeds the maximum allowed object size.")
                    }
                    other => invalid_request!(other, "failed to read file stream"),
                })?;
            // Use saturating_add to prevent overflow in release builds (security-relevant for content-length-range validation)
            let file_size = vec_bytes.iter().map(|b| b.len() as u64).fold(0u64, u64::saturating_add);
            let vec_stream = crate::stream::VecByteStream::new(vec_bytes);
            Ok((crate::stream::into_dyn(vec_stream), file_size))
        }
    }
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
            return serialize_error(err, false);
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
                    serialize_error(err, false)
                }
            }
        }
        Prepare::CustomRoute => {
            let max_body_size = ccx.config.snapshot().custom_route_max_body_size;
            let result = reject_custom_route_body_too_large(extract_content_length(req)?, max_body_size);
            if let Err(err) = result {
                error!(?err, "custom route request body is too large");
                return serialize_error(err, false);
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
                    serialize_error(err, false)
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
) -> S3Result<Option<S3Path>> {
    if custom_route_hit {
        return Ok(None);
    }

    let default_validation = &const { AwsNameValidation::new() };
    let validation = ccx.validation.unwrap_or(default_validation);
    let normalize_path = ccx.config.snapshot().normalize_forward_slash_path;

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
    // Note: clone is necessary because Into<OffsetDateTime> consumes the Timestamp
    let expiration_time: time::OffsetDateTime = policy.expiration.clone().into();
    let now = time::OffsetDateTime::now_utc();
    if now >= expiration_time {
        return Err(S3Error::with_message(S3ErrorCode::AccessDenied, "Request has expired"));
    }

    Ok(Some(policy))
}

fn post_object_max_file_size(policy: Option<&PostPolicy>, config_max: u64) -> u64 {
    // Determine file size limit: use stricter of policy max or config max.
    // Use the minimum of policy max and config max to prevent resource exhaustion.
    // Note: policy min is validated later in policy.validate()
    match policy.and_then(PostPolicy::content_length_range) {
        Some((_, max)) => std::cmp::min(max, config_max),
        None => config_max,
    }
}

async fn resolve_post_object(
    ccx: &CallContext<'_>,
    bucket: &str,
    multipart: &mut crate::http::Multipart,
) -> S3Result<(crate::stream::DynByteStream, Option<PostPolicy>)> {
    debug!(?multipart);

    // Substitute `${filename}` in the key field before the policy conditions
    // are evaluated, so `$key` constraints apply to the final key.
    multipart.substitute_key_filename();

    let policy = parse_post_policy(multipart)?;
    let max_file_size = post_object_max_file_size(policy.as_ref(), ccx.config.snapshot().post_object_max_file_size);

    // Prepare the file stream for the operation: forwarded as a
    // stream when the exact length is known, aggregated otherwise.
    let file_stream = multipart.take_file_stream().expect("missing file stream");
    let (post_stream, file_size) = prepare_post_object_stream(file_stream, max_file_size).await?;

    // Validate the policy conditions (if policy exists)
    // Note: expiration was already checked above before reading the file
    // Pass the URL bucket so that the "bucket" condition can be validated
    // even when clients (like boto3) don't include it in form fields.
    let mut policy_out = None;
    if let Some(policy) = policy {
        policy.validate_conditions_only(multipart, file_size, Some(bucket))?;
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
) -> S3Result<Option<u64>> {
    let decoded_uri_path = urlencoding::decode(req.uri.path()).map_err(|_| S3ErrorCode::InvalidURI)?;

    let mime = extract_mime(&req.headers);
    let decoded_content_length = extract_decoded_content_length(&req.headers)?;

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

#[tracing::instrument(level = "debug", skip_all, err)]
async fn prepare(req: &mut Request, ccx: &CallContext<'_>) -> S3Result<Prepare> {
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
    req.s3ext.s3_path = classify_request_path(&decoded_uri_path, ccx, vh_bucket, custom_route_hit)?;

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
    let resolved_op = if custom_route_hit || is_multipart_post(req) {
        None
    } else {
        req.s3ext
            .s3_path
            .as_ref()
            .and_then(|s3_path| generated::resolve_route(req, s3_path, req.s3ext.qs.as_ref()).ok())
    };
    let request_has_payload = resolved_op.as_ref().is_none_or(|op| op.has_request_payload());
    let content_length_for_signature = signature_content_length(req, content_length, request_has_payload);
    content_length = verify_signature(req, ccx, vh_bucket, vh_region.as_deref(), content_length_for_signature).await?;

    if custom_route_hit {
        return Ok(Prepare::CustomRoute);
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
                        let (stream, policy) = resolve_post_object(ccx, bucket, multipart).await?;
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

    let config = ccx.config.snapshot();
    if op.needs_full_body() {
        extract_full_body(content_length, &mut req.body, config.xml_max_body_size).await?;
    } else if op.has_streaming_body() {
        req.body.set_limit(config.put_object_max_size);
        // Backfill a known request-body length so that the `S3`
        // implementation never sees an ambiguous missing `Content-Length`.
        // The `x-amz-decoded-content-length` value wins (aws-chunked uploads),
        // otherwise an exact remaining length (e.g. an empty body without
        // `Content-Length`, which is empty by definition per RFC 9112 §6.3).
        // Unknown-length bodies (chunked transfer-encoding without
        // aws-chunked) stay untouched.
        if config.normalize_content_length && content_length.is_none() {
            let known = extract_decoded_content_length(&req.headers)?
                .map(|x| x as u64)
                .or_else(|| req.body.remaining_length().exact().map(|x| x as u64));
            if let Some(known) = known {
                req.headers
                    .insert(hyper::header::CONTENT_LENGTH, hyper::header::HeaderValue::from(known));
            }
        }
    }

    Ok(Prepare::S3(op))
}
