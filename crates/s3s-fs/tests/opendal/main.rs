// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

mod operator;
mod suite;

use s3s_test::tcx::TestContext;

fn register(tcx: &mut TestContext) {
    operator::register(tcx);
}

s3s_test::main!(register);
