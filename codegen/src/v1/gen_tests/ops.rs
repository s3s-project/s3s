// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

//! The ops test family: operation routing and protocol behaviour.
//!
//! The module is part of the fixed module set of the generated test binary and
//! currently carries the generated marker only; the emitter that fills it is
//! added together with the cases.

use crate::v1::dto::RustTypes;
use crate::v1::ops::Operations;

use super::{codegen_file_header, write_test_file};

use scoped_writer::g;

pub(super) fn codegen(ops: &Operations, rust_types_base: &RustTypes, rust_types_minio: &RustTypes) {
    let _ = (ops, rust_types_base, rust_types_minio);

    write_test_file("ops.rs", || {
        codegen_file_header(Some("ops"));
        g!("//! Operation test family (routing and protocol behaviour).");
    });
}
