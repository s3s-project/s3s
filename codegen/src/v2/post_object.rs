// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

//! Ownership of the synthetic `PostObject` operation.
//!
//! `PostObject` has no shape in the upstream Smithy model, so the generation
//! pipeline used to special-case it in several modules. This module is the one
//! place that describes the operation; the v1 modules only assemble its output
//! in their own passes.
//!
//! It provides a descriptor and pure functions rather than running a pass of
//! its own: the operation has to be injected into the operation set and its
//! DTO types have to be forked while v1 generates those, so taking over the
//! execution order would add coupling without owning anything.

/// The facts of the synthetic `PostObject` operation.
///
/// The descriptor is the single source of truth for the name, the DTO type
/// names and the placeholder Smithy shapes; the emitters read it instead of
/// repeating the literals.
#[allow(dead_code)] // read by the v1 emitters once they delegate here
pub(crate) struct SyntheticOp {
    /// Operation name: the S3 trait method name and the generated file name.
    pub(crate) name: &'static str,
    /// Rust input DTO type name.
    pub(crate) input: &'static str,
    /// Rust output DTO type name.
    pub(crate) output: &'static str,
    /// Placeholder Smithy input shape: the request is not modeled as a shape.
    pub(crate) smithy_input: &'static str,
    /// Placeholder Smithy output shape: the response is not modeled as a shape.
    pub(crate) smithy_output: &'static str,
    /// The operation whose input DTO the synthetic operation forks.
    pub(crate) fork_input: &'static str,
    /// The operation whose output DTO the synthetic operation forks.
    pub(crate) fork_output: &'static str,
    /// HTTP method of the placeholder route.
    pub(crate) http_method: &'static str,
    /// HTTP URI of the placeholder route.
    pub(crate) http_uri: &'static str,
    /// HTTP status of the placeholder route.
    pub(crate) http_code: u16,
}

/// Returns the descriptor of the synthetic `PostObject` operation.
#[allow(dead_code)] // called by the v1 emitters once they delegate here
pub(crate) fn descriptor() -> SyntheticOp {
    SyntheticOp {
        name: "PostObject",
        input: "PostObjectInput",
        output: "PostObjectOutput",
        smithy_input: "Unit",
        smithy_output: "Unit",
        fork_input: "PutObjectInput",
        fork_output: "PutObjectOutput",
        http_method: "POST",
        http_uri: "/{Bucket}",
        http_code: 200,
    }
}
