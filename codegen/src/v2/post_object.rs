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

use crate::v1::o;
use crate::v1::rust;
use crate::v1::write_dir_file;
use crate::v1::{OPS_GENERATED_DIR, Operation, Operations, RustTypes, codegen_file_header};

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

/// Injects the synthetic operation into the operation set.
///
/// The upstream model has no shape for it; this is where the placeholder the other passes read comes from.
pub(crate) fn inject_operation(operations: &mut Operations) {
    if operations.contains_key(descriptor().name) {
        return;
    }
    let op = descriptor();
    let operation = Operation {
        name: o(op.name),

        input: o(op.input),
        output: o(op.output),

        // Placeholders: there is no Smithy-modeled input/output for the synthetic operation.
        smithy_input: o(op.smithy_input),
        smithy_output: o(op.smithy_output),

        s3_unwrapped_xml_output: false,
        doc: None,

        http_method: o(op.http_method),
        http_uri: o(op.http_uri),
        http_code: op.http_code,

        is_minio: false,
    };
    assert!(operations.insert(operation.name.clone(), operation).is_none());
}

/// Emits the generated operation module of the synthetic operation.
///
/// It behaves like the operation it forks, but stays a separate unit at the trait layer.
#[allow(clippy::too_many_lines)]
pub(crate) fn codegen_fork_op(rust_types: &RustTypes) {
    // PostObject is a synthetic operation: same behavior as PutObject (multipart form upload),
    // but separated at the trait layer so implementations can distinguish PUT vs POST later.
    // PostObjectInput has extra fields for POST-specific behavior (success_action_redirect, success_action_status).
    let Some(rust::Type::Struct(put_in)) = rust_types.get("PutObjectInput") else { return };
    let Some(rust::Type::Struct(_put_out)) = rust_types.get("PutObjectOutput") else { return };
    let Some(rust::Type::Struct(post_in)) = rust_types.get("PostObjectInput") else { return };
    let Some(rust::Type::Struct(_post_out)) = rust_types.get("PostObjectOutput") else { return };

    // PostObjectInput has extra fields, so we only verify that common fields match.
    assert!(post_in.fields.len() >= put_in.fields.len());
    for (a, b) in put_in.fields.iter().zip(post_in.fields.iter()) {
        assert_eq!(a.name, b.name);
        assert_eq!(a.type_, b.type_);
        assert_eq!(a.option_type, b.option_type);
    }

    write_dir_file(OPS_GENERATED_DIR, "post_object.rs", || {
        codegen_file_header();
        g([
            "use crate::dto::*;",
            "use crate::error::*;",
            "use crate::http;",
            "use crate::ops::{CallContext, PutObject};",
            "",
        ]);
        g(["pub struct PostObject;", "", "impl PostObject {"]);
        g([
            "    pub fn deserialize_http(req: &mut http::Request) -> S3Result<PostObjectInput> {",
            "        let Some(m) = req.s3ext.multipart.take() else {",
            "            return Err(invalid_request!(\"missing multipart form\"));",
            "        };",
            "",
            "        // Parse POST-specific fields before consuming the multipart form",
            "        let success_action_redirect: Option<String> = match http::parse_field_value(&m, \"success_action_redirect\")? {",
            "            Some(v) => Some(v),",
            "            None => http::parse_field_value(&m, \"redirect\")?,",
            "        };",
            "        let success_action_status: Option<i32> = http::parse_field_value(&m, \"success_action_status\")?;",
            "",
            "        // Get the validated POST policy from request extensions",
            "        let policy = req.s3ext.post_policy.take();",
            "",
            "        let put_input = PutObject::deserialize_http_multipart(req, m)?;",
            "        let mut post_input = put_object_input_into_post_object_input(put_input);",
            "        post_input.success_action_redirect = success_action_redirect;",
            "        post_input.success_action_status = success_action_status;",
            "        post_input.policy = policy;",
            "        Ok(post_input)",
            "    }",
            "",
            "    pub fn serialize_http(",
            "        bucket: &str,",
            "        key: &str,",
            "        success_action_redirect: Option<&str>,",
            "        success_action_status: Option<i32>,",
            "        output: &PostObjectOutput,",
            "    ) -> S3Result<http::Response> {",
            "        let etag_str = output.e_tag.as_ref().map(ETag::value).unwrap_or_default();",
            "",
            "        // Handle success_action_redirect: return 303 See Other with Location header",
            "        if let Some(redirect_url) = success_action_redirect {",
            "            // Defense-in-depth: Reject URLs with control characters that could enable header injection",
            "            if redirect_url.chars().any(char::is_control) {",
            "                return Err(s3_error!(InvalidArgument, \"success_action_redirect contains invalid control characters\"));",
            "            }",
            "",
            "            // Parse the URL to validate and manipulate it properly",
            "            let mut url = url::Url::parse(redirect_url).map_err(|e| s3_error!(e, InvalidArgument, \"Invalid redirect URL\"))?;",
            "",
            "            // Add query parameters (bucket, key, etag) to the URL",
            "            url.query_pairs_mut()",
            "                .append_pair(\"bucket\", bucket)",
            "                .append_pair(\"key\", key)",
            "                .append_pair(\"etag\", etag_str);",
            "",
            "            let mut res = http::Response::with_status(http::StatusCode::SEE_OTHER);",
            "            res.headers.insert(",
            "                hyper::header::LOCATION,",
            "                url.as_str().parse().map_err(|e| s3_error!(e, InternalError))?",
            "            );",
            "            return Ok(res);",
            "        }",
            "",
            "        // Handle success_action_status",
            "        match success_action_status {",
            "            Some(200) => {",
            "                // 200 OK with empty body",
            "                Ok(http::Response::with_status(http::StatusCode::OK))",
            "            }",
            "            Some(201) => {",
            "                // 201 Created with XML body using PostResponse DTO",
            "                let location = format!(\"/{bucket}/{key}\");",
            "                let post_response = crate::dto::PostResponse {",
            "                    location: &location,",
            "                    bucket,",
            "                    key,",
            "                    etag: etag_str,",
            "                };",
            "                let mut res = http::Response::with_status(http::StatusCode::CREATED);",
            "                http::set_xml_body(&mut res, &post_response)?;",
            "                Ok(res)",
            "            }",
            "            _ => {",
            "                // 204 No Content (default, also for unrecognized values)",
            "                Ok(http::Response::with_status(http::StatusCode::NO_CONTENT))",
            "            }",
            "        }",
            "    }",
            "}",
            "",
        ]);

        g(["#[async_trait::async_trait]", "impl crate::ops::Operation for PostObject {"]);
        g(["    fn name(&self) -> &'static str {", "        \"PostObject\"", "    }", ""]);
        g(["    fn needs_full_body(&self) -> bool {", "        false", "    }", ""]);
        g(["    fn has_request_payload(&self) -> bool {", "        true", "    }", ""]);
        g([
            "    fn has_streaming_body(&self) -> bool {",
            "        // POST Object's body is the multipart file stream governed by",
            "        // `post_object_max_file_size`, not the request body wrapped by",
            "        // `put_object_max_size`.",
            "        false",
            "    }",
            "",
        ]);

        g([
            "    async fn call(&self, ccx: &CallContext<'_>, req: &mut http::Request) -> S3Result<http::Response> {",
            "        let post_input = Self::deserialize_http(req)?;",
            "        // Save POST-specific fields before conversion",
            "        let success_action_redirect = post_input.success_action_redirect.clone();",
            "        let success_action_status = post_input.success_action_status;",
            "        let bucket = post_input.bucket.clone();",
            "        let key = post_input.key.clone();",
            "",
            "        let put_input = post_object_input_into_put_object_input(post_input);",
            "        let mut put_req = crate::ops::build_s3_request(put_input, req);",
            "        let s3 = ccx.s3;",
            "        if let Some(access) = ccx.access {",
            "            // Keep backward-compatible behavior: POST object used to be gated by put_object access check.",
            "            access.put_object(&mut put_req).await?;",
            "        }",
            "        let mut post_req = put_req.map_input(put_object_input_into_post_object_input);",
            "        // Restore POST-specific fields that were lost during conversion",
            "        post_req.input.success_action_redirect.clone_from(&success_action_redirect);",
            "        post_req.input.success_action_status = success_action_status;",
            "        if let Some(access) = ccx.access {",
            "            // New hook for POST object (optional).",
            "            access.post_object(&mut post_req).await?;",
            "        }",
            "        let result = s3.post_object(post_req).await;",
            "        let s3_resp = match result {",
            "            Ok(val) => val,",
            "            Err(err) => return crate::ops::serialize_error_for_method(&req.method, err, false),",
            "        };",
            "        // Serialize with POST-specific response behavior",
            "        let mut resp = Self::serialize_http(",
            "            &bucket,",
            "            &key,",
            "            success_action_redirect.as_deref(),",
            "            success_action_status,",
            "            &s3_resp.output,",
            "        )?;",
            "        if let Some(status) = s3_resp.status {",
            "            resp.status = status;",
            "        }",
            "        resp.headers.extend(s3_resp.headers);",
            "        resp.extensions.extend(s3_resp.extensions);",
            "        if http::is_bodyless_status(resp.status) {",
            "            http::strip_bodyless(&mut resp);",
            "        }",
            "        Ok(resp)",
            "    }",
            "}",
        ]);

        g!();
    });
}

/// Forks the unified DTO types of the operation the synthetic one derives from.
///
/// The synthetic operation is not modeled, so its DTO types are copies of the forked ones, renamed.
pub(crate) fn codegen_dto_clone(space: &mut RustTypes) {
    let op = descriptor();
    for (src, dst) in [(op.fork_input, op.input), (op.fork_output, op.output)] {
        if let Some(src_ty) = space.get(src).cloned() {
            let mut dst_ty = src_ty;
            match &mut dst_ty {
                rust::Type::Struct(s) => {
                    dst.clone_into(&mut s.name);
                }
                _ => {
                    // PutObject{Input,Output} are expected to be structs.
                    unimplemented!("{src} is not a struct");
                }
            }
            assert!(space.insert(dst.to_owned(), dst_ty).is_none());
        }
    }
}

/// Adds the POST-only fields to the input DTO of the synthetic operation.
///
/// The fields are not in the forked input; their position is s3s, so serializers and routers ignore them.
pub(crate) fn codegen_post_only_fields(space: &mut RustTypes) {
    let Some(rust::Type::Struct(post_in)) = space.get_mut(descriptor().input) else { return };
    post_in.fields.push(rust::StructField {
        name: o("success_action_redirect"),
        type_: o("String"),
        option_type: true,
        position: o("s3s"),
        doc: Some(o("The URL to which the client is redirected upon successful upload.")),
        ..rust::StructField::default()
    });
    post_in.fields.push(rust::StructField {
        name: o("success_action_status"),
        type_: o("i32"),
        option_type: true,
        position: o("s3s"),
        doc: Some(o(
            "The status code returned to the client upon successful upload. Valid values are 200, 201, and 204.",
        )),
        ..rust::StructField::default()
    });
    post_in.fields.push(rust::StructField {
        name: o("policy"),
        type_: o("PostPolicy"),
        option_type: true,
        position: o("s3s"),
        doc: Some(o("The POST policy document that was included in the request.")),
        ..rust::StructField::default()
    });
}

/// Emits the conversion helpers between the forked DTO types and the synthetic ones.
pub(crate) fn codegen_mapping_helpers(rust_types: &RustTypes) {
    let Some(rust::Type::Struct(put_in)) = rust_types.get("PutObjectInput") else { return };
    let Some(rust::Type::Struct(put_out)) = rust_types.get("PutObjectOutput") else { return };
    let Some(rust::Type::Struct(post_in)) = rust_types.get("PostObjectInput") else { return };
    let Some(rust::Type::Struct(post_out)) = rust_types.get("PostObjectOutput") else { return };

    // PostObjectInput has extra fields (success_action_redirect, success_action_status).
    // We verify that the common fields (those from PutObjectInput) match.
    assert!(post_in.fields.len() >= put_in.fields.len());
    for (a, b) in put_in.fields.iter().zip(post_in.fields.iter()) {
        assert_eq!(a.name, b.name);
        assert_eq!(a.type_, b.type_);
        assert_eq!(a.option_type, b.option_type);
    }
    assert_eq!(put_out.fields.len(), post_out.fields.len());
    for (a, b) in put_out.fields.iter().zip(post_out.fields.iter()) {
        assert_eq!(a.name, b.name);
        assert_eq!(a.type_, b.type_);
        assert_eq!(a.option_type, b.option_type);
    }

    // Collect POST-only field names (those not in PutObjectInput)
    let put_in_field_names: std::collections::BTreeSet<_> = put_in.fields.iter().map(|f| f.name.as_str()).collect();
    let post_only_fields: Vec<_> = post_in
        .fields
        .iter()
        .filter(|f| !put_in_field_names.contains(f.name.as_str()))
        .collect();

    g!();
    g([
        "// NOTE: PostObject is a synthetic API in s3s.",
        "// PostObjectInput has extra fields for POST-specific behavior (success_action_redirect, success_action_status).",
    ]);

    g!("pub(crate) fn put_object_input_into_post_object_input(x: PutObjectInput) -> PostObjectInput {{");
    g!("    PostObjectInput {{");
    for field in &put_in.fields {
        g!("        {}: x.{},", field.name, field.name);
    }
    // POST-only fields get default values
    for field in &post_only_fields {
        g!("        {}: None,", field.name);
    }
    g!("    }}");
    g!("}}");

    g!("pub(crate) fn post_object_input_into_put_object_input(x: PostObjectInput) -> PutObjectInput {{");
    g!("    PutObjectInput {{");
    // Only copy fields that exist in PutObjectInput
    for field in &put_in.fields {
        g!("        {}: x.{},", field.name, field.name);
    }
    g!("    }}");
    g!("}}");

    g!("pub(crate) fn put_object_output_into_post_object_output(x: PutObjectOutput) -> PostObjectOutput {{");
    g!("    PostObjectOutput {{");
    for field in &put_out.fields {
        g!("        {}: x.{},", field.name, field.name);
    }
    g!("    }}");
    g!("}}");

    // This function is currently unused but kept for symmetry and potential future use
    g!("#[allow(dead_code)]");
    g!("pub(crate) fn post_object_output_into_put_object_output(x: PostObjectOutput) -> PutObjectOutput {{");
    g!("    PutObjectOutput {{");
    for field in &post_out.fields {
        g!("        {}: x.{},", field.name, field.name);
    }
    g!("    }}");
    g!("}}");
}
