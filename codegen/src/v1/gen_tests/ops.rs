// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

//! The ops test family: every modeled operation is dispatched once through the
//! real `S3Service` pipeline with a minimal request derived from the model.
//!
//! A case satisfies the two contracts that the dispatch path checks before the
//! user implementation is reached:
//!
//! - the routing contract: the method, the path shape, and the query tag,
//!   query patterns and x-id value of the operation's httpUri, plus every query
//!   string and header that the router requires for the operation;
//! - the input contract: every field that the generated `deserialize_http` parses
//!   without a fallback is present, and a payload that must be non-empty gets a
//!   body whose element names mirror the model's XML names.
//!
//! The values are derived from the model, so an operation whose bindings change
//! changes the generated request in the same codegen run.
//!
//! The expected behaviour is asserted in two directions: a recording double
//! asserts which S3 method the request reaches, and a double with no overrides
//! asserts that every default method body answers with `NotImplemented`.

use crate::v1::dto::RustTypes;
use crate::v1::ops::Operation;
use crate::v1::ops::Operations;
use crate::v1::rust;

use super::debug::Mode;
use super::debug::spec;
use super::debug::val_expr;
use super::{codegen_file_header, write_test_file};

use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::ops::Not;

use heck::ToSnakeCase;
use s3s_model::error_codes;
use scoped_writer::g;

/// The synthetic multipart operation. Its input is parsed from a verified
/// multipart form, which needs a signed request, so the family covers it with a
/// direct call instead of a request-table entry.
const POST_OBJECT: &str = "PostObject";

/// Operations whose optional XML payload still needs a non-empty body: the
/// generated deserializer maps an empty body to `MalformedXML` for them.
const REQUIRES_BODY: &[&str] = &["CompleteMultipartUpload", "PutObjectLegalHold", "PutObjectRetention"];

/// Headers that are parsed by the dispatch path itself rather than by the
/// operation input: a value that does not match the body would break the body
/// handling before the operation is reached, so the cases leave them out.
const SKIP_HEADERS: &[&str] = &["content-length", "x-amz-decoded-content-length"];

/// Recursion limit for the XML body generator. The required-field graph of the
/// payload types is shallow; the limit only keeps a cyclic model from hanging.
const XML_DEPTH_LIMIT: usize = 8;

/// One operation with the minimal request that routes to it.
struct Case {
    name: String,
    method: String,
    uri: String,
    status: u16,
    headers: Vec<(String, String)>,
    body: String,
    is_minio: bool,
    /// The request path is not a bucket path, so the default bucket-name
    /// validation rejects it and the case needs permissive validation.
    literal_path: bool,
}

pub(super) fn codegen(ops: &Operations, rust_types_base: &RustTypes, rust_types_minio: &RustTypes) {
    let _ = rust_types_base;
    let cases = collect_cases(ops, rust_types_minio);
    assert_eq!(
        cases.len() + 1,
        ops.len(),
        "every operation except {POST_OBJECT} must have a request case"
    );

    write_test_file("ops.rs", || {
        codegen_file_header(Some("ops"));
        emit_imports();
        emit_doubles(ops);
        emit_requests(&cases);
        emit_harness();
        emit_error_codes();
        emit_populated_outputs(ops, rust_types_minio, &cases);
        emit_required_body_case();
    });
}

fn emit_imports() {
    g([
        "//! Operation test family: routing and protocol behaviour.",
        "",
        "#![allow(clippy::too_many_lines)]",
        "#![allow(clippy::needless_pass_by_value)]",
        "#![allow(clippy::doc_markdown)]",
        "#![allow(clippy::wildcard_imports)]",
        "",
        "use std::sync::Arc;",
        "use std::sync::Mutex;",
        "",
        "use http::Extensions;",
        "use http::HeaderMap;",
        "use http::Method;",
        "use http::Uri;",
        "use http_body_util::BodyExt;",
        "use s3s::access::S3Access;",
        "use s3s::dto;",
        "use s3s::dto::*;",
        "use s3s::service::S3ServiceBuilder;",
        "use s3s::Body;",
        "use s3s::S3;",
        "use s3s::S3ErrorCode;",
        "use s3s::S3Request;",
        "use s3s::S3Response;",
        "use s3s::S3Result;",
        "use s3s::validation::NameValidation;",
        "",
    ]);
}

/// Emits the two S3 doubles and the pass-through access double.
fn emit_doubles(ops: &Operations) {
    g([
        "/// An S3 double that overrides nothing: every operation runs the default",
        "/// body of its trait method.",
        "struct DefaultS3;",
        "",
        "impl S3 for DefaultS3 {}",
        "",
        "/// An S3 double that records the operation name and then reports",
        "/// NotImplemented, so a test can assert which operation a request reached.",
        "#[derive(Clone, Default)]",
        "struct RecordingS3 {",
        "    calls: Arc<Mutex<Vec<&'static str>>>,",
        "}",
        "",
        "impl RecordingS3 {",
        "    fn record(&self, name: &'static str) {",
        "        self.calls.lock().expect(\"recording lock\").push(name);",
        "    }",
        "",
        "    fn take(&self) -> Vec<&'static str> {",
        "        std::mem::take(&mut self.calls.lock().expect(\"recording lock\"))",
        "    }",
        "}",
        "",
        "#[async_trait::async_trait]",
        "impl S3 for RecordingS3 {",
    ]);

    for op in ops.values() {
        if op.is_minio {
            g!("#[cfg(feature = \"minio\")]");
        }
        g!(
            "async fn {}(&self, _req: S3Request<{}>) -> S3Result<S3Response<{}>> {{",
            op.name.to_snake_case(),
            op.input,
            op.output
        );
        g!("    self.record({:?});", op.name);
        g!("    Err(s3s::s3_error!(NotImplemented))");
        g!("}}");
        g!();
    }

    g([
        "}",
        "",
        "/// An access-control double that overrides nothing: every per-operation",
        "/// access method returns Ok(()) from its default body.",
        "struct PassThroughAccess;",
        "",
        "#[async_trait::async_trait]",
        "impl S3Access for PassThroughAccess {}",
        "",
        "/// A bucket-name validation that accepts any name, used by the operations",
        "/// whose literal request path is not a bucket path.",
        "struct PermissiveValidation;",
        "",
        "impl NameValidation for PermissiveValidation {",
        "    fn validate_bucket_name(&self, _name: &str) -> bool {",
        "        true",
        "    }",
        "}",
        "",
    ]);

    emit_succeeding_double(ops);
}

/// Emits the double whose operations all succeed with a default output, so the
/// output serialization of every operation runs.
fn emit_succeeding_double(ops: &Operations) {
    g([
        "/// An S3 double that answers every operation with a default output, so the",
        "/// request runs through the operation's output serialization.",
        "struct SucceedingS3;",
        "",
        "#[async_trait::async_trait]",
        "impl S3 for SucceedingS3 {",
    ]);

    for op in ops.values() {
        if op.is_minio {
            g!("#[cfg(feature = \"minio\")]");
        }
        g!(
            "async fn {}(&self, _req: S3Request<{}>) -> S3Result<S3Response<{}>> {{",
            op.name.to_snake_case(),
            op.input,
            op.output
        );
        g!("    Ok(S3Response::new({}::default()))", op.output);
        g!("}}");
        g!();
    }

    g(["}", ""]);
}

fn emit_requests(cases: &[Case]) {
    g([
        "/// The minimal request of one operation.",
        "struct OpRequest {",
        "    name: &'static str,",
        "    method: &'static str,",
        "    uri: &'static str,",
        "    status: u16,",
        "    literal_path: bool,",
        "    headers: &'static [(&'static str, &'static str)],",
        "    body: &'static [u8],",
        "}",
        "",
        "/// One request per modeled operation, in model (name) order.",
        "const OP_REQUESTS: &[OpRequest] = &[",
    ]);

    for case in cases {
        if case.is_minio {
            g!("#[cfg(feature = \"minio\")]");
        }
        g!(
            "OpRequest {{ name: {:?}, method: {:?}, uri: {:?}, status: {}, literal_path: {}, headers: &{}, body: b{:?} }},",
            case.name,
            case.method,
            case.uri,
            case.status,
            case.literal_path,
            header_list(case),
            case.body
        );
    }

    g(["];", ""]);
}

/// Whether `name` is a checksum value header, `x-amz-checksum-<algorithm>`, rather
/// than one of the auxiliary `x-amz-checksum-algorithm` / `-mode` / `-type` headers.
fn is_checksum_value_header(name: &str) -> bool {
    let Some(suffix) = name.strip_prefix("x-amz-checksum-") else {
        return false;
    };
    !matches!(suffix, "algorithm" | "mode" | "type")
}

fn header_list(case: &Case) -> String {
    // A request may carry at most one checksum value header: the service rejects a request
    // that names more than one, so the sample keeps the first and drops the rest. A sample
    // with all of them is refused before the operation reaches its handler, which is what
    // these cases exercise.
    let mut checksum_value_seen = false;
    let entries = case
        .headers
        .iter()
        .filter(|(name, _)| {
            if !is_checksum_value_header(name) {
                return true;
            }
            let keep = !checksum_value_seen;
            checksum_value_seen = true;
            keep
        })
        .map(|(name, value)| format!("({name:?}, {value:?})"))
        .collect::<Vec<_>>()
        .join(", ");
    format!("[{entries}]")
}

#[allow(clippy::too_many_lines)]
fn emit_harness() {
    g([
        "/// Builds the HTTP request of one case.",
        "fn request_of(case: &OpRequest) -> s3s::HttpRequest {",
        "    let mut builder = http::Request::builder().method(case.method).uri(case.uri);",
        "    for (name, value) in case.headers {",
        "        builder = builder.header(*name, *value);",
        "    }",
        "    builder.body(Body::from(case.body.to_vec())).expect(\"valid request\")",
        "}",
        "",
        "/// Reads a response body into a string.",
        "async fn response_text(resp: s3s::HttpResponse) -> String {",
        "    let bytes = resp.into_body().collect().await.expect(\"response body\").to_bytes();",
        "    String::from_utf8_lossy(&bytes).into_owned()",
        "}",
        "",
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

    g([
        "/// Every operation is reached through the real service pipeline, and the",
        "/// request records exactly the expected S3 method. A deserialization",
        "/// failure, a mis-route or a wrong handler all show up as a failure.",
        "#[tokio::test]",
        "async fn every_operation_reaches_its_handler() {",
        "    let mut failures: Vec<String> = Vec::new();",
        "    for case in OP_REQUESTS {",
        "        let recorder = RecordingS3::default();",
        "        let mut builder = S3ServiceBuilder::new(recorder.clone());",
        "        builder.set_access(PassThroughAccess);",
        "        if case.literal_path {",
        "            builder.set_validation(PermissiveValidation);",
        "        }",
        "        let service = builder.build();",
        "",
        "        let resp = service.call(request_of(case)).await.expect(\"service call\");",
        "        let status = resp.status();",
        "        let body = response_text(resp).await;",
        "        let reached = recorder.take();",
        "",
        "        if reached.as_slice() != [case.name].as_slice() {",
        "            failures.push(format!(\"{}: reached {reached:?}, expected {}\", case.name, case.name));",
        "            continue;",
        "        }",
        "        if status != http::StatusCode::NOT_IMPLEMENTED {",
        "            failures.push(format!(\"{}: status {status}, body {body}\", case.name));",
        "            continue;",
        "        }",
        "        if !body.contains(\"<Code>NotImplemented</Code>\") {",
        "            failures.push(format!(\"{}: unexpected body {body}\", case.name));",
        "        }",
        "    }",
        "    assert!(failures.is_empty(), \"{} operation(s) failed:\\n{}\", failures.len(), failures.join(\"\\n\"));",
        "}",
        "",
    ]);

    g([
        "/// Every default body of the S3 trait reports NotImplemented for the same",
        "/// requests, so a default body that silently returns a value fails here.",
        "#[tokio::test]",
        "async fn default_bodies_report_not_implemented() {",
        "    let mut failures: Vec<String> = Vec::new();",
        "    for case in OP_REQUESTS {",
        "        let mut builder = S3ServiceBuilder::new(DefaultS3);",
        "        builder.set_access(PassThroughAccess);",
        "        if case.literal_path {",
        "            builder.set_validation(PermissiveValidation);",
        "        }",
        "        let service = builder.build();",
        "",
        "        let resp = service.call(request_of(case)).await.expect(\"service call\");",
        "        let status = resp.status();",
        "        let body = response_text(resp).await;",
        "",
        "        if status != http::StatusCode::NOT_IMPLEMENTED {",
        "            failures.push(format!(\"{}: status {status}, body {body}\", case.name));",
        "            continue;",
        "        }",
        "        if !body.contains(\"<Code>NotImplemented</Code>\") {",
        "            failures.push(format!(\"{}: unexpected body {body}\", case.name));",
        "        }",
        "    }",
        "    assert!(failures.is_empty(), \"{} operation(s) failed:\\n{}\", failures.len(), failures.join(\"\\n\"));",
        "}",
        "",
    ]);

    g([
        "/// Every operation that succeeds renders its modeled status code, so the",
        "/// output serialization of each operation runs.",
        "#[tokio::test]",
        "async fn every_operation_serializes_its_output() {",
        "    let mut failures: Vec<String> = Vec::new();",
        "    for case in OP_REQUESTS {",
        "        let mut builder = S3ServiceBuilder::new(SucceedingS3);",
        "        builder.set_access(PassThroughAccess);",
        "        if case.literal_path {",
        "            builder.set_validation(PermissiveValidation);",
        "        }",
        "        let service = builder.build();",
        "",
        "        let resp = service.call(request_of(case)).await.expect(\"service call\");",
        "        let status = resp.status();",
        "        if status.as_u16() != case.status {",
        "            failures.push(format!(\"{}: status {status}, expected {}\", case.name, case.status));",
        "        }",
        "    }",
        "    assert!(failures.is_empty(), \"{} operation(s) failed:\\n{}\" , failures.len(), failures.join(\"\\n\"));",
        "}",
        "",
    ]);

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

fn collect_cases(ops: &Operations, types: &RustTypes) -> Vec<Case> {
    let mut cases = Vec::new();

    for op in ops.values() {
        if op.name == POST_OBJECT {
            continue;
        }

        let input = expect_struct(op.input.as_str(), types);
        let (path, query, literal_path) = split_http_uri(&op.http_uri);
        let mut query_parts: Vec<String> = query
            .map(|query| query.split('&').map(ToOwned::to_owned).collect())
            .unwrap_or_default();
        let mut headers: Vec<(String, String)> = Vec::new();
        let mut body = String::new();

        for field in &input.fields {
            match field.position.as_str() {
                "query" if query_required(field) => {
                    let name = field.http_query.as_deref().expect("query binding");
                    let value = leaf_text(&field.type_, types, Site::Body);
                    query_parts.push(format!("{name}={value}"));
                }
                "header" => {
                    let name = field.http_header.clone().expect("header binding");
                    if SKIP_HEADERS.contains(&name.as_str()) || headers.iter().any(|(seen, _)| *seen == name) {
                        continue;
                    }
                    headers.push((name, header_text(field, types)));
                }
                "payload" => body = payload_body(op, field, types),
                _ => {}
            }
        }

        let mut uri = path;
        if !query_parts.is_empty() {
            uri.push('?');
            uri.push_str(&query_parts.join("&"));
        }

        assert!(uri.is_ascii(), "request URI of {} must be ASCII", op.name);
        for (name, value) in &headers {
            assert!(name.is_ascii() && value.is_ascii(), "header of {} must be ASCII", op.name);
        }
        assert!(body.is_ascii(), "request body of {} must be ASCII", op.name);

        cases.push(Case {
            name: op.name.clone(),
            method: op.http_method.clone(),
            uri,
            status: op.http_code,
            headers,
            body,
            is_minio: op.is_minio,
            literal_path,
        });
    }

    cases
}

/// Splits an httpUri into the request path (placeholders substituted with a
/// sample bucket and key), the raw query string, and whether the path is a
/// literal path rather than a bucket or root path.
fn split_http_uri(uri: &str) -> (String, Option<&str>, bool) {
    let (path, query) = match uri.split_once('?') {
        Some((path, query)) => (path, Some(query)),
        None => (uri, None),
    };
    assert!(path.starts_with('/'), "httpUri must start with a slash: {uri:?}");
    let literal_path = path != "/" && path.starts_with("/{Bucket}").not();
    let path = path.replace("{Bucket}", "bucket").replace("{Key+}", "key");
    (path, query, literal_path)
}

/// Mirrors `required_query_strings` of the ops emitter: a query field without a
/// fallback must be present in the request.
fn query_required(field: &rust::StructField) -> bool {
    field.option_type.not() && field.default_value.is_none()
}

/// The sample value of a header field. Header timestamps default to the HTTP
/// date format when the model declares no format.
fn header_text(field: &rust::StructField, types: &RustTypes) -> String {
    match types.get(field.type_.as_str()) {
        Some(rust::Type::List(list)) => leaf_text(&list.member.type_, types, Site::Header),
        Some(rust::Type::Timestamp(ty)) => timestamp_text(ty.format.as_deref(), &ty.name, Site::Header),
        _ => leaf_text(&field.type_, types, Site::Header),
    }
}

/// Builds the request body of the operation's payload field.
fn payload_body(op: &Operation, field: &rust::StructField, types: &RustTypes) -> String {
    let required = field.option_type.not() || REQUIRES_BODY.contains(&op.name.as_str());
    if required.not() {
        return String::new();
    }

    match field.type_.as_str() {
        "Policy" => "{\"Version\":\"2012-10-17\"}".to_owned(),
        "StreamingBlob" => "sample payload".to_owned(),
        type_name => {
            let content = match types.get(type_name) {
                Some(rust::Type::Struct(ty)) => struct_content(ty, types, 0),
                Some(rust::Type::StructEnum(ty)) => struct_enum_content(ty, types, 0),
                other => panic!("unsupported payload type of {}: {other:?}", op.name),
            };
            xml_element(&root_element_name(field, type_name, types), &content)
        }
    }
}

/// The XML name of the payload root: the payload field's xmlName binding first,
/// then the payload type's own xmlName, then the type name.
fn root_element_name(field: &rust::StructField, type_name: &str, types: &RustTypes) -> String {
    if let Some(name) = &field.xml_name {
        return name.clone();
    }
    if let Some(rust::Type::Struct(ty)) = types.get(type_name)
        && let Some(name) = &ty.xml_name
    {
        return name.clone();
    }
    type_name.to_owned()
}

/// Builds the content of a struct element: its required xml fields, in model
/// order.
fn struct_content(ty: &rust::Struct, types: &RustTypes, depth: usize) -> String {
    assert!(depth < XML_DEPTH_LIMIT, "xml nesting limit reached in {}", ty.name);

    let mut out = String::new();
    for field in &ty.fields {
        if field.position != "xml" || field.option_type || field.is_xml_attr {
            continue;
        }
        out.push_str(&field_element(field, types, depth));
    }
    out
}

/// Builds one element of a struct's content.
fn field_element(field: &rust::StructField, types: &RustTypes, depth: usize) -> String {
    let name = field.xml_name.clone().unwrap_or_else(|| field.camel_name.clone());

    match types.get(field.type_.as_str()) {
        Some(rust::Type::List(list)) => {
            let item = type_content(&list.member.type_, types, depth + 1);
            if field.xml_flattened {
                xml_element(&name, &item)
            } else {
                let member_name = list
                    .member
                    .xml_name
                    .clone()
                    .unwrap_or_else(|| panic!("non-flattened list {} needs a member XML name", list.name));
                xml_element(&name, &xml_element(&member_name, &item))
            }
        }
        _ => xml_element(&name, &type_content(&field.type_, types, depth + 1)),
    }
}

/// Builds the content of one element of a given type.
fn type_content(type_name: &str, types: &RustTypes, depth: usize) -> String {
    assert!(depth < XML_DEPTH_LIMIT, "xml nesting limit reached at {type_name}");
    match types.get(type_name) {
        Some(rust::Type::Struct(ty)) => struct_content(ty, types, depth),
        Some(rust::Type::StructEnum(ty)) => struct_enum_content(ty, types, depth),
        _ => leaf_text(type_name, types, Site::Body),
    }
}

/// Builds the element of a union payload: its first variant, named by the
/// variant's xmlName.
fn struct_enum_content(ty: &rust::StructEnum, types: &RustTypes, depth: usize) -> String {
    let variant = ty
        .variants
        .first()
        .unwrap_or_else(|| panic!("union {} has no variant", ty.name));
    let name = variant.xml_name.clone().unwrap_or_else(|| variant.name.clone());
    let content = match types.get(variant.type_.as_str()) {
        Some(rust::Type::Struct(ty)) => struct_content(ty, types, depth + 1),
        other => panic!("union variant {} must be a struct: {other:?}", variant.name),
    };
    xml_element(&name, &content)
}

fn xml_element(name: &str, content: &str) -> String {
    format!("<{name}>{content}</{name}>")
}

/// Where a sample value is used. A timestamp without a model format defaults to
/// the HTTP date format in a header and to date-time in a body or query string,
/// mirroring the deserializer.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Site {
    Header,
    Body,
}

/// The text content of a non-struct value, derived from the model type.
fn leaf_text(type_name: &str, types: &RustTypes, site: Site) -> String {
    match types.get(type_name) {
        None => primitive_text(type_name),
        Some(rust::Type::Alias(ty)) => leaf_text(&ty.type_, types, site),
        Some(rust::Type::StrEnum(ty)) => match ty.variants.first() {
            Some(variant) => variant.value.clone(),
            None => panic!("enum {} has no variant", ty.name),
        },
        Some(rust::Type::Timestamp(ty)) => timestamp_text(ty.format.as_deref(), &ty.name, site),
        Some(rust::Type::List(ty)) => leaf_text(&ty.member.type_, types, site),
        Some(rust::Type::Provided(ty)) => provided_text(&ty.name),
        Some(other) => panic!("{type_name} is not a leaf type: {other:?}"),
    }
}

/// Sample values of the primitive types that the model leaves unaliased.
fn primitive_text(type_name: &str) -> String {
    match type_name {
        "String" => "sample".to_owned(),
        "bool" => "true".to_owned(),
        "i8" | "i16" | "i32" | "i64" | "u8" | "u16" | "u32" | "u64" | "f32" | "f64" => "1".to_owned(),
        "Blob" => "sample".to_owned(),
        "ETagCondition" => "sample".to_owned(),
        other => panic!("unsupported primitive type {other}"),
    }
}

/// Sample values of the manually provided types.
fn provided_text(type_name: &str) -> String {
    match type_name {
        "CopySource" => "src-bucket/src-key".to_owned(),
        "ContentType" => "text/plain".to_owned(),
        "ETag" => "sample".to_owned(),
        "Range" => "bytes=0-1".to_owned(),
        other => panic!("unsupported provided type {other}"),
    }
}

/// A timestamp literal in the wire format of the model type.
fn timestamp_text(format: Option<&str>, type_name: &str, site: Site) -> String {
    let format = match (format, site) {
        (Some(format), _) => format,
        (None, Site::Header) => "HttpDate",
        (None, Site::Body) => "DateTime",
    };
    match format {
        "DateTime" => "2026-01-02T03:04:05.000Z".to_owned(),
        "HttpDate" => "Fri, 02 Jan 2026 03:04:05 GMT".to_owned(),
        "EpochSeconds" => "1767323045".to_owned(),
        other => panic!("unsupported timestamp format {other} of {type_name}"),
    }
}

/// Looks a type up as a struct, failing codegen when the model changes shape.
fn expect_struct<'a>(type_name: &str, types: &'a RustTypes) -> &'a rust::Struct {
    match types.get(type_name) {
        Some(rust::Type::Struct(ty)) => ty,
        other => panic!("{type_name} must be a struct: {other:?}"),
    }
}

/// The error codes of the official table, filtered exactly like the error
/// emitter: the `503 SlowDown` alias and dotted service codes are not enum
/// variants. The status is the table's HTTP status, or None for a code that
/// declares no status.
fn error_codes() -> Vec<(String, Option<u16>)> {
    let map = error_codes::load_json("data/s3_error_codes.json").expect("the error code table must load");
    let mut codes = BTreeSet::new();
    for group in map.values() {
        for entry in group {
            if entry.code == "503 SlowDown" || entry.code.contains('.') {
                continue;
            }
            codes.insert((entry.code.clone(), entry.http_status_code));
        }
    }
    codes.into_iter().collect()
}

/// Emits the table-driven error-code checks: every code of the official table
/// must resolve to its own variant, and every variant must carry a status code
/// and a default message.
fn emit_error_codes() {
    let codes = error_codes();
    assert!(!codes.is_empty(), "the error code table must not be empty");

    g([
        "/// Every code of the official S3 error-code table with the HTTP status that",
        "/// the table declares for it (None where the table declares no status).",
        "const ERROR_CODES: &[(&str, Option<u16>)] = &[",
    ]);
    for (code, status) in &codes {
        assert!(code.is_ascii(), "error code {code:?} must be ASCII");
        match status {
            Some(status) => g!("({:?}, Some({})),", code, status),
            None => g!("({:?}, None),", code),
        }
    }
    g(["];", ""]);

    g([
        "/// Every error code resolves to its own variant, and every variant carries",
        "/// a status code and a default message.",
        "#[test]",
        "fn every_error_code_resolves_with_status_and_message() {",
        "    let mut failures: Vec<String> = Vec::new();",
        "    for (code, expected_status) in ERROR_CODES {",
        "        let Some(parsed) = S3ErrorCode::from_bytes(code.as_bytes()) else {",
        "            failures.push(format!(\"{code}: from_bytes returned None\"));",
        "            continue;",
        "        };",
        "        if parsed.as_str() != *code {",
        "            failures.push(format!(\"{code}: resolved to {}\", parsed.as_str()));",
        "            continue;",
        "        }",
        "        let status = parsed.status_code().map(|status| status.as_u16());",
        "        if status != *expected_status {",
        "            failures.push(format!(\"{code}: status {status:?}, table says {expected_status:?}\"));",
        "        }",
        "        if parsed.default_message().is_none() {",
        "            failures.push(format!(\"{code}: no default message\"));",
        "        }",
        "    }",
        "    assert!(failures.is_empty(), \"{} code(s) failed:\\n{}\" , failures.len(), failures.join(\"\\n\"));",
        "}",
        "",
    ]);
}
/// The value of one output header field.
///
/// A header value must be visible ASCII, so a string-like field uses a plain
/// sample instead of the dto sample text. A string enum is built from a borrowed
/// and from an owned string in turn, so both arms of its Cow conversion run.
/// The second element of the pair reports whether the expression reads the
/// owned flag of the double.
fn header_value_expr(types: &RustTypes, type_name: &str) -> (String, bool) {
    match types.get(type_name) {
        Some(rust::Type::StrEnum(ty)) => {
            let value = match ty.variants.first() {
                Some(variant) => variant.value.clone(),
                None => "sample".to_owned(),
            };
            let borrowed = format!("dto::{}::from_static({value:?})", ty.name);
            let owned = format!("dto::{}::from(String::from({value:?}))", ty.name);
            (format!("if owned {{ {owned} }} else {{ {borrowed} }}"), true)
        }
        Some(rust::Type::List(list)) => {
            let (item, uses_owned) = header_value_expr(types, &list.member.type_);
            (format!("vec![{item}]"), uses_owned)
        }
        Some(rust::Type::Timestamp(ts)) => {
            let format = ts.format.as_deref().unwrap_or("HttpDate");
            let sample = match format {
                "HttpDate" => "Fri, 02 Jan 2026 03:04:05 GMT",
                "EpochSeconds" => "1767323045",
                _ => "2026-01-02T03:04:05.000Z",
            };
            (
                format!("dto::Timestamp::parse(dto::TimestampFormat::{format}, {sample:?}).unwrap()"),
                false,
            )
        }
        Some(rust::Type::Alias(alias)) => match alias.type_.as_str() {
            "String" => ("String::from(\"s3s-header\")".to_owned(), false),
            "bool" => ("true".to_owned(), false),
            "i32" | "i64" => ("1".to_owned(), false),
            _ => (val_expr(&spec(types, type_name, Mode::Full, "header")), false),
        },
        _ => (val_expr(&spec(types, type_name, Mode::Full, "header")), false),
    }
}

/// The value of one output field of a populated output, and whether it reads the
/// owned flag. A field the generated serializer ignores keeps its default.
fn output_field_assign(types: &RustTypes, field: &rust::StructField) -> Option<(String, bool)> {
    match field.position.as_str() {
        "header" => {
            let (value, uses_owned) = header_value_expr(types, &field.type_);
            let value = if field.option_type { format!("Some({value})") } else { value };
            Some((value, uses_owned))
        }
        "metadata" => Some((
            "Some(std::collections::HashMap::from([(String::from(\"s3s-meta\"), String::from(\"s3s-value\"))]))".to_owned(),
            false,
        )),
        "xml" | "payload" => {
            let value = val_expr(&spec(types, &field.type_, Mode::Full, &field.name));
            let value = if field.option_type { format!("Some({value})") } else { value };
            Some((value, false))
        }
        _ => None,
    }
}

/// Emits one populated output value per request case, the double that answers
/// with it, and the case that drives the pair.
///
/// The generated output serialization only reaches a header conversion or a body
/// writer when the corresponding field is set, and the default output leaves
/// every optional field unset. The family therefore drives a second double whose
/// outputs are filled from the model, once with borrowed and once with owned
/// string values.
#[allow(clippy::too_many_lines)]
fn emit_populated_outputs(ops: &Operations, types: &RustTypes, cases: &[Case]) {
    let case_names: BTreeSet<&str> = cases.iter().map(|case| case.name.as_str()).collect();

    g([
        "/// A populated output value of one operation. The generated output",
        "/// serialization only converts a header or writes a body when the field is",
        "/// set, so a value filled from the model is what makes those paths run.",
    ]);

    let mut output_cases: Vec<(bool, String)> = Vec::new();
    for case in cases {
        let Some(op) = ops.get(&case.name) else { continue };
        let Some(rust::Type::Struct(ty)) = types.get(&op.output) else {
            continue;
        };

        let mut fields = String::new();
        let mut uses_owned = false;
        let mut set = 0_usize;
        for field in &ty.fields {
            if let Some((value, field_uses_owned)) = output_field_assign(types, field) {
                uses_owned |= field_uses_owned;
                set += 1;
                let _ = writeln!(fields, "        {}: {value},", field.name);
            }
        }

        let parameter = if uses_owned { "owned" } else { "_owned" };
        if case.is_minio {
            g!("#[cfg(feature = \"minio\")]");
        }
        g!("fn output_{}({parameter}: bool) -> dto::{} {{", op.name.to_snake_case(), op.output);
        if set == 0 {
            // No serialized field of the output can be set from the model.
            g!("    dto::{}::default()", op.output);
        } else {
            g!("    dto::{} {{", op.output);
            g!("{}", fields.trim_end());
            if set < ty.fields.len() {
                g!("        ..Default::default()");
            }
            g!("    }}");
        }
        g!("}}");
        g!();

        let header = ty
            .fields
            .iter()
            .find(|field| field.position == "header")
            .and_then(|field| field.http_header.clone());
        // GetObject::serialize_http maps a set content_range to 206 Partial
        // Content, so the populated output answers with that status.
        let status = if op.name == "GetObject" && ty.fields.iter().any(|field| field.name == "content_range") {
            206
        } else {
            op.http_code
        };
        let header = match header {
            Some(header) => format!("Some({header:?})"),
            None => "None".to_owned(),
        };
        output_cases.push((
            case.is_minio,
            format!("OutputCase {{ name: {:?}, header: {header}, status: {status} }},", case.name),
        ));
    }

    g([
        "",
        "/// An S3 double that answers every operation with a populated output, so the",
        "/// header conversions and the body writers of the operation output run.",
        "struct SampleS3 {",
        "    owned: bool,",
        "}",
        "",
        "#[async_trait::async_trait]",
        "impl S3 for SampleS3 {",
    ]);
    for op in ops.values() {
        if op.is_minio {
            g!("#[cfg(feature = \"minio\")]");
        }
        g!(
            "async fn {}(&self, _req: S3Request<{}>) -> S3Result<S3Response<{}>> {{",
            op.name.to_snake_case(),
            op.input,
            op.output
        );
        if case_names.contains(op.name.as_str()) && matches!(types.get(&op.output), Some(rust::Type::Struct(_))) {
            g!("    Ok(S3Response::new(output_{}(self.owned)))", op.name.to_snake_case());
        } else {
            g!("    Ok(S3Response::new({}::default()))", op.output);
        }
        g!("}}");
        g!();
    }
    g(["}", ""]);

    g([
        "/// The populated output of one request case: the first response header the",
        "/// output must produce, and the status the request must answer with.",
        "struct OutputCase {",
        "    name: &'static str,",
        "    header: Option<&'static str>,",
        "    status: u16,",
        "}",
        "",
        "/// One entry per request case, in the same order.",
        "const OUTPUT_CASES: &[OutputCase] = &[",
    ]);
    for (is_minio, entry) in &output_cases {
        if *is_minio {
            g!("#[cfg(feature = \"minio\")]");
        }
        g!("    {entry}");
    }
    g(["];", ""]);

    g([
        "/// Every operation serializes a populated output: the request answers with the",
        "/// modeled status, the response carries the header the model binds first, and",
        "/// the string enums are converted once from a borrowed and once from an owned",
        "/// string.",
        "#[tokio::test]",
        "async fn every_operation_serializes_a_populated_output() {",
        "    let mut failures: Vec<String> = Vec::new();",
        "    for owned in [false, true] {",
        "        for (case, output) in OP_REQUESTS.iter().zip(OUTPUT_CASES) {",
        "            assert_eq!(case.name, output.name, \"the case table must match the request table\");",
        "            let mut builder = S3ServiceBuilder::new(SampleS3 { owned });",
        "            builder.set_access(PassThroughAccess);",
        "            if case.literal_path {",
        "                builder.set_validation(PermissiveValidation);",
        "            }",
        "            let service = builder.build();",
        "",
        "            let resp = service.call(request_of(case)).await.expect(\"service call\");",
        "            let status = resp.status();",
        "            if status.as_u16() != output.status {",
        "                failures.push(format!(\"{} (owned={owned}): status {status}, expected {}\", output.name, output.status));",
        "                continue;",
        "            }",
        "            if let Some(header) = output.header",
        "                && !resp.headers().contains_key(header)",
        "            {",
        "                failures.push(format!(\"{} (owned={owned}): missing header {header}\", output.name));",
        "            }",
        "        }",
        "    }",
        "    assert!(failures.is_empty(), \"{} populated output(s) failed:\\n{}\" , failures.len(), failures.join(\"\\n\"));",
        "}",
        "",
    ]);
}

/// Emits the case of the operations whose optional payload the wire protocol
/// still requires: an empty body must answer with `MalformedXML`.
fn emit_required_body_case() {
    g([
        "/// Operations whose optional payload still needs a non-empty body.",
        "const REQUIRES_BODY: &[&str] = &[",
    ]);
    for name in REQUIRES_BODY {
        g!("    {name:?},");
    }
    g(["];", ""]);

    g([
        "/// The request of one case with an empty body.",
        "fn bodyless_request_of(case: &OpRequest) -> s3s::HttpRequest {",
        "    let mut builder = http::Request::builder().method(case.method).uri(case.uri);",
        "    for (name, value) in case.headers {",
        "        builder = builder.header(*name, *value);",
        "    }",
        "    builder.body(Body::from(Vec::new())).expect(\"valid request\")",
        "}",
        "",
        "/// An operation whose payload the model declares optional still requires a",
        "/// non-empty body, and the generated deserializer maps a missing one to",
        "/// MalformedXML instead of the missing-body error.",
        "#[tokio::test]",
        "async fn required_payloads_reject_a_missing_body() {",
        "    let mut failures: Vec<String> = Vec::new();",
        "    let mut seen = 0_usize;",
        "    for case in OP_REQUESTS.iter().filter(|case| REQUIRES_BODY.contains(&case.name)) {",
        "        seen += 1;",
        "        let mut builder = S3ServiceBuilder::new(SucceedingS3);",
        "        builder.set_access(PassThroughAccess);",
        "        if case.literal_path {",
        "            builder.set_validation(PermissiveValidation);",
        "        }",
        "        let service = builder.build();",
        "",
        "        let resp = service.call(bodyless_request_of(case)).await.expect(\"service call\");",
        "        let status = resp.status();",
        "        let body = response_text(resp).await;",
        "        if status != http::StatusCode::BAD_REQUEST || !body.contains(\"<Code>MalformedXML</Code>\") {",
        "            failures.push(format!(\"{}: status {status}, body {body}\", case.name));",
        "        }",
        "    }",
        "    assert_eq!(seen, REQUIRES_BODY.len(), \"every listed operation must have a request case\");",
        "    assert!(failures.is_empty(), \"{} operation(s) failed:\\n{}\" , failures.len(), failures.join(\"\\n\"));",
        "}",
        "",
    ]);
}
