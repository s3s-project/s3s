// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

//! The machinery that the generator and the tests share.
//!
//! The modules are plain files next to this one, so the generator example and the two
//! integration tests compile exactly the same reader and the same oracle adapter:
//!
//! - support owns the fixture schema and the canonical instant arithmetic,
//! - api puts the oracle and the candidate behind one trait.
//!
//! The generator example and the two integration tests each compile this module and
//! each use a different part of it, so the unused half of the shared surface is
//! expected in every target.
#![allow(dead_code)]

pub mod api;
pub mod support;
