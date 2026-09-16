// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

//! In-crate unit tests for [`crate::ops`], split out of the former `ops/tests.rs`.
//!
//! # Placement rules
//!
//! - Tests that drive crate-private items (`CallContext`, `resolve_route`, `ops::prepare`,
//!   `crate::http::Request`) live inside the crate; they cannot move to
//!   `crates/s3s/tests/`.
//! - One file per subject, named after the subject (no `_tests` suffix — the directory
//!   says it).
//! - Fixtures shared by two or more files go to [`common`]; a helper used by a single file
//!   stays in that file.
//! - Micro-benchmarks live in `ops/benches/`, not here.
//!
//! The route test files (`ops/route_oir_tests.rs`, `ops/route_skip_validation_tests.rs`,
//! `ops/route_fixture_check.rs`) and the error-response files are declared directly by
//! `ops/mod.rs` and are untouched by this split.

use crate::ops::*;

// Shared harness. `pub(super)` so `ops/benches/` — outside this subtree — can use it too.
pub(super) mod common;

// `ops/signature.rs` tests reach this double as `ops::tests::NeverGetSecretKeyAuth`; keep
// that path working while the fixture itself lives in `common`.
pub(crate) use self::common::NeverGetSecretKeyAuth;

mod access;
mod budget;
mod content_length;
mod custom_route;
mod custom_route_body_limit;
mod decoded_content_length;
mod error_response;
mod generated_ops;
mod host;
#[cfg(feature = "minio")]
mod listen_bucket_notification;
mod oir;
mod post_object;
mod put_object_max_size;
mod routing_fixtures;
mod signature_coverage;
mod virtual_hosted_style_hint;
