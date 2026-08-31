// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 The s3s Authors

#![deny(missing_docs)]

//! Experimental HTTP/3 over QUIC transport for [`s3s`].
//!
//! This crate is intentionally opt-in while the HTTP/3 ecosystem and API are
//! still evolving.

mod body;
mod server;

pub use quinn::Endpoint;
pub use server::{DEFAULT_SHUTDOWN_TIMEOUT, serve};
