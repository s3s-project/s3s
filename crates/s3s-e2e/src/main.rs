// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

#![allow(
    clippy::missing_errors_doc, // TODO
    clippy::missing_panics_doc, // TODO
    clippy::wildcard_imports,
)]

mod utils;

mod advanced;
mod basic;

use s3s_test::tcx::TestContext;

fn register(tcx: &mut TestContext) {
    basic::register(tcx);
    advanced::register(tcx);
}

s3s_test::main!(register);
