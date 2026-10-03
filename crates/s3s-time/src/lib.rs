// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

//! Date and time wire formats for the S3 API.
//!
//! The crate carries the wire formats that the S3 data model uses for time
//! values:
//!
//! - `date-time`: RFC 3339 date-time,
//! - `http-date`: IMF-fixdate, the date form used by HTTP,
//! - `epoch-seconds`: seconds since the Unix epoch, with an optional fraction.
//!
//! The implementation and the stable surface are still being cultivated: the
//! crate is a placeholder and exposes no items yet.
