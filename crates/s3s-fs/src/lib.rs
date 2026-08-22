// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 Nugine

#![allow(
    clippy::wildcard_imports,
    clippy::missing_errors_doc, // TODO: docs
    clippy::let_underscore_untyped,
    clippy::module_name_repetitions,
    clippy::multiple_crate_versions, // TODO: check later
)]

#[macro_use]
mod error;

mod checksum;
mod fs;
mod s3;
mod utils;

pub use self::error::*;
pub use self::fs::FileSystem;
