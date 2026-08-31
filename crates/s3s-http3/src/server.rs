// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 The s3s Authors

use bytes::Bytes;
use h3::error::Code;
use h3::server::{RequestResolver, RequestStream};
use http::{HeaderMap, HeaderName, Response, Version, header};
use http_body::Body as HttpBody;
use quinn::{Endpoint, Incoming, VarInt};
use s3s::HttpResponse;
use s3s::service::S3Service;

use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, warn};

use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use crate::body::Body;

/// Maximum time allowed for active HTTP/3 requests to drain during shutdown.
pub const DEFAULT_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(10);

type Resolver = RequestResolver<h3_quinn::Connection, Bytes>;
type SendStream = RequestStream<h3_quinn::SendStream<Bytes>, Bytes>;

/// Serves an [`S3Service`] on a configured QUIC [`Endpoint`].
///
/// The endpoint must already be configured with TLS 1.3 and the `h3` ALPN
/// protocol. The shutdown future stops new connections, gracefully drains
/// existing requests, and closes the endpoint after the shutdown timeout.
pub async fn serve<F>(endpoint: Endpoint, service: S3Service, shutdown: F)
where
    F: Future<Output = ()>,
{
    let cancellation = CancellationToken::new();
    let mut connections = JoinSet::new();
    let mut shutdown = Box::pin(shutdown);

    loop {
        tokio::select! {
            incoming = endpoint.accept() => {
                let Some(incoming) = incoming else { break };
                connections.spawn(handle_incoming(incoming, service.clone(), cancellation.child_token()));
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
                error!(?error, "HTTP/3 connection taks failed while draining");
            }
        }
    };

    if tokio::time::timeout(DEFAULT_SHUTDOWN_TIMEOUT, drain).await.is_err() {
        warn!("HTTP/3 shutdown timed out; closing endpoint");
        endpoint.close(VarInt::from_u32(0), b"server shutdown");
    } else {
        endpoint.close(VarInt::from_u32(0), b"server shutdown");
    }
}

async fn handle_incoming(incoming: Incoming, service: S3Service, cancellation: CancellationToken) {
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

    if let Err(error) = handle_connection(connection, service, cancellation).await {
        debug!(%remote, ?error, "HTTP/3 connection closed with an error");
    }
}

async fn handle_connection(
    connection: quinn::Connection,
    service: S3Service,
    cancellation: CancellationToken,
) -> Result<(), h3::error::ConnectionError> {
    let c = h3_quinn::Connection::new(connection);
    let mut connection = h3::server::builder().build(c).await?;

    loop {
        tokio::select! {
            result = connection.accept() => match result? {
                Some(resolver) => {
                    tokio::spawn(handle_request(resolver, service.clone()));
                }
                None => return Ok(()),
            },
            () = cancellation.cancelled() => {
                connection.shutdown(0).await?;

                while connection.accept().await?.is_some() {}
                return Ok(());
            }
        }
    }
}

async fn handle_request(resolver: Resolver, service: S3Service) {
    let (request, stream) = match resolver.resolve_request().await {
        Ok(request) => request,
        Err(error) => {
            error!(?error, "failed to resolve HTTP/3 request");
            return;
        }
    };

    let (send_stream, recv_stream) = stream.split();
    let request = request.map(|()| Body::new(recv_stream));

    let mut service = service;

    match tower::Service::call(&mut service, request).await {
        Ok(response) => send_response(send_stream, response).await,
        Err(error) => {
            error!(?error, "S3 service failed for HTTP/3 request");
            let mut send_stream = send_stream;
            send_stream.stop_stream(Code::H3_INTERNAL_ERROR);
        }
    }
}

async fn send_response(mut stream: SendStream, response: HttpResponse) {
    let (parts, mut body) = response.into_parts();
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
        let frame = std::future::poll_fn(|cx| Pin::new(&mut body).poll_frame(cx)).await;

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
