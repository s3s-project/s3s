// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

//! Focused micro-benchmarks for `ops`.
//!
//! These drive crate-private items (`resolve_route`, `resolve_oir`, `CallContext`,
//! `ops::prepare`), so they cannot live in `crates/s3s/benches/` — a Cargo bench target
//! only sees the public API — and they are ordinary `#[ignore]`d tests instead. This
//! directory is a *module* directory: `cargo bench` does not collect it.
//!
//! Run them with:
//!
//! ```bash
//! cargo test -p s3s --release --lib -- ops::benches --ignored --nocapture
//! ```
//!
//! The measurements are diagnostic, not a gate: CI compiles them (they are part of the
//! lib test target) but never runs them.

mod get_object;
mod route;
