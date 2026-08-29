// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

//! AWS Signature Version 2 — parsing, canonicalization, and signing.

#![deny(missing_docs)]

mod authorization;
pub use self::authorization::*;

mod methods;
pub use self::methods::*;

mod post_signature;
pub use self::post_signature::*;

mod presigned_url;
pub use self::presigned_url::*;
