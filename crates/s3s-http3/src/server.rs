// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

use bytes::Bytes;
use h3::error::Code;
use h3::server::{RequestResolver, RequestStream};
use http::{HeaderMap, HeaderName, Request, Response, Version, header};
use http_body::Body as HttpBody;
use quinn::{Endpoint, Incoming, VarInt};
use s3s::service::S3Service;

use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use tower::{Service, ServiceExt};
use tracing::{debug, error, warn};

use std::future::Future;
use std::net::SocketAddr;
use std::time::Duration;

use crate::RequestBody;
use crate::body::Body;

/// Maximum time allowed for HTTP/3 connections to drain and close during shutdown.
pub const DEFAULT_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(10);

type Resolver = RequestResolver<h3_quinn::Connection, Bytes>;
type SendStream = RequestStream<h3_quinn::SendStream<Bytes>, Bytes>;

/// Serves HTTP/3 requests on a configured QUIC [`Endpoint`].
///
/// The endpoint must already be configured with TLS 1.3 and the `h3` ALPN
/// protocol. After each QUIC handshake, `make_service` is called with the
/// peer's [`SocketAddr`] and its result is used for that connection. The
/// service is cloned for concurrently handled requests on the connection.
///
/// Requests are passed to the service with a streaming [`RequestBody`].
/// Response data and trailers are streamed to the client without buffering;
/// HTTP/3-forbidden hop-by-hop headers are removed from responses.
///
/// The service is checked for readiness before each request. Service errors
/// and response-body errors terminate the affected HTTP/3 stream with an
/// internal error.
///
/// When `shutdown` resolves, the server stops accepting connections, asks
/// active connections to shut down, and waits for them to drain. Connections
/// that remain open after [`DEFAULT_SHUTDOWN_TIMEOUT`] are forcibly closed.
pub async fn serve_with<M, S, B, E, F>(endpoint: Endpoint, make_service: M, shutdown: F)
where
    M: Fn(SocketAddr) -> S + Clone + Send + 'static,
    S: Service<Request<RequestBody>, Response = Response<B>, Error = E> + Clone + Send + 'static,
    S::Future: Send + 'static,
    B: HttpBody<Data = Bytes> + Send + 'static,
    B::Error: std::fmt::Debug + Send + 'static,
    E: std::fmt::Debug + Send + 'static,
    F: Future<Output = ()>,
{
    let cancellation = CancellationToken::new();
    let mut connections = JoinSet::new();
    let mut shutdown = Box::pin(shutdown);

    loop {
        tokio::select! {
            incoming = endpoint.accept() => {
                let Some(incoming) = incoming else { break };
                connections.spawn(handle_incoming(incoming, make_service.clone(), cancellation.child_token()));
            }
            joined = connections.join_next(), if !connections.is_empty() => {
                if let Some(Err(error)) = joined {
                    error!(?error, "HTTP/3 connection task failed");
                }
            }
            () = &mut shutdown => break,
        }
    }

    cancellation.cancel();

    let drain = async {
        while let Some(result) = connections.join_next().await {
            if let Err(error) = result {
                error!(?error, "HTTP/3 connection task failed while draining");
            }
        }
    };

    if tokio::time::timeout(DEFAULT_SHUTDOWN_TIMEOUT, drain).await.is_err() {
        warn!("HTTP/3 shutdown timed out; closing endpoint");
    }
    endpoint.close(VarInt::from_u32(0), b"server shutdown");
}

/// Serves an [`S3Service`] on a configured QUIC [`Endpoint`].
///
/// The endpoint must already be configured with TLS 1.3 and the `h3` ALPN
/// protocol. The shutdown future stops new connections, sends GOAWAY, and
/// waits for clients to finish reading responses and close their connections.
/// Connections still open after [`DEFAULT_SHUTDOWN_TIMEOUT`] are forcibly closed,
/// so a client that keeps its connection open makes the shutdown wait for the
/// whole timeout before `serve` returns.
pub async fn serve<F>(endpoint: Endpoint, service: S3Service, shutdown: F)
where
    F: Future<Output = ()>,
{
    serve_with(endpoint, move |_| service.clone(), shutdown).await;
}

async fn handle_incoming<M, S, B, E>(incoming: Incoming, make_service: M, cancellation: CancellationToken)
where
    M: Fn(SocketAddr) -> S + Clone + Send + 'static,
    S: Service<Request<RequestBody>, Response = Response<B>, Error = E> + Clone + Send + 'static,
    S::Future: Send + 'static,
    B: HttpBody<Data = Bytes> + Send + 'static,
    B::Error: std::fmt::Debug + Send + 'static,
    E: std::fmt::Debug + Send + 'static,
{
    let connection = tokio::select! {
        result = incoming => match result {
            Ok(connection) => connection,
            Err(error) => {
                debug!(?error, "QUIC connection failed during handshake");
                return;
            }
        },
        () = cancellation.cancelled() => return,
    };

    let remote = connection.remote_address();

    if let Err(error) = handle_connection(connection, make_service(remote), cancellation).await {
        debug!(%remote, ?error, "HTTP/3 connection closed with an error");
    }
}

async fn handle_connection<S, B, E>(
    quic: quinn::Connection,
    service: S,
    cancellation: CancellationToken,
) -> Result<(), h3::error::ConnectionError>
where
    S: Service<Request<RequestBody>, Response = Response<B>, Error = E> + Clone + Send + 'static,
    S::Future: Send + 'static,
    B: HttpBody<Data = Bytes> + Send + 'static,
    B::Error: std::fmt::Debug + Send + 'static,
    E: std::fmt::Debug + Send + 'static,
{
    let c = h3_quinn::Connection::new(quic.clone());
    let mut connection = h3::server::builder().build(c).await?;
    let mut requests: JoinSet<()> = JoinSet::new();
    let mut shutting_down = false;

    loop {
        tokio::select! {
            result = connection.accept() => match result? {
                Some(resolver) => {
                    requests.spawn(handle_request(resolver, service.clone()));
                }
                None => break,
            },
            joined = requests.join_next(), if !requests.is_empty() => {
                if let Some(Err(error)) = joined {
                    error!(?error, "HTTP/3 request task failed");
                }
            },
            () = cancellation.cancelled(), if !shutting_down => {
                shutting_down = true;
                connection.shutdown(0).await?;
            }
        }
    }

    while let Some(result) = requests.join_next().await {
        if let Err(error) = result {
            error!(?error, "HTTP/3 request task failed while draining");
        }
    }

    // finish() only queues bytes. Dropping the h3 connection closes QUIC and can
    // discard unread responses, so let the peer close first. serve() bounds this wait.
    let _ = quic.closed().await;
    Ok(())
}

async fn handle_request<S, B, E>(resolver: Resolver, service: S)
where
    S: Service<Request<RequestBody>, Response = Response<B>, Error = E> + Clone + Send + 'static,
    S::Future: Send + 'static,
    B: HttpBody<Data = Bytes> + Send + 'static,
    B::Error: std::fmt::Debug + Send + 'static,
    E: std::fmt::Debug + Send + 'static,
{
    let (request, stream) = match resolver.resolve_request().await {
        Ok(request) => request,
        Err(error) => {
            error!(?error, "failed to resolve HTTP/3 request");
            return;
        }
    };

    let (mut send_stream, recv_stream) = stream.split();

    // RFC 9114 §4.2: connection-specific fields are forbidden in HTTP/3, and
    // `TE` may only carry "trailers". The h2 crate treats the same fields as
    // malformed for HTTP/2, while h3 forwards them unchecked, so validate here
    // before the request reaches the S3 service.
    if let Some(field) = disallowed_request_field(request.headers()) {
        debug!(%field, "rejected an HTTP/3 request carrying a connection-specific field");
        send_stream.stop_stream(Code::H3_MESSAGE_ERROR);
        return;
    }

    let content_length = request
        .headers()
        .get(http::header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse().ok());
    let request = request.map(|()| Body::new(recv_stream, content_length));

    match service.oneshot(request).await {
        Ok(response) => send_response(send_stream, response).await,
        Err(error) => {
            error!(?error, "S3 service failed for HTTP/3 request");
            send_stream.stop_stream(Code::H3_INTERNAL_ERROR);
        }
    }
}

async fn send_response<B>(mut stream: SendStream, response: Response<B>)
where
    B: HttpBody<Data = Bytes> + Send + 'static,
    B::Error: std::fmt::Debug + Send + 'static,
{
    let (parts, body) = response.into_parts();
    let mut body = std::pin::pin!(body);
    let mut headers = parts.headers;

    strip_hop_by_hop_headers(&mut headers);

    let mut head = Response::new(());
    *head.status_mut() = parts.status;
    *head.version_mut() = Version::HTTP_3;
    *head.headers_mut() = headers;

    if let Err(error) = stream.send_response(head).await {
        error!(?error, "failed to send HTTP/3 response headers");
        return;
    }

    loop {
        let frame = std::future::poll_fn(|cx| body.as_mut().poll_frame(cx)).await;

        match frame {
            Some(Ok(frame)) => match frame.into_data() {
                Ok(data) => {
                    if let Err(error) = stream.send_data(data).await {
                        error!(?error, "failed to send HTTP/3 response body");
                        return;
                    }
                }
                Err(frame) => {
                    if let Ok(trailers) = frame.into_trailers() {
                        let mut trailers = trailers;
                        strip_hop_by_hop_headers(&mut trailers);
                        if let Err(error) = stream.send_trailers(trailers).await {
                            error!(?error, "failed to send HTTP/3 response trailers");
                            return;
                        }
                        break;
                    }
                }
            },
            Some(Err(error)) => {
                error!(?error, "response body failed");
                stream.stop_stream(Code::H3_INTERNAL_ERROR);
                return;
            }
            None => break,
        }
    }

    if let Err(error) = stream.finish().await {
        error!(?error, "failed to finish HTTP/3 response stream");
    }
}

/// Returns the first field that RFC 9114 §4.2 forbids in an HTTP/3 request.
///
/// HTTP/3 carries connection-specific metadata in frames rather than fields, so
/// a request containing these fields is malformed. `TE` is the only exception,
/// and only with the value `trailers`. Whitespace surrounding a field value is
/// excluded before the value is evaluated (RFC 9110 §5.5) and the ABNF literal
/// is case-insensitive (RFC 5234 §2.3), so those spellings are accepted as well.
/// The h2 crate applies the same field list to HTTP/2, but compares `TE` byte
/// for byte, so it rejects them.
fn disallowed_request_field(headers: &HeaderMap) -> Option<&'static str> {
    for name in ["connection", "transfer-encoding", "upgrade", "keep-alive", "proxy-connection"] {
        if headers.contains_key(name) {
            return Some(name);
        }
    }

    if headers
        .get_all(header::TE)
        .iter()
        .any(|value| !trim_ows(value.as_bytes()).eq_ignore_ascii_case(b"trailers"))
    {
        return Some("te");
    }

    None
}

/// Removes the optional whitespace (SP / HTAB) around a field value, which
/// RFC 9110 §5.5 requires before the value is evaluated. Other whitespace
/// characters are left in place: CR, LF, and NUL make a field value invalid
/// rather than equivalent.
fn trim_ows(value: &[u8]) -> &[u8] {
    let start = value
        .iter()
        .position(|byte| !matches!(*byte, b' ' | b'\t'))
        .unwrap_or(value.len());
    let end = value
        .iter()
        .rposition(|byte| !matches!(*byte, b' ' | b'\t'))
        .map_or(start, |index| index + 1);

    value.get(start..end).unwrap_or_default()
}

fn strip_hop_by_hop_headers(headers: &mut HeaderMap) {
    let connection_headers = headers
        .get_all(header::CONNECTION)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .filter_map(|name| HeaderName::from_bytes(name.trim().as_bytes()).ok())
        .collect::<Vec<_>>();

    for name in connection_headers {
        headers.remove(name);
    }

    for name in [
        header::CONNECTION,
        header::PROXY_AUTHENTICATE,
        header::PROXY_AUTHORIZATION,
        header::TE,
        header::TRAILER,
        header::TRANSFER_ENCODING,
        header::UPGRADE,
        HeaderName::from_static("keep-alive"),
    ] {
        headers.remove(name);
    }
}
