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

use scoped_writer::g;

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

/// The name of the synthetic operation.
///
/// Emitters that compare operation names read it here instead of repeating the literal.
pub(crate) fn name() -> &'static str {
    descriptor().name
}

/// Whether `name` is the synthetic operation.
///
/// The passes that enumerate the Smithy-modeled operations skip it.
pub(crate) fn is_synthetic(name: &str) -> bool {
    name == descriptor().name
}

/// Whether `name` is one of the synthetic operation's DTO type names.
///
/// The aws-sdk conversion has no corresponding types to map them to.
pub(crate) fn is_synthetic_type(name: &str) -> bool {
    let op = descriptor();
    name == op.input || name == op.output
}

/// Emits the request fixture of the synthetic operation into the ops test file.
///
/// The caller keeps the emission position, so the generated file stays byte for byte the same.
pub(crate) fn codegen_test_fixture() {
    g([
        "/// Builds a request for the direct call of the synthetic multipart operation.",
        "fn post_object_request() -> S3Request<PostObjectInput> {",
        "    S3Request {",
        "        input: PostObjectInput::default(),",
        "        method: Method::POST,",
        "        uri: Uri::from_static(\"/bucket/key\"),",
        "        headers: HeaderMap::new(),",
        "        extensions: Extensions::new(),",
        "        credentials: None,",
        "        region: None,",
        "        service: None,",
        "        trailing_headers: None,",
        "    }",
        "}",
        "",
    ]);
}

/// Emits the direct-call test of the synthetic operation into the ops test file.
///
/// Its input comes from a verified multipart form, which the request table cannot produce;
/// the family covers it with a direct call; the test also pins the default bridge to `put_object`.
pub(crate) fn codegen_generated_test() {
    g([
        "/// The synthetic POST Object operation is covered by a direct call: its",
        "/// input comes from a verified multipart form, which the request table",
        "/// cannot produce. The default body delegates to put_object.",
        "#[tokio::test]",
        "async fn post_object_delegates_to_put_object() {",
        "    let err = DefaultS3",
        "        .post_object(post_object_request())",
        "        .await",
        "        .expect_err(\"the default body delegates to put_object, which reports NotImplemented\");",
        "    assert_eq!(err.code().as_str(), \"NotImplemented\");",
        "",
        "    let recorder = RecordingS3::default();",
        "    let err = recorder",
        "        .post_object(post_object_request())",
        "        .await",
        "        .expect_err(\"the recording double reports NotImplemented\");",
        "    assert_eq!(err.code().as_str(), \"NotImplemented\");",
        "    assert_eq!(recorder.take().as_slice(), [\"PostObject\"].as_slice());",
        "",
        "    let mut req = post_object_request();",
        "    PassThroughAccess",
        "        .post_object(&mut req)",
        "        .await",
        "        .expect(\"the default access body allows the request\");",
        "}",
        "",
    ]);
}
