// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

//! AWS Signature Version 4 — parsing and canonicalization.

#![deny(missing_docs)]
#![allow(
    clippy::multiple_crate_versions,
    clippy::module_name_repetitions,
    clippy::single_match_else,
    clippy::wildcard_imports,
    clippy::let_underscore_untyped,
    clippy::inline_always,
    clippy::needless_continue
)]

mod amz_date;
pub use self::amz_date::*;

mod authorization;
pub use self::authorization::*;

mod post_signature;
pub use self::post_signature::*;

pub(crate) mod parser;
