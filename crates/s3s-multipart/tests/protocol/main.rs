// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

//! Integration tests for the multipart protocol surface, compiled as a
//! single test target so the shared helpers in `common` are linked once.

mod common;

mod compat_notes;
mod post_object;
mod rfc2046;
mod rfc7578;
