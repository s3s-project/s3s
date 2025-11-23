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
use self::signature::SignatureContext;

mod get_object;
mod multipart;

#[cfg(test)]
mod tests;

use crate::access::{S3Access, S3AccessContext};
use crate::auth::{Credentials, S3Auth};
use crate::error::*;
use crate::header;
use crate::host::S3Host;
use crate::http;
use crate::http::Body;
use crate::http::{OrderedHeaders, OrderedQs};
use crate::http::{Request, Response};
use crate::path::{ParseS3PathError, S3Path};
use crate::protocol::S3Request;
use crate::route::S3Route;
use crate::s3_trait::S3;
use crate::stream::VecByteStream;
use crate::stream::aggregate_unlimited;
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
use tracing::{debug, error};

/// Trait representing a single S3 operation (e.g., GetObject, PutObject).
///
/// Each S3 operation implements this trait to handle specific API requests.
/// Operations are resolved during the routing phase and executed via `call()`.
#[async_trait::async_trait]
pub trait Operation: Send + Sync + 'static {
    /// Returns the operation name (e.g., "GetObject", "ListBuckets")
    fn name(&self) -> &'static str;

    /// Executes the operation with the given context and request.
    ///
    /// This method contains the core logic for processing the specific S3 operation,
    /// interacting with the storage backend, and building the response.
    async fn call(&self, ccx: &CallContext<'_>, req: &mut Request) -> S3Result<Response>;
}

/// Context passed to operations containing all configured service components.
///
/// This structure bundles all optional components (auth, access control, routing, etc.)
/// so operations can access them as needed during request processing.
pub struct CallContext<'a> {
    pub s3: &'a Arc<dyn S3>,
    pub host: Option<&'a dyn S3Host>,
    pub auth: Option<&'a dyn S3Auth>,
    pub access: Option<&'a dyn S3Access>,
    pub route: Option<&'a dyn S3Route>,
    pub validation: Option<&'a dyn NameValidation>,
}

/// Builds an S3Request by extracting components from the internal Request.
///
/// Transfers ownership of method, URI, headers, extensions, and S3-specific
/// metadata (credentials, region, etc.) from the internal request to create
/// a clean S3Request that operations can work with.
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

/// Converts an S3Error into an HTTP response with XML error body.
///
/// This function ensures all errors are returned in S3-compatible format
/// with proper status codes and XML error structure.
///
/// # Arguments
///
/// * `e` - The S3 error to serialize
/// * `no_decl` - If true, omits XML declaration (for certain edge cases)
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

fn unknown_operation() -> S3Error {
    S3Error::with_message(S3ErrorCode::NotImplemented, "Unknown operation")
}

fn extract_host(req: &Request) -> S3Result<Option<String>> {
    let Some(val) = req.headers.get(crate::header::HOST) else { return Ok(None) };
    let on_err = |e| s3_error!(e, InvalidRequest, "invalid header: Host: {val:?}");
    let host = val.to_str().map_err(on_err)?;
    Ok(Some(host.into()))
}

/// Checks if the host string is an IP address or socket address.
///
/// Used to determine if virtual-hosted-style parsing should be skipped.
/// When connecting via IP (e.g., 127.0.0.1:8080), path-style is used.
fn is_socket_addr_or_ip_addr(host: &str) -> bool {
    host.parse::<SocketAddr>().is_ok() || host.parse::<IpAddr>().is_ok()
}

/// Converts path parsing errors to appropriate S3 error codes.
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

fn extract_headers(headers: &HeaderMap) -> S3Result<OrderedHeaders<'_>> {
    OrderedHeaders::from_headers(headers).map_err(|source| invalid_request!(source, "invalid headers"))
}

fn extract_mime(hs: &OrderedHeaders<'_>) -> Option<Mime> {
    let content_type = hs.get_unique(crate::header::CONTENT_TYPE)?;

    // https://github.com/s3s-project/s3s/issues/361
    if content_type.is_empty() {
        return None;
    }

    content_type.parse::<Mime>().ok()
}

fn extract_content_length(req: &Request) -> Option<u64> {
    req.headers
        .get(hyper::header::CONTENT_LENGTH)
        .and_then(|val| atoi::atoi::<u64>(val.as_bytes()))
}

fn extract_decoded_content_length(hs: &'_ OrderedHeaders<'_>) -> S3Result<Option<usize>> {
    let Some(val) = hs.get_unique(crate::header::X_AMZ_DECODED_CONTENT_LENGTH) else { return Ok(None) };
    match atoi::atoi::<usize>(val.as_bytes()) {
        Some(x) => Ok(Some(x)),
        None => Err(invalid_request!("invalid header: x-amz-decoded-content-length")),
    }
}

/// Extracts and validates the complete request body.
///
/// This function handles body extraction and validation:
/// - If body is already buffered, returns it immediately
/// - Otherwise, reads the entire body into memory
/// - Validates that actual body size matches Content-Length header
///
/// Used for operations that require the full body upfront (e.g., PutObject).
async fn extract_full_body(content_length: Option<u64>, body: &mut Body) -> S3Result<Bytes> {
    if let Some(bytes) = body.bytes() {
        return Ok(bytes);
    }

    let bytes = body
        .store_all_unlimited()
        .await
        .map_err(|e| S3Error::with_source(S3ErrorCode::InternalError, e))?;

    if bytes.is_empty().not() {
        let content_length = content_length.ok_or(S3ErrorCode::MissingContentLength)?;
        if bytes.len() as u64 != content_length {
            return Err(s3_error!(IncompleteBody));
        }
    }

    Ok(bytes)
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

/// The main dispatcher that orchestrates the entire S3 request processing pipeline.
///
/// This function is the core of the S3 request handling system. It coordinates
/// authentication, authorization, routing, and operation execution.
///
/// # Flow Diagram
///
/// ```text
///                                ┌─────────────┐
///                                │   call()    │
///                                └──────┬──────┘
///                                       │
///                                       ▼
///                            ┌──────────────────────┐
///                            │   prepare(req, ccx)  │
///                            │                      │
///                            │ • Parse S3 path      │
///                            │ • Extract headers/qs │
///                            │ • Verify signature   │
///                            │ • Transform body     │
///                            │ • Route resolution   │
///                            └──────────┬───────────┘
///                                       │
///                    ┌──────────────────┴──────────────────┐
///                    │                                     │
///              ┌─────▼─────┐                         ┌────▼────┐
///              │   Error   │                         │   Ok    │
///              └─────┬─────┘                         └────┬────┘
///                    │                                    │
///                    ▼                                    ▼
///         ┌────────────────────┐              ┌──────────────────────┐
///         │ serialize_error()  │              │  Prepare Result      │
///         │  • Convert to XML  │              └──────────┬───────────┘
///         │  • Set status code │                         │
///         └────────────────────┘          ┌──────────────┴───────────────┐
///                                         │                              │
///                                   ┌─────▼──────┐              ┌────────▼────────┐
///                                   │ Prepare::S3│              │Prepare::Custom  │
///                                   │ (operation)│              │     Route       │
///                                   └─────┬──────┘              └────────┬────────┘
///                                         │                              │
///                                         ▼                              ▼
///                              ┌────────────────────┐        ┌──────────────────────┐
///                              │  op.call(ccx, req) │        │ route.check_access() │
///                              │                    │        │ route.call()         │
///                              │ • Execute S3 op    │        │                      │
///                              │   (GetObject, etc) │        │ • Custom handler     │
///                              └──────────┬─────────┘        └──────────┬───────────┘
///                                         │                              │
///                          ┌──────────────┴──────────────┐               │
///                          │                             │               │
///                    ┌─────▼─────┐               ┌──────▼──────┐        │
///                    │    Ok     │               │    Error    │        │
///                    │           │               │             │        │
///                    └─────┬─────┘               └──────┬──────┘        │
///                          │                            │               │
///                          │                            ▼               │
///                          │                 ┌────────────────────┐     │
///                          │                 │ serialize_error()  │     │
///                          │                 └────────────────────┘     │
///                          │                                            │
///                          └────────────────────┬───────────────────────┘
///                                               │
///                                               ▼
///                                      ┌─────────────────┐
///                                      │  S3 Response    │
///                                      │                 │
///                                      │ • Status code   │
///                                      │ • Headers       │
///                                      │ • Body          │
///                                      └─────────────────┘
/// ```
///
/// # Processing Stages
///
/// ## 1. Preparation Phase (`prepare`)
///
/// The preparation phase handles:
/// - **Path parsing**: Extracts bucket/object from URI (path-style or virtual-hosted-style)
/// - **Authentication**: Verifies AWS Signature V2/V4, extracts credentials
/// - **Body transformation**: Handles chunked encoding, multipart forms
/// - **Route resolution**: Determines which operation to execute
/// - **Access control**: Checks permissions for the requested operation
///
/// ## 2. Execution Phase
///
/// Depending on the preparation result:
///
/// ### Standard S3 Operations (`Prepare::S3`)
/// - Calls the resolved operation (e.g., `GetObject`, `PutObject`)
/// - Operation processes the request using the S3 storage backend
/// - Returns success response or error
///
/// ### Custom Routes (`Prepare::CustomRoute`)
/// - Checks access permissions via `route.check_access()`
/// - Delegates to custom route handler
/// - Useful for extending S3 API with custom endpoints
///
/// ## 3. Error Handling
///
/// All errors are serialized to S3-compatible XML error responses with appropriate
/// HTTP status codes. Errors are logged at the `error` level with context.
///
/// # Arguments
///
/// * `req` - Mutable reference to the incoming HTTP request
/// * `ccx` - Call context containing configured components (auth, access, routing, etc.)
///
/// # Returns
///
/// * `Ok(Response)` - Successfully processed response (may be error response with proper XML)
/// * `Err(S3Error)` - Only returned if error serialization itself fails (rare)
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
            let body = mem::take(&mut req.body);
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

/// Result of the preparation phase indicating how to handle the request.
enum Prepare {
    /// Standard S3 operation with resolved operation handler
    S3(&'static dyn Operation),
    /// Custom route that bypasses standard S3 operation handling
    CustomRoute,
}

/// Prepares an incoming request for execution.
///
/// This complex function handles the entire request preparation pipeline:
///
/// 1. **Path Resolution**
///    - Decodes URI path
///    - Parses virtual-hosted-style (bucket.s3.example.com) or path-style (/bucket/key)
///    - Validates bucket/object names
///
/// 2. **Signature Verification**
///    - Checks AWS Signature V2/V4 if auth is configured
///    - Handles chunked encoding (aws-chunked)
///    - Processes multipart/form-data uploads
///    - Transforms request body if needed
///
/// 3. **Route Resolution**
///    - Checks for custom route matches first
///    - Falls back to standard S3 operation resolution
///    - Maps (method, path, query) to specific operations
///
/// 4. **Access Control**
///    - Verifies permissions for the resolved operation
///    - Uses configured access handler or default rules
///
/// 5. **Body Handling**
///    - Loads full body if operation requires it
///    - Validates content-length matches actual body size
///
/// # Returns
///
/// * `Ok(Prepare::S3(op))` - Resolved to a standard S3 operation
/// * `Ok(Prepare::CustomRoute)` - Matched a custom route
/// * `Err(S3Error)` - Preparation failed (invalid request, auth failure, etc.)
#[allow(clippy::too_many_lines)]
#[tracing::instrument(level = "debug", skip_all, err)]
async fn prepare(req: &mut Request, ccx: &CallContext<'_>) -> S3Result<Prepare> {
    let s3_path;
    let mut content_length;
    {
        let decoded_uri_path = urlencoding::decode(req.uri.path())
            .map_err(|_| S3ErrorCode::InvalidURI)?
            .into_owned();

        let host_header = extract_host(req)?;
        let vh;
        let vh_bucket;
        {
            let default_validation = &const { AwsNameValidation::new() };
            let validation = ccx.validation.unwrap_or(default_validation);

            // Core Logic: Parse the S3 path based on the host header and URI path.
            let result = 'parse: {
                if let (Some(host_header), Some(s3_host)) = (host_header.as_deref(), ccx.host) {
                    if !is_socket_addr_or_ip_addr(host_header) {
                        debug!(?host_header, ?decoded_uri_path, "parsing virtual-hosted-style request");

                        vh = s3_host.parse_host_header(host_header)?;
                        debug!(?vh);

                        vh_bucket = vh.bucket();
                        break 'parse crate::path::parse_virtual_hosted_style_with_validation(
                            vh_bucket,
                            &decoded_uri_path,
                            validation,
                        );
                    }
                }

                debug!(?decoded_uri_path, "parsing path-style request");
                vh_bucket = None;
                crate::path::parse_path_style_with_validation(&decoded_uri_path, validation)
            };

            req.s3ext.s3_path = Some(result.map_err(|err| convert_parse_s3_path_error(&err))?);
            s3_path = req.s3ext.s3_path.as_ref().unwrap();
        }

        req.s3ext.qs = extract_qs(&req.uri)?;
        content_length = extract_content_length(req);

        let hs = extract_headers(&req.headers)?;
        let mime = extract_mime(&hs);
        let decoded_content_length = extract_decoded_content_length(&hs)?;

        // ===================================================================
        // AWS Signature V2/V4 Verification (Core Authentication Logic)
        // ===================================================================
        //
        // This block handles all forms of AWS signature verification:
        //
        // Signature V4 (AWS4-HMAC-SHA256):
        //   1. Header-based auth: Authorization header with AWS4-HMAC-SHA256
        //   2. Presigned URLs: X-Amz-Signature in query string
        //   3. POST signature: multipart/form-data with x-amz-signature
        //
        // Signature V2 (AWS2):
        //   1. Header-based auth: Authorization header with AWS
        //   2. Presigned URLs: Signature in query string
        //   3. POST signature: multipart/form-data with signature
        //
        // The SignatureContext.check() method:
        //   - Detects signature type from headers/query parameters
        //   - Retrieves the secret key for the given access key
        //   - Calculates expected signature using canonical request
        //   - Compares with provided signature
        //   - Handles special cases like:
        //       * aws-chunked encoding for streaming uploads
        //       * Content-SHA256 verification
        //       * Trailing headers for chunked uploads
        //       * Multipart form data transformations
        //
        // If no signature is present and auth is not configured, the request
        // is treated as anonymous and credentials remain None.
        //
        let body_changed;
        let transformed_body;
        {
            let mut scx = SignatureContext {
                // This allows users to customize how access credentials and secret keys are retrieved.
                auth: ccx.auth,

                req_version: req.version,
                req_method: &req.method,
                req_uri: &req.uri,
                req_body: &mut req.body,

                qs: req.s3ext.qs.as_ref(),
                hs,

                decoded_uri_path,
                vh_bucket,

                content_length,
                decoded_content_length,
                mime,

                multipart: None,
                transformed_body: None,
                trailing_headers: None,
            };

            // Execute signature verification - this is where authentication happens!
            // Returns credentials if signature is valid, None for anonymous requests,
            // or S3Error for invalid/expired signatures
            let credentials = scx.check().await?;

            body_changed = scx.transformed_body.is_some() || scx.multipart.is_some();
            transformed_body = scx.transformed_body;

            req.s3ext.multipart = scx.multipart;
            req.s3ext.trailing_headers = scx.trailing_headers;

            match credentials {
                Some(cred) => {
                    req.s3ext.credentials = Some(Credentials {
                        access_key: cred.access_key,
                        secret_key: cred.secret_key,
                    });
                    req.s3ext.region = cred.region;
                    req.s3ext.service = cred.service;
                }
                None => {
                    req.s3ext.credentials = None;
                }
            }
        }

        if body_changed {
            // invalidate the original content length
            if let Some(val) = req.headers.get_mut(header::CONTENT_LENGTH) {
                *val = fmt_content_length(decoded_content_length.unwrap_or(0));
            }
            if let Some(val) = &mut content_length {
                *val = 0;
            }
        }
        if let Some(body) = transformed_body {
            req.body = body;
        }

        let has_multipart = req.s3ext.multipart.is_some();
        debug!(?body_changed, ?decoded_content_length, ?has_multipart);
    }

    // Core Logic: If Route is Custom Route, return Prepare::CustomRoute.
    if let Some(route) = ccx.route {
        if route.is_match(&req.method, &req.uri, &req.headers, &mut req.extensions) {
            return Ok(Prepare::CustomRoute);
        }
    }

    // Core Logic: If Route is S3 Route, resolve the route and determine if the operation needs the full body.
    let (op, needs_full_body) = 'resolve: {
        if let Some(multipart) = &mut req.s3ext.multipart {
            if req.method == Method::POST {
                match s3_path {
                    S3Path::Root => return Err(unknown_operation()),
                    S3Path::Bucket { .. } => {
                        // POST object
                        debug!(?multipart);
                        let file_stream = multipart.take_file_stream().expect("missing file stream");
                        let vec_bytes = aggregate_unlimited(file_stream).await.map_err(S3Error::internal_error)?;
                        let vec_stream = VecByteStream::new(vec_bytes);
                        req.s3ext.vec_stream = Some(vec_stream);
                        break 'resolve (&PutObject as &'static dyn Operation, false);
                    }
                    // FIXME: POST /bucket/key hits this branch
                    S3Path::Object { .. } => return Err(s3_error!(MethodNotAllowed)),
                }
            }
        }
        resolve_route(req, s3_path, req.s3ext.qs.as_ref())?
    };

    // FIXME: hack for E2E tests (minio/mint)
    if op.name() == "ListObjects" {
        if let Some(qs) = req.s3ext.qs.as_ref() {
            if qs.has("events") {
                return Err(s3_error!(NotImplemented, "listenBucketNotification only works on MinIO"));
            }
        }
    }

    debug!(op = %op.name(), ?s3_path, "resolved route");

    if ccx.auth.is_some() {
        let mut acx = S3AccessContext {
            credentials: req.s3ext.credentials.as_ref(),
            s3_path,
            s3_op: &crate::S3Operation { name: op.name() },
            method: &req.method,
            uri: &req.uri,
            headers: &req.headers,
            extensions: &mut req.extensions,
        };
        match ccx.access {
            Some(access) => access.check(&mut acx).await?,
            None => crate::access::default_check(&mut acx)?,
        }
    }

    debug!(op = %op.name(), ?s3_path, "checked access");

    if needs_full_body {
        extract_full_body(content_length, &mut req.body).await?;
    }

    Ok(Prepare::S3(op))
}
