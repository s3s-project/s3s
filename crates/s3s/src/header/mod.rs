// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

//! S3-specific HTTP header name constants.
//!
//! This module re-exports generated constants for all HTTP headers used by the
//! Amazon S3 REST API, such as `x-amz-*` headers and other S3-specific fields.

mod generated;

pub use self::generated::*;
