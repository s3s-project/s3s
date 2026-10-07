// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

//! Authentication passthrough for requests the proxy cannot authenticate.
//!
//! The proxy authenticates clients against a static credential table. A
//! credential it does not know can only be verified by the backend: a user the
//! backend's admin API created, or the temporary credential an STS
//! `AssumeRole` call hands out. This module forwards such a request verbatim —
//! every header (including `authorization` and `host`), the body, and the
//! response — so the `SigV4` signature still verifies at the backend.
//!
//! Forwarding changes the trust model: a request that reaches this path is no
//! longer authenticated by the proxy. It is therefore opt-in, limited to request
//! shapes that are safe to forward, and it never logs credentials.

use s3s::Body;
use s3s::S3Response;

use hyper::HeaderMap;
use hyper::Method;
use hyper::StatusCode;
use hyper::Uri;
use hyper::body::{Bytes, Frame, Incoming, SizeHint};
use hyper::header::AUTHORIZATION;
use hyper::http::uri::PathAndQuery;

use std::future::Future;
use std::pin::Pin;
use std::task::Context;
use std::task::Poll;

use crate::hop_by_hop;

/// Query keys an object-level request may carry.
///
/// A key that is not listed denies the passthrough, so a bucket configuration
/// surface (`policy`, `acl`, `cors`, `versioning`, `tagging`, ...) is decided
/// by the proxy instead of being forwarded. `location` is a deliberate
/// exception: clients query the bucket location before object operations, and
/// the answer is read-only.
const ALLOWED_QUERY_KEYS: &[&str] = &[
    "continuation-token",
    "delete",
    "delimiter",
    "encoding-type",
    "fetch-owner",
    "list-type",
    "location",
    "marker",
    "max-keys",
    "partNumber",
    "prefix",
    "response-cache-control",
    "response-content-disposition",
    "response-content-encoding",
    "response-content-language",
    "response-content-type",
    "response-expires",
    "start-after",
    "uploadId",
    "uploads",
    "versionId",
    "x-id",
];

/// Forwards requests carrying a credential this proxy does not know.
#[derive(Debug, Clone)]
pub struct AuthPassthrough {
    /// Backend base URL (e.g. `http://localhost:9000`).
    endpoint_url: reqwest::Url,
    /// HTTP client used to forward requests.
    client: reqwest::Client,
    /// The access key the proxy authenticates locally.
    known_access_key: String,
    /// Extra query keys allowed on object-level requests.
    extra_query_keys: Vec<String>,
    /// Upper bound, in bytes, on a forwarded request body.
    max_body_size: u64,
}

impl AuthPassthrough {
    /// Creates a passthrough targeting `endpoint_url` that treats every access
    /// key other than `known_access_key` as unknown.
    #[must_use]
    pub fn new(
        endpoint_url: reqwest::Url,
        client: reqwest::Client,
        known_access_key: String,
        extra_query_keys: Vec<String>,
        max_body_size: u64,
    ) -> Self {
        Self {
            endpoint_url,
            client,
            known_access_key,
            extra_query_keys,
            max_body_size,
        }
    }

    /// Whether the request carries a credential this proxy does not know and a
    /// shape the passthrough may forward.
    #[must_use]
    pub fn is_candidate(&self, method: &Method, uri: &Uri, headers: &HeaderMap) -> bool {
        // An unsigned request is not a passthrough candidate: it stays on the
        // typed path, where the proxy rejects it as it does today.
        let Some(access_key) = access_key(uri, headers) else {
            return false;
        };
        // A credential the proxy knows stays on the typed path, so the proxy
        // keeps deciding what it can decide.
        if access_key == self.known_access_key {
            return false;
        }
        if !is_object_level_request(method, uri, &self.extra_query_keys) {
            // A shape the passthrough does not forward is worth a line: the
            // request then fails locally with NotSignedUp, and the shape is what
            // has to be added to the allowlist. Only names are recorded.
            tracing::debug!(
                method = %method,
                path = %uri.path(),
                query_keys = ?query_keys(uri),
                "unknown credentials on a shape the passthrough does not forward; the request stays on the typed path"
            );
            return false;
        }
        true
    }

    /// Whether the service request is a passthrough candidate.
    #[must_use]
    pub fn is_match(&self, req: &hyper::Request<Incoming>) -> bool {
        self.is_candidate(req.method(), req.uri(), req.headers())
    }

    /// Forwards the request verbatim and returns the backend response.
    pub async fn forward(&self, req: hyper::Request<Incoming>) -> s3s::HttpResponse {
        let (parts, incoming) = req.into_parts();
        let response = forward_verbatim(
            &self.client,
            &self.endpoint_url,
            &parts.method,
            &parts.uri,
            &parts.headers,
            Body::from(incoming),
            self.max_body_size,
        )
        .await;

        into_http(response)
    }
}

/// Forwards a request to `endpoint_url` without changing it.
///
/// Every header is copied — including `authorization` and `host`, which the
/// signature covers — the body is sent with the bytes it arrived with (the
/// signature covers the payload), and the response is returned as it arrived.
pub(crate) async fn forward_verbatim(
    client: &reqwest::Client,
    endpoint_url: &reqwest::Url,
    method: &Method,
    uri: &Uri,
    headers: &HeaderMap,
    mut body: Body,
    max_body_size: u64,
) -> S3Response<Body> {
    let Some(target) = backend_url(endpoint_url, uri.path_and_query().map(PathAndQuery::as_str)) else {
        return error_response(StatusCode::BAD_GATEWAY, "InternalError", "invalid passthrough request URI");
    };

    // The bytes must not change, and they must not be read without a bound. A
    // body that is already in memory is measured without being copied; a
    // streaming body enforces the bound while it is read.
    let too_large = || {
        error_response(
            StatusCode::BAD_REQUEST,
            "EntityTooLarge",
            "The request body exceeds the configured maximum size.",
        )
    };
    let bytes = match body.bytes() {
        Some(bytes) if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > max_body_size => return too_large(),
        Some(bytes) => bytes,
        None => {
            body.set_limit(Some(max_body_size));
            match body.store_all_limited(usize::MAX).await {
                Ok(bytes) => bytes,
                Err(err) if err.is::<s3s::BodySizeLimitExceeded>() => return too_large(),
                Err(err) => {
                    tracing::warn!(category = body_error_category(&err), "passthrough request body could not be read");
                    return error_response(StatusCode::BAD_GATEWAY, "InternalError", "failed to read the request body");
                }
            }
        }
    };

    send_to_backend(client, method, target, headers, bytes.into(), || {
        passthrough_log_line(method, uri, endpoint_url)
    })
    .await
}

/// Turns a backend answer into the proxy's answer.
///
/// Both passthroughs return the backend's status, headers and streamed body
/// unchanged, so the conversion lives in one place and the two cannot drift
/// apart.
#[must_use]
pub(crate) fn into_http(response: S3Response<Body>) -> s3s::HttpResponse {
    let mut out = s3s::HttpResponse::new(response.output);
    if let Some(status) = response.status {
        *out.status_mut() = status;
    }
    *out.headers_mut() = response.headers;
    out
}

/// Sends a prepared request to the backend and returns its streamed answer.
///
/// The caller decides what the body is. The authentication passthrough hands
/// over bytes it had to read, because the signature it forwards covers the
/// payload hash; the POST Object passthrough hands over the stream it is still
/// receiving, because a form upload declares its own length and reading it
/// first would hold the whole body in memory.
///
/// The log line arrives as a closure: every forwarded request takes this path,
/// and the line is built only when it is recorded, so a disabled log level costs
/// nothing.
pub(crate) async fn send_to_backend(
    client: &reqwest::Client,
    method: &Method,
    target: reqwest::Url,
    headers: &HeaderMap,
    body: reqwest::Body,
    log_line: impl FnOnce() -> String,
) -> S3Response<Body> {
    let mut request = client.request(method.clone(), target);
    for (name, value) in headers {
        // Connection-specific headers are dropped (RFC 9110 section 7.6.1); every
        // other header, including the ones the signature covers, is forwarded
        // unchanged.
        if hop_by_hop::is_connection_specific(headers, name.as_str()) {
            continue;
        }
        request = request.header(name, value);
    }
    let request = match request.body(body).build() {
        Ok(request) => request,
        Err(err) => {
            tracing::warn!(category = transport_error_category(&err), "passthrough request could not be built");
            return error_response(StatusCode::BAD_GATEWAY, "InternalError", "failed to build the forwarded request");
        }
    };

    tracing::debug!("{}", log_line());

    let response = match client.execute(request).await {
        Ok(response) => response,
        Err(err) => {
            tracing::warn!(category = transport_error_category(&err), "backend is unreachable");
            return error_response(StatusCode::BAD_GATEWAY, "InternalError", "failed to reach the backend");
        }
    };
    let status = response.status();
    // The backend's response headers make the same hop as the request headers:
    // the connection-specific ones stop here (RFC 9110 section 7.6.1), exactly as
    // they do on the way out. `content-length` is not one of them, and it is what
    // frames the streamed body.
    let headers = hop_by_hop::without_connection_specific(response.headers());

    // The response is streamed: a large object must not be buffered in the proxy,
    // and the client starts receiving bytes while the backend is still producing
    // them. Once the head is on the wire the status cannot change, so a failure
    // mid-stream can only truncate the body, which the stream reports.
    let mut out = S3Response::new(Body::http_body_unsync(BackendResponseStream::new(response)));
    out.status = Some(status);
    out.headers = headers;
    out
}

/// One in-flight `Response::chunk()` call: it owns the response and hands it back.
type ChunkFuture = Pin<Box<dyn Future<Output = (reqwest::Response, Result<Option<Bytes>, reqwest::Error>)> + Send>>;

/// Streams a backend response body chunk by chunk.
///
/// `reqwest` can only produce chunks through `Response::chunk()`, which borrows
/// the response mutably. Each call is therefore driven as a boxed future that
/// owns the response, and the response is handed back when the call resolves.
struct BackendResponseStream {
    /// The response; absent while a `chunk()` call owns it.
    response: Option<reqwest::Response>,
    /// The in-flight `chunk()` call.
    pending: Option<ChunkFuture>,
    /// Bytes already yielded, so `size_hint` reports what is left.
    sent: u64,
    finished: bool,
}

impl BackendResponseStream {
    fn new(response: reqwest::Response) -> Self {
        Self {
            response: Some(response),
            pending: None,
            sent: 0,
            finished: false,
        }
    }
}

impl hyper::body::Body for BackendResponseStream {
    type Data = Bytes;
    type Error = s3s::StdError;

    fn poll_frame(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        let this = self.get_mut();
        if this.finished {
            return Poll::Ready(None);
        }
        if this.pending.is_none() {
            let Some(mut response) = this.response.take() else {
                this.finished = true;
                return Poll::Ready(None);
            };
            this.pending = Some(Box::pin(async move {
                let chunk = response.chunk().await;
                (response, chunk)
            }));
        }

        let Some(pending) = this.pending.as_mut() else {
            this.finished = true;
            return Poll::Ready(None);
        };
        match pending.as_mut().poll(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready((response, result)) => {
                this.pending = None;
                match result {
                    Ok(Some(chunk)) => {
                        this.sent += chunk.len() as u64;
                        this.response = Some(response);
                        Poll::Ready(Some(Ok(Frame::data(chunk))))
                    }
                    Ok(None) => {
                        this.finished = true;
                        Poll::Ready(None)
                    }
                    Err(err) => {
                        this.finished = true;
                        // The head is already on the wire, so this can only
                        // truncate the body. Only the category is recorded.
                        tracing::warn!(category = transport_error_category(&err), "passthrough response stream failed");
                        Poll::Ready(Some(Err(Box::new(err))))
                    }
                }
            }
        }
    }

    fn size_hint(&self) -> SizeHint {
        let mut hint = SizeHint::new();
        if let Some(total) = self.response.as_ref().and_then(reqwest::Response::content_length) {
            hint.set_exact(total.saturating_sub(self.sent));
        }
        hint
    }
}

/// The category of a transport failure.
///
/// Never the message: a `reqwest` error carries the request URL, and a presigned
/// URL contains the credential and the signature.
#[must_use]
fn transport_error_category(err: &reqwest::Error) -> &'static str {
    if err.is_timeout() {
        "timeout"
    } else if err.is_connect() {
        "connect"
    } else if err.is_body() || err.is_decode() {
        "body"
    } else if err.is_request() {
        "request"
    } else {
        "other"
    }
}

/// The category of a request-body read failure.
///
/// The message is not recorded for the same reason as
/// [`transport_error_category`].
#[must_use]
fn body_error_category(err: &s3s::StdError) -> &'static str {
    if err.downcast_ref::<s3s::BodySizeLimitExceeded>().is_some() {
        "too_large"
    } else if err.downcast_ref::<hyper::Error>().is_some_and(hyper::Error::is_timeout) {
        "timeout"
    } else if err
        .downcast_ref::<hyper::Error>()
        .is_some_and(|err| err.is_incomplete_message() || err.is_body_write_aborted())
    {
        "body"
    } else if err.downcast_ref::<hyper::Error>().is_some_and(hyper::Error::is_parse) {
        "parse"
    } else {
        "other"
    }
}

/// Builds the backend URL for a request path, preserving the path and query
/// verbatim: re-encoding either of them would invalidate the signature.
#[must_use]
pub(crate) fn backend_url(endpoint_url: &reqwest::Url, path_and_query: Option<&str>) -> Option<reqwest::Url> {
    let path_and_query = path_and_query?;
    let base = endpoint_url.as_str().trim_end_matches('/');
    reqwest::Url::parse(&format!("{base}{path_and_query}")).ok()
}

/// The access key a request is signed with, when it carries one.
///
/// Both mechanisms are recognised: the `Credential` field of a `SigV4`
/// `authorization` header (or `X-Amz-Credential` in a presigned URL), and the
/// `SigV2` forms. An `authorization` header this function does not recognise
/// yields `None`, so an ambiguous request stays on the typed path.
#[must_use]
fn access_key(uri: &Uri, headers: &HeaderMap) -> Option<String> {
    if let Some(value) = headers.get(AUTHORIZATION).and_then(|value| value.to_str().ok()) {
        if let Some(rest) = value.strip_prefix("AWS4-HMAC-SHA256 ") {
            let credential = rest.split(',').find_map(|part| part.trim().strip_prefix("Credential="))?;
            return credential.split('/').next().map(str::to_owned);
        }
        if let Some(rest) = value.strip_prefix("AWS ") {
            return rest.split(':').next().map(str::to_owned);
        }
        return None;
    }

    for (key, value) in query_pairs(uri) {
        if key == "X-Amz-Credential" {
            return value.split('/').next().map(str::to_owned);
        }
        if key == "AWSAccessKeyId" {
            return Some(value);
        }
    }
    None
}

/// The query parameters of a request, percent-decoded.
#[must_use]
fn query_pairs(uri: &Uri) -> Vec<(String, String)> {
    let Some(path_and_query) = uri.path_and_query() else {
        return Vec::new();
    };
    let Ok(url) = reqwest::Url::parse(&format!("http://placeholder{path_and_query}")) else {
        return Vec::new();
    };
    url.query_pairs()
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .collect()
}

/// The query parameter names of a request, sorted and deduplicated.
#[must_use]
fn query_keys(uri: &Uri) -> Vec<String> {
    let mut keys: Vec<String> = query_pairs(uri).into_iter().map(|(key, _value)| key).collect();
    keys.sort();
    keys.dedup();
    keys
}

/// Whether `method` and `uri` describe an object-level request that may be
/// forwarded verbatim.
///
/// A `POST` is one only when it carries `?delete`; every other `POST` is
/// either `POST Object`, whose multipart form the typed path has to parse, or
/// the STS protocol shape, which has its own route. A `PUT` or `DELETE` must
/// name an object inside a bucket, because on a bucket those methods address the
/// bucket itself (create, delete). The path is what decides, so a
/// virtual-hosted-style request, which carries the bucket in `Host` and only the
/// key in the path, is not forwarded for either method. Paths under `/minio/` are
/// the admin, health and metrics surfaces, which the proxy already forwards
/// through their own handlers.
#[must_use]
fn is_object_level_request(method: &Method, uri: &Uri, extra_query_keys: &[String]) -> bool {
    // Bulk delete (`POST` with `?delete`) is an object operation; every other
    // `POST` is either `POST Object` or the STS protocol shape, which have their
    // own handlers.
    let supported = if method == Method::POST {
        query_keys(uri).iter().any(|key| key == "delete")
    } else {
        method == Method::GET || method == Method::HEAD || method == Method::PUT || method == Method::DELETE
    };
    if !supported {
        return false;
    }

    let path = uri.path();
    if path.starts_with("/minio/") {
        return false;
    }
    // A bucket must be named; the key may be absent (a bucket-level listing).
    let Some(bucket_and_key) = path.strip_prefix('/').filter(|rest| !rest.is_empty()) else {
        return false;
    };
    // Creating or deleting a bucket is not an object operation, and neither is a
    // path that stops at the bucket separator.
    if (method == Method::PUT || method == Method::DELETE)
        && bucket_and_key.split_once('/').is_none_or(|(_bucket, key)| key.is_empty())
    {
        return false;
    }

    for key in query_keys(uri) {
        let allowed = ALLOWED_QUERY_KEYS.contains(&key.as_str()) || extra_query_keys.iter().any(|extra| extra == &key);
        if !allowed {
            return false;
        }
    }
    true
}

/// The log line for a forwarded request.
///
/// Only the method, the path, the query parameter **names** and the backend
/// authority are recorded: the values may carry a credential or a signature, and
/// header values are never logged at all.
#[must_use]
fn passthrough_log_line(method: &Method, uri: &Uri, endpoint_url: &reqwest::Url) -> String {
    let host = endpoint_url.host_str().unwrap_or_default();
    let port = endpoint_url.port().map_or_else(String::new, |port| format!(":{port}"));
    format!(
        "forwarding passthrough request: method={method} path={} query_keys={:?} backend={host}{port}",
        uri.path(),
        query_keys(uri)
    )
}

/// Builds an error response for a failure inside the passthrough.
#[must_use]
pub(crate) fn error_response(status: StatusCode, code: &str, message: &str) -> S3Response<Body> {
    let body =
        format!("<?xml version=\"1.0\" encoding=\"UTF-8\"?><Error><Code>{code}</Code><Message>{message}</Message></Error>");
    let mut out = S3Response::new(Body::from(body));
    out.status = Some(status);
    out.headers
        .insert(hyper::header::CONTENT_TYPE, hyper::header::HeaderValue::from_static("application/xml"));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::net::SocketAddr;
    use std::sync::Arc;
    use std::sync::Mutex;

    use tokio::io::AsyncReadExt as _;
    use tokio::io::AsyncWriteExt as _;
    use tokio::net::TcpListener;

    /// Serves exactly one request: records its head and body, answers 200.
    async fn serve_one() -> (SocketAddr, Arc<Mutex<String>>, Arc<Mutex<Vec<u8>>>, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("local addr");
        let head = Arc::new(Mutex::new(String::new()));
        let body = Arc::new(Mutex::new(Vec::new()));
        let seen_head = Arc::clone(&head);
        let seen_body = Arc::clone(&body);

        let handle = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept");
            let mut buf = Vec::new();
            let mut chunk = [0_u8; 4096];
            let head_end = loop {
                if let Some(pos) = buf.windows(4).position(|window| window == b"\r\n\r\n") {
                    break pos + 4;
                }
                let read = socket.read(&mut chunk).await.expect("read");
                assert!(read > 0, "connection closed before the request head");
                buf.extend_from_slice(&chunk[..read]);
            };
            let text = String::from_utf8_lossy(&buf[..head_end]).into_owned();
            let length: usize = text
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    if name.eq_ignore_ascii_case("content-length") {
                        value.trim().parse().ok()
                    } else {
                        None
                    }
                })
                .unwrap_or(0);
            let mut received = buf[head_end..].to_vec();
            while received.len() < length {
                let read = socket.read(&mut chunk).await.expect("read");
                if read == 0 {
                    break;
                }
                received.extend_from_slice(&chunk[..read]);
            }
            *seen_head.lock().expect("lock") = text;
            *seen_body.lock().expect("lock") = received;
            socket
                .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\nx-backend: yes\r\n\r\nok")
                .await
                .expect("write response");
        });

        (addr, head, body, handle)
    }

    /// Serves one request with a raw response head and body.
    async fn serve_raw(head: &'static str, body: &'static [u8]) -> (SocketAddr, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("local addr");

        let handle = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept");
            let mut buf = Vec::new();
            let mut chunk = [0_u8; 4096];
            loop {
                if buf.windows(4).any(|window| window == b"\r\n\r\n") {
                    break;
                }
                let read = socket.read(&mut chunk).await.expect("read");
                assert!(read > 0, "connection closed before the request head");
                buf.extend_from_slice(&chunk[..read]);
            }
            socket.write_all(head.as_bytes()).await.expect("write head");
            socket.write_all(body).await.expect("write body");
            socket.flush().await.expect("flush");
        });

        (addr, handle)
    }

    /// Serves one request with a known body.
    ///
    /// When `staged` is set, the first `first` bytes are flushed before the
    /// rest, and the rest only after the returned signal fires: a caller can then
    /// prove it received a frame while the backend was still holding the tail.
    async fn serve_known_body(
        body: Vec<u8>,
        first: usize,
        staged: bool,
    ) -> (SocketAddr, tokio::sync::oneshot::Sender<()>, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("local addr");
        let (release, gate) = tokio::sync::oneshot::channel();

        let handle = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept");
            let mut buf = Vec::new();
            let mut chunk = [0_u8; 4096];
            loop {
                if buf.windows(4).any(|window| window == b"\r\n\r\n") {
                    break;
                }
                let read = socket.read(&mut chunk).await.expect("read");
                assert!(read > 0, "connection closed before the request head");
                buf.extend_from_slice(&chunk[..read]);
            }

            let head = format!("HTTP/1.1 200 OK\r\ncontent-length: {}\r\n\r\n", body.len());
            socket.write_all(head.as_bytes()).await.expect("write head");
            socket.write_all(&body[..first]).await.expect("write the first part");
            socket.flush().await.expect("flush");
            if staged {
                gate.await.expect("the test released the tail");
            }
            socket.write_all(&body[first..]).await.expect("write the rest");
            socket.flush().await.expect("flush");
        });

        (addr, release, handle)
    }

    /// Reads the next data frame of a response body.
    async fn next_frame(body: &mut Body) -> Result<Option<Bytes>, s3s::StdError> {
        let frame = std::future::poll_fn(|cx| hyper::body::Body::poll_frame(Pin::new(&mut *body), cx)).await;
        match frame {
            None => Ok(None),
            Some(Ok(frame)) => Ok(Some(frame.into_data().expect("the passthrough yields data frames"))),
            Some(Err(err)) => Err(err),
        }
    }

    fn request_uri(value: &str) -> Uri {
        value.parse().expect("uri")
    }

    fn auth_header(access_key: &str) -> hyper::header::HeaderValue {
        format!("AWS4-HMAC-SHA256 Credential={access_key}/20261004/us-east-1/s3/aws4_request, SignedHeaders=host, Signature=abc")
            .parse()
            .expect("header")
    }

    #[test]
    fn reads_the_access_key_from_both_mechanisms() {
        let uri = Uri::from_static("/bucket/key");
        let mut headers = HeaderMap::new();
        headers.insert(AUTHORIZATION, auth_header("AKIAHEADER"));
        assert_eq!(access_key(&uri, &headers).as_deref(), Some("AKIAHEADER"));

        let mut headers = HeaderMap::new();
        headers.insert(AUTHORIZATION, "AWS AKIASIGV2:signature".parse().expect("header"));
        assert_eq!(access_key(&uri, &headers).as_deref(), Some("AKIASIGV2"));

        let presigned = request_uri(
            "/bucket/key?X-Amz-Credential=AKIAPRESIGNED%2F20261004%2Fus-east-1%2Fs3%2Faws4_request&X-Amz-Signature=deadbeef",
        );
        assert_eq!(access_key(&presigned, &HeaderMap::new()).as_deref(), Some("AKIAPRESIGNED"));

        let v2 = request_uri("/bucket/key?AWSAccessKeyId=AKIAV2&Signature=abc");
        assert_eq!(access_key(&v2, &HeaderMap::new()).as_deref(), Some("AKIAV2"));

        assert_eq!(access_key(&uri, &HeaderMap::new()), None);
        let mut headers = HeaderMap::new();
        headers.insert(AUTHORIZATION, "Bearer token".parse().expect("header"));
        assert_eq!(access_key(&uri, &headers), None);
    }

    #[test]
    fn allows_object_level_shapes() {
        for uri in [
            "/bucket/key",
            "/bucket/key?x-id=PutObject",
            "/bucket/key?versionId=abc",
            "/bucket?list-type=2&prefix=a&max-keys=10",
            "/bucket?uploads",
            "/bucket/key?uploadId=abc&partNumber=1",
            "/bucket?location=",
            "/bucket?delete",
        ] {
            assert!(is_object_level_request(&Method::GET, &request_uri(uri), &[]), "{uri} should be forwarded");
        }
        // Bulk delete is a POST on the bucket; it is an object operation.
        assert!(
            is_object_level_request(&Method::POST, &request_uri("/bucket?delete"), &[]),
            "POST with ?delete is the bulk delete shape"
        );
        // PUT and DELETE are object operations when they name an object.
        for method in [Method::PUT, Method::DELETE] {
            assert!(
                is_object_level_request(&method, &request_uri("/bucket/key"), &[]),
                "{method} /bucket/key should be forwarded"
            );
        }
    }

    #[test]
    fn denies_configuration_and_unknown_shapes() {
        for uri in [
            "/bucket?policy",
            "/bucket?acl",
            "/bucket?cors",
            "/bucket?versioning",
            "/bucket?tagging",
            "/bucket?encryption",
            "/bucket?lifecycle",
            "/bucket?notification",
            "/bucket/key?unknown-key",
            "/minio/admin/v3/info",
            "/",
        ] {
            assert!(
                !is_object_level_request(&Method::GET, &request_uri(uri), &[]),
                "{uri} should stay on the typed path"
            );
        }
        assert!(!is_object_level_request(&Method::POST, &request_uri("/bucket"), &[]));
        assert!(!is_object_level_request(&Method::PATCH, &request_uri("/bucket/key"), &[]));
        // Creating or deleting a bucket is not an object operation, and neither is
        // a path that stops at the separator.
        for method in [Method::PUT, Method::DELETE] {
            for uri in ["/bucket", "/bucket/"] {
                assert!(
                    !is_object_level_request(&method, &request_uri(uri), &[]),
                    "{method} {uri} should stay on the typed path"
                );
            }
        }
    }

    #[test]
    fn extra_query_keys_extend_the_allowlist() {
        assert!(!is_object_level_request(&Method::GET, &request_uri("/bucket?extra-key"), &[]));
        assert!(is_object_level_request(
            &Method::GET,
            &request_uri("/bucket?extra-key"),
            &["extra-key".to_owned()]
        ));
    }

    #[test]
    fn candidates_are_unknown_credentials_with_an_allowed_shape() {
        let passthrough = AuthPassthrough::new(
            reqwest::Url::parse("http://localhost:9000").expect("url"),
            reqwest::Client::new(),
            "AKIAKNOWN".to_owned(),
            Vec::new(),
            1024,
        );

        let uri = request_uri("/bucket/key");
        let mut unknown = HeaderMap::new();
        unknown.insert(AUTHORIZATION, auth_header("AKIAUNKNOWN"));
        assert!(passthrough.is_candidate(&Method::PUT, &uri, &unknown));

        let mut known = HeaderMap::new();
        known.insert(AUTHORIZATION, auth_header("AKIAKNOWN"));
        assert!(!passthrough.is_candidate(&Method::PUT, &uri, &known), "known credentials stay typed");

        assert!(
            !passthrough.is_candidate(&Method::PUT, &uri, &HeaderMap::new()),
            "an unsigned request stays typed"
        );

        let configuration = request_uri("/bucket?policy");
        assert!(
            !passthrough.is_candidate(&Method::GET, &configuration, &unknown),
            "a configuration shape stays typed"
        );
    }

    #[test]
    fn the_log_line_hides_credentials() {
        let uri = request_uri(
            "/bucket/key?X-Amz-Credential=AKIAUNKNOWN%2F20261004%2Fus-east-1%2Fs3%2Faws4_request&X-Amz-Signature=deadbeefcafe&x-id=GetObject",
        );
        let endpoint = reqwest::Url::parse("http://localhost:9000").expect("url");
        let line = passthrough_log_line(&Method::GET, &uri, &endpoint);

        assert!(line.contains("method=GET"), "{line}");
        assert!(line.contains("path=/bucket/key"), "{line}");
        assert!(line.contains("X-Amz-Credential"), "the parameter names are recorded: {line}");
        assert!(!line.contains("AKIAUNKNOWN"), "the access key must not be logged: {line}");
        assert!(!line.contains("deadbeefcafe"), "the signature must not be logged: {line}");
        assert!(!line.contains("20261004"), "the credential scope must not be logged: {line}");
        assert!(line.contains("backend=localhost:9000"), "{line}");
    }

    #[tokio::test]
    async fn forwards_headers_and_body_verbatim() {
        let (addr, head, body, server) = serve_one().await;
        let client = reqwest::Client::new();
        let endpoint = reqwest::Url::parse(&format!("http://{addr}")).expect("url");

        let authorization = "AWS4-HMAC-SHA256 Credential=AKIAUNKNOWN/20261004/us-east-1/s3/aws4_request, SignedHeaders=host;x-amz-security-token, Signature=abc";
        let mut headers = HeaderMap::new();
        headers.insert(AUTHORIZATION, authorization.parse().expect("header"));
        headers.insert("host", addr.to_string().parse().expect("header"));
        headers.insert("content-length", "11".parse().expect("header"));
        headers.insert("x-amz-security-token", "SESSION-TOKEN".parse().expect("header"));

        let uri = request_uri("/bucket/key?x-id=PutObject");
        let mut response = forward_verbatim(
            &client,
            &endpoint,
            &Method::PUT,
            &uri,
            &headers,
            Body::from(b"hello world".to_vec()),
            1024,
        )
        .await;

        server.await.expect("server task");
        let head = head.lock().expect("lock").to_ascii_lowercase();
        assert!(head.starts_with("put /bucket/key?x-id=putobject "), "{head}");
        assert!(
            head.contains(&format!("authorization: {}", authorization.to_ascii_lowercase())),
            "the signature header must arrive byte for byte: {head}"
        );
        assert!(head.contains("x-amz-security-token: session-token"), "{head}");
        assert_eq!(head.matches("content-length: 11").count(), 1, "{head}");
        assert_eq!(body.lock().expect("lock").as_slice(), b"hello world");

        assert_eq!(response.status, Some(StatusCode::OK));
        assert_eq!(response.headers.get("x-backend").and_then(|value| value.to_str().ok()), Some("yes"));
        // The response body is streamed, so it is collected instead of read from
        // a buffer; the bytes are the ones the backend sent.
        let out = response.output.store_all_limited(usize::MAX).await.expect("collect the body");
        assert_eq!(out.as_ref(), b"ok".as_slice());
    }

    #[tokio::test]
    async fn a_large_response_is_streamed_byte_for_byte() {
        // Larger than any internal chunk, so the body arrives in many frames.
        const SIZE: usize = 8 * 1024 * 1024;
        let body: Vec<u8> = (0..SIZE).map(|index| u8::try_from(index % 251).expect("in range")).collect();
        let (addr, _release, server) = serve_known_body(body.clone(), SIZE, false).await;

        let client = reqwest::Client::new();
        let endpoint = reqwest::Url::parse(&format!("http://{addr}")).expect("url");
        let mut response = forward_verbatim(
            &client,
            &endpoint,
            &Method::GET,
            &request_uri("/bucket/key"),
            &HeaderMap::new(),
            Body::empty(),
            1024,
        )
        .await;

        assert_eq!(response.status, Some(StatusCode::OK));
        assert_eq!(
            response.headers.get("content-length").and_then(|value| value.to_str().ok()),
            Some(SIZE.to_string().as_str())
        );

        let received = response.output.store_all_limited(usize::MAX).await.expect("collect the body");
        assert_eq!(received.len(), SIZE);
        assert_eq!(received.as_ref(), body.as_slice());
        server.await.expect("server task");
    }

    #[tokio::test]
    async fn a_response_chunk_reaches_the_client_before_the_backend_finishes() {
        const SIZE: usize = 64 * 1024;
        const FIRST: usize = 16;
        let (addr, release, server) = serve_known_body(vec![b'z'; SIZE], FIRST, true).await;

        let client = reqwest::Client::new();
        let endpoint = reqwest::Url::parse(&format!("http://{addr}")).expect("url");
        let mut response = forward_verbatim(
            &client,
            &endpoint,
            &Method::GET,
            &request_uri("/bucket/key"),
            &HeaderMap::new(),
            Body::empty(),
            1024,
        )
        .await;

        // A buffering implementation would block here until the backend is
        // released below.
        let frame = tokio::time::timeout(std::time::Duration::from_secs(10), next_frame(&mut response.output))
            .await
            .expect("the first frame must not wait for the whole response")
            .expect("the frame read succeeds")
            .expect("a frame arrives");
        assert_eq!(frame.len(), FIRST);

        release.send(()).expect("the backend is waiting for the tail");
        let rest = response.output.store_all_limited(usize::MAX).await.expect("collect the rest");
        assert_eq!(rest.len(), SIZE - FIRST);
        server.await.expect("server task");
    }

    #[tokio::test]
    async fn connection_specific_headers_are_not_forwarded() {
        let (addr, head, _body, server) = serve_one().await;
        let client = reqwest::Client::new();
        let endpoint = reqwest::Url::parse(&format!("http://{addr}")).expect("url");

        let mut headers = HeaderMap::new();
        headers.insert("connection", "x-drop-me".parse().expect("header"));
        headers.insert("x-drop-me", "1".parse().expect("header"));
        headers.insert("transfer-encoding", "chunked".parse().expect("header"));
        headers.insert("x-keep-me", "1".parse().expect("header"));

        let response = forward_verbatim(
            &client,
            &endpoint,
            &Method::GET,
            &request_uri("/bucket/key"),
            &headers,
            Body::empty(),
            1024,
        )
        .await;

        server.await.expect("server task");
        let head = head.lock().expect("lock").to_ascii_lowercase();
        assert!(!head.contains("x-drop-me"), "a header the connection names must not be forwarded: {head}");
        assert!(!head.contains("transfer-encoding"), "framing is managed locally: {head}");
        assert!(
            !head.contains("connection: x-drop-me"),
            "the connection header must not be forwarded: {head}"
        );
        assert!(head.contains("x-keep-me: 1"), "an end-to-end header is forwarded: {head}");
        assert_eq!(response.status, Some(StatusCode::OK));
    }

    #[tokio::test]
    async fn connection_specific_response_headers_are_not_forwarded() {
        // A response may not carry both a length and chunked framing, so the
        // transfer-encoding case has a test of its own below.
        let (addr, server) = serve_raw(
            "HTTP/1.1 200 OK\r\ncontent-length: 2\r\nx-backend: yes\r\nconnection: x-foo\r\nx-foo: 1\r\nkeep-alive: timeout=5\r\n\r\n",
            b"ok",
        )
        .await;

        let client = reqwest::Client::new();
        let endpoint = reqwest::Url::parse(&format!("http://{addr}")).expect("url");
        let mut response = forward_verbatim(
            &client,
            &endpoint,
            &Method::GET,
            &request_uri("/bucket/key"),
            &HeaderMap::new(),
            Body::empty(),
            1024,
        )
        .await;

        server.await.expect("server task");
        assert_eq!(response.status, Some(StatusCode::OK));
        assert_eq!(
            response.headers.get("content-length").and_then(|value| value.to_str().ok()),
            Some("2"),
            "the length that frames the streamed body is kept"
        );
        assert_eq!(
            response.headers.get("x-backend").and_then(|value| value.to_str().ok()),
            Some("yes"),
            "an end-to-end header is kept"
        );
        for name in ["connection", "x-foo", "keep-alive", "transfer-encoding"] {
            assert!(
                response.headers.get(name).is_none(),
                "{name} must not be forwarded to the client: {:?}",
                response.headers
            );
        }

        let body = response.output.store_all_limited(usize::MAX).await.expect("collect the body");
        assert_eq!(body.as_ref(), b"ok".as_slice());
    }

    #[tokio::test]
    async fn a_chunked_backend_response_is_reframed_for_the_client() {
        let (addr, server) = serve_raw(
            "HTTP/1.1 200 OK\r\ntransfer-encoding: chunked\r\nx-backend: yes\r\n\r\n2\r\nok\r\n0\r\n\r\n",
            b"",
        )
        .await;

        let client = reqwest::Client::new();
        let endpoint = reqwest::Url::parse(&format!("http://{addr}")).expect("url");
        let mut response = forward_verbatim(
            &client,
            &endpoint,
            &Method::GET,
            &request_uri("/bucket/key"),
            &HeaderMap::new(),
            Body::empty(),
            1024,
        )
        .await;

        server.await.expect("server task");
        assert_eq!(response.status, Some(StatusCode::OK));
        assert!(
            response.headers.get("transfer-encoding").is_none(),
            "framing is managed locally: {:?}",
            response.headers
        );
        assert_eq!(response.headers.get("x-backend").and_then(|value| value.to_str().ok()), Some("yes"));

        let body = response.output.store_all_limited(usize::MAX).await.expect("collect the body");
        assert_eq!(body.as_ref(), b"ok".as_slice());
    }

    #[tokio::test]
    async fn a_body_over_the_bound_is_a_client_error() {
        let (addr, _head, _body, _server) = serve_one().await;
        let client = reqwest::Client::new();
        let endpoint = reqwest::Url::parse(&format!("http://{addr}")).expect("url");

        let response = forward_verbatim(
            &client,
            &endpoint,
            &Method::PUT,
            &request_uri("/bucket/key"),
            &HeaderMap::new(),
            Body::from(vec![0_u8; 64]),
            8,
        )
        .await;

        assert_eq!(response.status, Some(StatusCode::BAD_REQUEST));
        let body = String::from_utf8(response.output.bytes().expect("buffered body").to_vec()).expect("utf-8");
        assert!(body.contains("<Code>EntityTooLarge</Code>"), "{body}");
    }

    #[tokio::test]
    async fn an_unreachable_backend_is_a_bad_gateway() {
        // Bind and drop, so the port has no listener.
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("local addr");
        drop(listener);

        let client = reqwest::Client::new();
        let endpoint = reqwest::Url::parse(&format!("http://{addr}")).expect("url");
        let response = forward_verbatim(
            &client,
            &endpoint,
            &Method::GET,
            &request_uri("/bucket/key"),
            &HeaderMap::new(),
            Body::empty(),
            1024,
        )
        .await;

        assert_eq!(response.status, Some(StatusCode::BAD_GATEWAY));
    }

    #[test]
    fn an_answer_keeps_its_status_headers_and_body() {
        use hyper::header::HeaderValue;

        let mut response = S3Response::new(Body::from(b"ok".to_vec()));
        response.status = Some(StatusCode::CREATED);
        let mut headers = HeaderMap::new();
        headers.insert("x-backend", HeaderValue::from_static("yes"));
        response.headers = headers;

        let http = into_http(response);
        assert_eq!(http.status(), StatusCode::CREATED, "the backend's status is the proxy's");
        assert_eq!(
            http.headers().get("x-backend").and_then(|value| value.to_str().ok()),
            Some("yes"),
            "the backend's headers are the proxy's"
        );
        let body = http.into_body().bytes().expect("the test body is in memory");
        assert_eq!(body.as_ref(), b"ok");

        // Without a status the proxy's own default stands.
        let default = into_http(S3Response::new(Body::from(b"ok".to_vec())));
        assert_eq!(default.status(), StatusCode::OK);
    }
}
