// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

#![deny(missing_docs)]

//! Experimental server-side HTTP/3 transport for [`s3s`].
//!
//! This crate adapts an existing [`s3s::service::S3Service`] to a configured
//! Quinn QUIC endpoint. Request and response bodies are streamed without
//! buffering objects in the adapter.
//!
//! # Usage
//!
//! The endpoint is built by the caller (QUIC TLS 1.3 with the `h3` ALPN protocol);
//! this crate only serves requests on it. [`serve`] takes an
//! [`s3s::service::S3Service`]:
//!
//! ```
//! # use std::error::Error;
//! # async fn example(endpoint: s3s_http3::Endpoint, service: s3s::service::S3Service) -> Result<(), Box<dyn Error>> {
//! s3s_http3::serve(endpoint, service, std::future::pending::<()>()).await;
//! # Ok(())
//! # }
//! ```
//!
//! [`serve_with`] takes a factory that is called once per QUIC connection with the
//! peer address, so a generic [`tower::Service`] receives a streaming
//! [`RequestBody`] and can keep per-connection context:
//!
//! ```
//! # use std::error::Error;
//! use bytes::Bytes;
//! use http::{Request, Response};
//! use http_body_util::{BodyExt, Full};
//! use s3s_http3::RequestBody;
//!
//! # async fn example(endpoint: s3s_http3::Endpoint) -> Result<(), Box<dyn Error + Send + Sync>> {
//! let make_service = move |remote_addr: std::net::SocketAddr| {
//!     tower::service_fn(move |request: Request<RequestBody>| async move {
//!         let body = BodyExt::collect(request.into_body()).await?;
//!         let received = body.to_bytes().len();
//!         let text = format!("from {remote_addr}: {received} bytes\n");
//!         Ok::<_, Box<dyn Error + Send + Sync>>(Response::new(Full::new(Bytes::from(text))))
//!     })
//! };
//!
//! s3s_http3::serve_with(endpoint, make_service, std::future::pending::<()>()).await;
//! # Ok(())
//! # }
//! ```
//!
//! A runnable example lives in `examples/serve-with.rs` (a generic service).
//! The `s3s-fs` crate serves the file system over HTTP/3 and is a reference for
//! an `S3Service` on an endpoint.
//!
//! # TLS and networking
//!
//! The supplied [`Endpoint`] must use QUIC TLS 1.3 with the `h3` ALPN
//! protocol. Certificate management is intentionally left to the caller.
//! The endpoint must be reachable over UDP, including through firewalls,
//! load balancers, and NAT configuration.
//!
//! # Client compatibility
//!
//! Clients must support HTTP/3 over QUIC. This crate is server-side only and
//! does not provide HTTP/3 clients, 0-RTT, datagrams, or WebTransport.
//!
//! # Stability
//!
//! The API is experimental and may change while the HTTP/3 ecosystem evolves.
//! HTTP/3 remains opt-in; existing `s3s` services and binaries are unaffected.

mod body;
mod server;

pub use body::{Body as RequestBody, BodyError};
pub use quinn::Endpoint;
pub use server::{DEFAULT_SHUTDOWN_TIMEOUT, serve, serve_with};
