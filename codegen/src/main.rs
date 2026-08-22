// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

#![allow(
    clippy::single_match_else, //
    clippy::wildcard_imports,
    clippy::match_same_arms,
    clippy::let_underscore_untyped,
)]

mod v1;
mod v2;

fn main() {
    v1::run();
    v2::run();
}
