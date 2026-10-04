// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

//! The machinery that the integration tests share.
//!
//! The modules are plain files next to this one, so the tests compile exactly the same
//! reader and the same adapter:
//!
//! - support owns the fixture schema and the canonical instant arithmetic,
//! - api puts this crate's implementation behind one trait and replays the fixture.
//!
//! Each test target compiles this module and uses a different part of it, so the unused
//! half of the shared surface is expected in every target.
#![allow(dead_code)]

pub mod api;
pub mod support;
