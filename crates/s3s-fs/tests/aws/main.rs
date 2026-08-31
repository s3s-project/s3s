// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

mod bucket;
mod conditional;
mod copy;
mod list;
mod multipart;
mod object;
mod sts;
mod suite;

use s3s_test::tcx::TestContext;

fn register(tcx: &mut TestContext) {
    bucket::register(tcx);
    list::register(tcx);
    object::register(tcx);
    multipart::register(tcx);
    copy::register(tcx);
    conditional::register(tcx);
    sts::register(tcx);
}

s3s_test::main!(register);
