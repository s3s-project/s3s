// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

//! AWS Signature Version 4 — parsing, canonicalization, and signing.

#![deny(missing_docs)]
#![deny(
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::unreachable,
    clippy::unwrap_used
)]
#![allow(
    clippy::multiple_crate_versions,
    clippy::module_name_repetitions,
    clippy::single_match_else,
    clippy::wildcard_imports,
    clippy::let_underscore_untyped,
    clippy::inline_always,
    clippy::needless_continue
)]

mod amz_content_sha256;
pub use self::amz_content_sha256::*;

mod amz_date;
pub use self::amz_date::*;

mod authorization;
pub use self::authorization::*;

mod methods;
pub use self::methods::*;

mod post_signature;
pub use self::post_signature::*;

mod presigned_url;
pub use self::presigned_url::*;

pub(crate) mod crypto;
pub(crate) mod parser;
