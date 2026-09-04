// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 The s3s Authors

#![deny(missing_docs)]

//! Experimental server-side HTTP/3 transport for [`s3s`].
//!
//! This crate adapts an existing [`s3s::service::S3Service`] to a configured
//! Quinn QUIC endpoint. Request and response bodies are streamed without
//! buffering objects in the adapter.
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

pub use quinn::Endpoint;
pub use server::{DEFAULT_SHUTDOWN_TIMEOUT, serve};
