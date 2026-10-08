// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

/// Smithy model description. Some shapes are not constructed by the pipeline that
/// currently consumes this module, so dead-code analysis does not apply here.
#[allow(dead_code)]
pub mod smithy;

/// The synthetic `PostObject` operation: its descriptor and its emitters.
pub(crate) mod post_object;

pub fn run() {}
