// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

#![allow(
    clippy::module_name_repetitions,//
    clippy::match_same_arms, //
    clippy::missing_errors_doc, // TODO: docs
    clippy::wildcard_imports, //
    clippy::let_underscore_untyped,
    clippy::multiple_crate_versions, // TODO: check later
)]

#[macro_use]
mod error;

mod body;
mod event_stream;

pub mod conv;

mod connector;
pub use self::connector::{Client, Connector};

mod proxy;
pub use self::proxy::Proxy;

#[cfg(feature = "minio")]
pub mod minio_compat;
