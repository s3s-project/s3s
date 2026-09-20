// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

//! Integration tests for the `aws-chunked` protocol surface, compiled as a
//! single test target so the shared helpers in `common` are linked once.

mod common;

mod format;
mod linear;
mod shapes;
mod trailer_handle;
mod types;
