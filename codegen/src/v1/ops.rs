// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

use super::dto::RustTypes;
use super::rust::default_value_literal;
use super::smithy::SmithyTraitsExt;
use super::xml::{is_xml_output, is_xml_payload};
use super::{dto, rust, smithy};
use super::{headers, o, write_dir_file};

use crate::v2::post_object;

use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::format as f;
use std::ops::Not;

use heck::ToSnakeCase;
use scoped_writer::g;
use stdx::default::default;

#[derive(Debug, Clone)]
pub struct Operation {
    pub name: String,

    pub input: String,
    pub output: String,

    pub smithy_input: String,
    pub smithy_output: String,

    pub s3_unwrapped_xml_output: bool,

    pub doc: Option<String>,

    pub http_method: String,
    pub http_uri: String,
    pub http_code: u16,

    /// Whether this operation is a `MinIO` extension (only present in the `MinIO` model variant).
    pub is_minio: bool,
}

pub type Operations = BTreeMap<String, Operation>;

pub const SKIPPED_OPS: &[&str] = &[];

pub fn collect_operations(model: &smithy::Model) -> Operations {
    let mut operations: Operations = default();
    let insert = |operations: &mut Operations, name: String, op: Operation| {
        assert!(operations.insert(name, op).is_none());
    };

    for (shape_name, shape) in &model.shapes {
        let smithy::Shape::Operation(sh) = shape else { continue };

        let op_name = dto::to_type_name(shape_name).to_owned();

        if SKIPPED_OPS.contains(&op_name.as_str()) {
            continue;
        }

        let cvt = |n| {
            if n == "smithy.api#Unit" {
                o("Unit")
            } else {
                o(dto::to_type_name(n))
            }
        };

        let smithy_input = cvt(sh.input.target.as_str());
        let smithy_output = cvt(sh.output.target.as_str());

        let input = {
            if smithy_input != "Unit" {
                assert_eq!(smithy_input.strip_suffix("Request").unwrap(), op_name);
            }
            f!("{op_name}Input")
        };

        let output = {
            if smithy_output != "Unit" && smithy_output != "NotificationConfiguration" {
                // The upstream model mostly names op outputs `<Op>Output`, but a few
                // use `<Op>Response`; both are normalized to the `<Op>Output` convention.
                let op_base = smithy_output
                    .strip_suffix("Output")
                    .or_else(|| smithy_output.strip_suffix("Response"))
                    .unwrap();
                assert_eq!(op_base, op_name);
            }
            f!("{op_name}Output")
        };

        // See https://github.com/awslabs/smithy-rs/discussions/2308
        let smithy_http_code = sh.traits.http_code().unwrap();
        let http_code = if op_name == "PutBucketPolicy" {
            assert_eq!(smithy_output, "Unit");
            assert_eq!(smithy_http_code, 200);
            204
        } else {
            smithy_http_code
        };

        // https://smithy.io/2.0/aws/customizations/s3-customizations.html
        // https://github.com/Nugine/s3s/pull/127
        if sh.traits.s3_unwrapped_xml_output() {
            assert_eq!(op_name, "GetBucketLocation");
        }

        let op = Operation {
            name: op_name.clone(),

            input,
            output,

            smithy_input,
            smithy_output,

            s3_unwrapped_xml_output: sh.traits.s3_unwrapped_xml_output(),

            doc: sh.traits.doc().map(o),

            http_method: sh.traits.http_method().unwrap().to_owned(),
            http_uri: sh.traits.http_uri().unwrap().to_owned(),
            http_code,

            is_minio: sh.traits.minio(),
        };
        insert(&mut operations, op_name, op);
    }

    // The synthetic multipart operation is not in the upstream model; the v2 module owns it.
    post_object::inject_operation(&mut operations);

    operations
}

pub fn is_op_input(name: &str, ops: &Operations) -> bool {
    name.strip_suffix("Input").is_some_and(|x| ops.contains_key(x))
}

pub fn is_op_output(name: &str, ops: &Operations) -> bool {
    name.strip_suffix("Output").is_some_and(|x| ops.contains_key(x))
}

/// Generate the merged operation code from the union (`MinIO`) model.
///
/// `ops` is the union operation set (base ⊆ `MinIO`). `rust_types_base` and
/// `rust_types_minio` are the rust type collections of the two model
/// variants; per-operation differences between them are emitted inline with
/// mutually exclusive `#[cfg(feature = "minio")]` / `#[cfg(not(feature = "minio"))]`
/// gates instead of generating two files and merging them textually.
// Visible to `v2::post_object`, which emits the operation module with them.
pub(crate) const OPS_GENERATED_DIR: &str = "crates/s3s/src/ops/generated";

pub(crate) fn codegen_file_header() {
    g!("// SPDX-License-Identifier: Apache-2.0");
    g!("// SPDX-FileCopyrightText: 2023-2026 The s3s Authors");
    g!();
    g!("//! Auto generated by `s3s_codegen::v1::ops::codegen`");
    g!();
}

pub fn codegen(ops: &Operations, rust_types_base: &RustTypes, rust_types_minio: &RustTypes) {
    write_dir_file(OPS_GENERATED_DIR, "header_value.rs", || {
        codegen_file_header();
        g(["use crate::dto::*;", "use crate::http;", "use std::borrow::Cow;", ""]);
        codegen_header_value(ops, rust_types_minio);
    });

    post_object::codegen_fork_op(rust_types_minio);
    codegen_http(ops, rust_types_base, rust_types_minio);
    codegen_router(ops, rust_types_minio);
    crate::v1::oir::codegen_oir(ops);

    write_dir_file(OPS_GENERATED_DIR, "mod.rs", || {
        codegen_file_header();

        for op in ops.values() {
            if op.is_minio.not() {
                g!("// {}", op.name);
            }
        }
        g!();

        g([
            "#![allow(clippy::declare_interior_mutable_const)]",
            "#![allow(clippy::borrow_interior_mutable_const)]",
            "#![allow(clippy::needless_pass_by_value)]",
            "#![allow(clippy::too_many_lines)]",
            "#![allow(clippy::collapsible_if)]",
            "#![allow(clippy::unnecessary_wraps)]",
            "#![allow(clippy::unreadable_literal)]",
            "#![deny(clippy::unwrap_used)]",
            "#![deny(clippy::expect_used)]",
            "#![deny(clippy::indexing_slicing)]",
            "#![deny(clippy::panic)]",
            "#![deny(clippy::unreachable)]",
            "",
        ]);

        for op in ops.values() {
            if post_object::is_synthetic(&op.name) {
                continue;
            }
            g!("mod {};", op.name.to_snake_case());
        }
        g!("mod header_value;");
        g!("mod post_object;");
        g!("mod router;");
        g!("mod oir;");
        g!();

        for op in ops.values() {
            if post_object::is_synthetic(&op.name) {
                continue;
            }
            if op.is_minio {
                g!("#[cfg(feature = \"minio\")]");
            }
            g!("pub use self::{}::{};", op.name.to_snake_case(), op.name);
        }
        g!("pub use self::post_object::PostObject;");
        g!("pub use self::router::resolve_route;");
        g!("pub use self::oir::resolve_operation_by_id;");
        g!();
    });
}

/// Whether the operation input struct differs between the base and `MinIO` model variants.
fn op_input_differs(op: &Operation, rust_types_base: &RustTypes, rust_types_minio: &RustTypes) -> bool {
    assert!(op.is_minio.not());
    let (rust::Type::Struct(base), rust::Type::Struct(minio)) =
        (&rust_types_base[op.input.as_str()], &rust_types_minio[op.input.as_str()])
    else {
        panic!("op input must be a struct");
    };

    // The gate granularity only covers `deserialize_http` /
    // `deserialize_http_multipart`. The serialize path depends on the output
    // struct, so a divergence there would need new gate kinds — fail loudly
    // instead of silently dropping the MinIO side.
    let (rust::Type::Struct(base_out), rust::Type::Struct(minio_out)) =
        (&rust_types_base[op.output.as_str()], &rust_types_minio[op.output.as_str()])
    else {
        panic!("op output must be a struct");
    };
    assert_eq!(
        base_out.fields, minio_out.fields,
        "output struct of {} differs between base and minio; extend the gate granularity",
        op.name
    );

    base.fields != minio.fields
}

/// Whether the operation's input or output struct carries header-position fields.
/// Used to decide whether the per-operation file needs `use crate::header::*;`.
fn op_has_header_fields(op: &Operation, rust_types: &RustTypes) -> bool {
    let has_header = |name: &str| {
        let rust::Type::Struct(ty) = &rust_types[name] else { return false };
        ty.fields.iter().any(|field| field.position == "header")
    };
    has_header(op.input.as_str()) || has_header(op.output.as_str())
}

fn codegen_http(ops: &Operations, rust_types_base: &RustTypes, rust_types_minio: &RustTypes) {
    for op in ops.values() {
        if post_object::is_synthetic(&op.name) {
            continue;
        }
        codegen_op_unit(op, rust_types_base, rust_types_minio);
    }
}

#[allow(clippy::too_many_lines)]
fn codegen_op_unit(op: &Operation, rust_types_base: &RustTypes, rust_types_minio: &RustTypes) {
    let file = op.name.to_snake_case();
    write_dir_file(OPS_GENERATED_DIR, &format!("{file}.rs"), || {
        codegen_file_header();
        g!("// {}", op.name);
        g!();
        // MinIO-only operations keep no items under `cfg(not(minio))`; their
        // use statements must be gated as well to avoid unused-import warnings.
        let gate = if op.is_minio { "#[cfg(feature = \"minio\")]\n" } else { "" };
        if op_has_header_fields(op, rust_types_minio) {
            g!("{gate}use crate::header::*;");
        }
        g!("{gate}use crate::dto::*;");
        g!("{gate}use crate::error::*;");
        g!("{gate}use crate::http;");
        g!("{gate}use crate::ops::CallContext;");
        g!();

        if op.is_minio {
            g!("#[cfg(feature = \"minio\")]");
        }
        g!("pub struct {};", op.name);
        g!();

        if op.is_minio {
            g!("#[cfg(feature = \"minio\")]");
        }
        g!("impl {} {{", op.name);

        let input_differs = op.is_minio.not() && op_input_differs(op, rust_types_base, rust_types_minio);

        if input_differs {
            g!("#[cfg(not(feature = \"minio\"))]");
            codegen_op_http_de_fn(op, rust_types_base);
            g!("#[cfg(feature = \"minio\")]");
            codegen_op_http_de_fn(op, rust_types_minio);
            if op.name == "PutObject" {
                g!();
                g!("#[cfg(not(feature = \"minio\"))]");
                codegen_op_http_de_multipart(op, rust_types_base);
                g!();
                g!("#[cfg(feature = \"minio\")]");
                codegen_op_http_de_multipart(op, rust_types_minio);
            }
            g!();
        } else {
            codegen_op_http_de(op, rust_types_minio);
        }

        codegen_op_http_ser(op, rust_types_minio);

        g!("}}");
        g!();

        codegen_op_http_call(op, rust_types_minio);
        g!();
    });
}

fn status_code_name(code: u16) -> &'static str {
    match code {
        200 => "OK",
        204 => "NO_CONTENT",
        _ => unimplemented!(),
    }
}

fn codegen_header_value(ops: &Operations, rust_types: &RustTypes) {
    let mut str_enum_names: BTreeSet<&str> = default();

    for op in ops.values() {
        for ty_name in [op.input.as_str(), op.output.as_str()] {
            let rust_type = &rust_types[ty_name];
            match rust_type {
                rust::Type::Provided(_) => {}
                rust::Type::Struct(ty) => {
                    for field in ty.fields.iter().filter(|field| field.position == "header") {
                        let field_type = &rust_types[field.type_.as_str()];
                        match field_type {
                            rust::Type::List(list_ty) => {
                                let member_type = &rust_types[list_ty.member.type_.as_str()];
                                if let rust::Type::StrEnum(ty) = member_type {
                                    str_enum_names.insert(ty.name.as_str());
                                }
                            }
                            rust::Type::StrEnum(ty) => {
                                str_enum_names.insert(ty.name.as_str());
                            }
                            rust::Type::Alias(_) => {}
                            rust::Type::Provided(_) => {}
                            rust::Type::Timestamp(_) => {}
                            _ => unimplemented!("{field_type:#?}"),
                        }
                    }
                }
                _ => unimplemented!(),
            }
        }
    }

    // Each enum emits its `TryIntoHeaderValue` impl immediately followed by its
    // `TryFromHeaderValue` impl, matching the interleaved layout of the merged file.
    for rust_type in str_enum_names.iter().map(|&x| &rust_types[x]) {
        let rust::Type::StrEnum(ty) = rust_type else { panic!() };

        // MinIO-only enums are gated so that no-minio builds keep compiling
        // (none exist today; this is a defensive guard for model evolution).
        if ty.is_custom_extension {
            g!("#[cfg(feature = \"minio\")]");
        }
        g!("impl http::TryIntoHeaderValue for {} {{", ty.name);
        g!("type Error = http::InvalidHeaderValue;");
        g!("fn try_into_header_value(self) -> Result<http::HeaderValue, Self::Error> {{");
        g!("    match Cow::from(self) {{");
        g!("        Cow::Borrowed(s) => http::HeaderValue::try_from(s),");
        g!("        Cow::Owned(s) => http::HeaderValue::try_from(s),");
        g!("    }}");
        g!("}}");
        g!("}}");
        g!();

        if ty.is_custom_extension {
            g!("#[cfg(feature = \"minio\")]");
        }
        g!("impl http::TryFromHeaderValue for {} {{", ty.name);
        g!("type Error = http::ParseHeaderError;");
        g!("fn try_from_header_value(val: &http::HeaderValue) -> Result<Self, Self::Error> {{");
        g!("    let val = val.to_str().map_err(|_|http::ParseHeaderError::Enum)?;");
        g!("    Ok(Self::from(val.to_owned()))");
        g!("}}");
        g!("}}");
        g!();
    }
}

fn codegen_op_http_ser_unit(op: &Operation) {
    g!("pub fn serialize_http() -> http::Response {{");

    if op.http_code == 200 {
        g!("http::Response::default()");
    } else {
        g!("let mut res = http::Response::default();");
        g!("res.status = http::StatusCode::{};", status_code_name(op.http_code));
        g!("res");
    }

    g!("}}");
}

#[allow(clippy::too_many_lines)]
fn codegen_op_http_ser(op: &Operation, rust_types: &RustTypes) {
    let output = op.output.as_str();
    let rust_type = &rust_types[output];
    match rust_type {
        rust::Type::Provided(ty) => {
            assert_eq!(ty.name, "Unit");
            codegen_op_http_ser_unit(op);
        }
        rust::Type::Struct(ty) => {
            if op.name == "CompleteMultipartUpload" {
                return; // custom implementation
            }

            if ty.fields.is_empty() {
                g!("pub fn serialize_http(_: {output}) -> S3Result<http::Response> {{");
                {
                    let code_name = status_code_name(op.http_code);
                    g!("Ok(http::Response::with_status(http::StatusCode::{code_name}))");
                }
                g!("}}");
            } else {
                g!("pub fn serialize_http(x: {output}) -> S3Result<http::Response> {{");

                assert!(ty.fields.is_empty().not());
                for field in &ty.fields {
                    assert!(["header", "metadata", "xml", "payload", "s3s"].contains(&field.position.as_str()),);
                }

                if op.name == "GetObject" {
                    assert_eq!(op.http_code, 200);
                    g!("let mut res = http::Response::default();");
                    // https://github.com/Nugine/s3s/issues/118
                    g!("if x.content_range.is_some() {{");
                    g!("    res.status = http::StatusCode::PARTIAL_CONTENT;");
                    g!("}}");
                } else {
                    let code_name = status_code_name(op.http_code);
                    g!("let mut res = http::Response::with_status(http::StatusCode::{code_name});");
                }

                if is_xml_output(ty) {
                    if op.name == "CompleteMultipartUpload" {
                        g!("http::set_xml_body_no_decl(&mut res, &x)?;");
                    } else {
                        g!("http::set_xml_body(&mut res, &x)?;");
                    }
                } else if let Some(field) = ty.fields.iter().find(|x| x.position == "payload") {
                    match field.type_.as_str() {
                        "Policy" => {
                            assert!(field.option_type);
                            g!("if let Some(val) = x.{} {{", field.name);
                            g!("res.body = http::Body::from(val);");
                            g!("}}");
                        }
                        "StreamingBlob" => {
                            if field.option_type {
                                g!("if let Some(val) = x.{} {{", field.name);
                                g!("http::set_stream_body(&mut res, val);");
                                g!("}}");
                            } else {
                                g!("http::set_stream_body(&mut res, x.{});", field.name);
                            }
                        }
                        "SelectObjectContentEventStream" => {
                            assert!(field.option_type);
                            g!("if let Some(val) = x.{} {{", field.name);
                            g!("http::set_event_stream_body(&mut res, val);");
                            g!("}}");
                        }
                        _ => {
                            if field.option_type {
                                g!("if let Some(ref val) = x.{} {{", field.name);
                                g!("    http::set_xml_body(&mut res, val)?;");
                                g!("}}");
                            } else {
                                g!("http::set_xml_body(&mut res, &x.{})?;", field.name);
                            }
                        }
                    }
                }

                for field in &ty.fields {
                    if field.position == "header" {
                        let field_name = field.name.as_str();
                        let header_name = headers::to_constant_name(field.http_header.as_deref().unwrap());

                        let field_type = &rust_types[field.type_.as_str()];
                        if let rust::Type::Timestamp(ts_ty) = field_type {
                            assert!(field.option_type);
                            let fmt = ts_ty.format.as_deref().unwrap_or("HttpDate");
                            g!(
                                "http::add_opt_header_timestamp(&mut res, {header_name}, x.{field_name}, TimestampFormat::{fmt})?;"
                            );
                        } else if field.option_type {
                            g!("http::add_opt_header(&mut res, {header_name}, x.{field_name})?;");
                        } else {
                            g!("http::add_header(&mut res, {header_name}, x.{field_name})?;");
                        }
                    }
                    if field.position == "metadata" {
                        assert!(field.option_type);
                        g!("http::add_opt_metadata(&mut res, x.{})?;", field.name);
                    }
                }

                g!("Ok(res)");

                g!("}}");
            }
        }
        _ => unimplemented!(),
    }
    g!();
}

#[allow(clippy::too_many_lines)]
fn codegen_op_http_de(op: &Operation, rust_types: &RustTypes) {
    codegen_op_http_de_fn(op, rust_types);
    if op.name == "PutObject" {
        codegen_op_http_de_multipart(op, rust_types);
    }
    g!();
}

/// The header fields whose value must not fail the request when it cannot be a valid date.
///
/// RFC 9110 requires a recipient to ignore an `If-Modified-Since` or `If-Unmodified-Since` value
/// that is not a valid HTTP date (§13.1.3, §13.1.4), and the service ignores such a value in the
/// copy-source and rename-source conditions as well.
const LENIENT_TIMESTAMP_HEADERS: [&str; 6] = [
    "if-modified-since",
    "if-unmodified-since",
    "x-amz-copy-source-if-modified-since",
    "x-amz-copy-source-if-unmodified-since",
    "x-amz-rename-source-if-modified-since",
    "x-amz-rename-source-if-unmodified-since",
];

fn is_lenient_timestamp_header(name: &str) -> bool {
    LENIENT_TIMESTAMP_HEADERS
        .iter()
        .any(|header| name.eq_ignore_ascii_case(header))
}

#[allow(clippy::too_many_lines)]
fn codegen_op_http_de_fn(op: &Operation, rust_types: &RustTypes) {
    let input = op.input.as_str();
    let rust_type = &rust_types[input];
    match rust_type {
        rust::Type::Provided(ty) => {
            assert_eq!(ty.name, "Unit");
        }
        rust::Type::Struct(ty) => {
            if ty.fields.is_empty() {
                g!("pub fn deserialize_http(_: &mut http::Request) -> S3Result<{input}> {{");
                g!("Ok({input} {{}})");
                g!("}}");
            } else {
                g!("pub fn deserialize_http(req: &mut http::Request) -> S3Result<{input}> {{");

                if op.name == "PutObject" {
                    // POST object
                    g!("if let Some(m) = req.s3ext.multipart.take() {{");
                    g!("    return Self::deserialize_http_multipart(req, m);");
                    g!("}}");
                    g!();
                }

                let path_pattern = PathPattern::parse(op.http_uri.as_str());
                match path_pattern {
                    PathPattern::Root => {}
                    PathPattern::Bucket => {
                        if op.name != "WriteGetObjectResponse" {
                            g!("let bucket = http::unwrap_bucket(req);");
                            g!();
                        }
                    }
                    PathPattern::Object => {
                        g!("let (bucket, key) = http::unwrap_object(req);");
                        g!();
                    }
                }

                for field in &ty.fields {
                    match field.position.as_str() {
                        "bucket" => {
                            assert_eq!(field.name, "bucket");
                        }
                        "key" => {
                            assert_eq!(field.name, "key");
                        }
                        "query" => {
                            codegen_field_de_query(field, rust_types);
                        }
                        "header" => {
                            let header = headers::to_constant_name(field.http_header.as_deref().unwrap());
                            let field_type = &rust_types[&field.type_];

                            if let rust::Type::List(_) = field_type {
                                assert!(field.name == "object_attributes" || field.name == "optional_object_attributes");
                                if field.is_required {
                                    assert!(field.option_type.not());
                                    g!("let {}: {} = http::parse_list_header(req, &{header})?;", field.name, field.type_,);
                                } else {
                                    assert!(field.option_type);
                                    g!(
                                        "let {}: Option<{}> = http::parse_opt_list_header(req, &{header})?;",
                                        field.name,
                                        field.type_,
                                    );
                                }
                            } else if let rust::Type::Timestamp(ts_ty) = field_type {
                                assert!(field.option_type);
                                let fmt = ts_ty.format.as_deref().unwrap_or("HttpDate");
                                match field.http_header.as_deref() {
                                    // A conditional date that cannot be a valid date is ignored
                                    // instead of rejected (RFC 9110 §13.1.3, §13.1.4, Amazon S3):
                                    // that parser cannot fail, so the call carries no `?`.
                                    Some(name) if is_lenient_timestamp_header(name) => {
                                        g!(
                                            "let {}: Option<{}> = http::parse_opt_header_timestamp_ignoring_invalid(req, &{}, TimestampFormat::{});",
                                            field.name,
                                            field.type_,
                                            header,
                                            fmt
                                        );
                                    }
                                    _ => {
                                        g!(
                                            "let {}: Option<{}> = http::parse_opt_header_timestamp(req, &{}, TimestampFormat::{})?;",
                                            field.name,
                                            field.type_,
                                            header,
                                            fmt
                                        );
                                    }
                                }
                            } else if field.option_type {
                                if field.name == "checksum_algorithm" {
                                    g!(
                                        "let {}: Option<{}> = http::parse_checksum_algorithm_header(req)?;",
                                        field.name,
                                        field.type_,
                                    );
                                } else if field.name == "range" {
                                    // A Range field that cannot be served as a single byte range
                                    // is ignored instead of rejected, as Amazon S3 does.
                                    g!("let {}: Option<{}> = http::parse_opt_range_header(req);", field.name, field.type_);
                                } else {
                                    g!(
                                        "let {}: Option<{}> = http::parse_opt_header(req, &{})?;",
                                        field.name,
                                        field.type_,
                                        header
                                    );
                                }
                            } else if let Some(ref default_value) = field.default_value {
                                // ASK: content length
                                // In S3 smithy model, content-length has a default value (0).
                                // Why? Is it correct???

                                let literal = default_value_literal(default_value);
                                g!(
                                    "let {}: {} = http::parse_opt_header(req, &{})?.unwrap_or({});",
                                    field.name,
                                    field.type_,
                                    header,
                                    literal,
                                );
                            } else {
                                g!("let {}: {} = http::parse_header(req, &{})?;", field.name, field.type_, header);
                            }
                        }
                        "metadata" => {
                            assert!(field.option_type);
                            g!("let {}: Option<{}> = http::parse_opt_metadata(req)?;", field.name, field.type_);
                        }
                        "payload" => match field.type_.as_str() {
                            "Policy" => {
                                assert!(field.option_type.not());
                                g!("let {}: {} = http::take_string_body(req)?;", field.name, field.type_);
                            }
                            "StreamingBlob" => {
                                assert!(field.option_type);
                                g!("let {}: Option<{}> = Some(http::take_stream_body(req));", field.name, field.type_);
                            }
                            _ => {
                                // AWS S3 returns MalformedXML for empty bodies on these operations,
                                // which differs from the default behavior where empty optional XML bodies are accepted.
                                // - CompleteMultipartUpload: requires XML body with list of uploaded parts
                                // - PutObjectLegalHold: requires XML body with ON/OFF legal hold status
                                // - PutObjectRetention: requires XML body with retention mode and date
                                let requires_body = matches!(
                                    (op.name.as_str(), field.name.as_str()),
                                    ("CompleteMultipartUpload", "multipart_upload")
                                        | ("PutObjectLegalHold", "legal_hold")
                                        | ("PutObjectRetention", "retention")
                                );

                                if requires_body {
                                    // These operations require XML body to match AWS S3 behavior; empty body should return MalformedXML instead of being treated as optional
                                    assert!(field.option_type);
                                    g!("let {}: Option<{}> = match http::take_xml_body(req) {{", field.name, field.type_);
                                    g!("    Ok(body) => Some(body),");
                                    g!("    Err(e) if *e.code() == crate::S3ErrorCode::MissingRequestBodyError => {{");
                                    g!("        return Err(crate::S3ErrorCode::MalformedXML.into());");
                                    g!("    }}");
                                    g!("    Err(e) => return Err(e),");
                                    g!("}};");
                                } else if let Some(literal) = &field.body_literal {
                                    if field.option_type {
                                        g!(
                                            "let {}: Option<{}> = http::take_opt_body_literal::<{}>(req, \"{literal}\")?;",
                                            field.name,
                                            field.type_,
                                            field.type_
                                        );
                                    } else {
                                        g!(
                                            "let {}: {} = http::take_body_literal::<{}>(req, \"{literal}\")?;",
                                            field.name,
                                            field.type_,
                                            field.type_
                                        );
                                    }
                                } else if field.option_type {
                                    g!("let {}: Option<{}> = http::take_opt_xml_body(req)?;", field.name, field.type_);
                                } else {
                                    g!("let {}: {} = http::take_xml_body(req)?;", field.name, field.type_);
                                }
                            }
                        },

                        _ => unimplemented!(),
                    }
                    g!();
                }

                let has_range = ty.fields.iter().any(|field| field.name == "range");
                let has_part_number = ty.fields.iter().any(|field| field.name == "part_number");
                if has_range && has_part_number {
                    // AWS answers 400 InvalidRequest when both are present.
                    g!("if range.is_some() && part_number.is_some() {{");
                    g!(
                        "    return Err(s3_error!(InvalidRequest, \"Cannot specify both Range header and partNumber query parameter\"));"
                    );
                    g!("}}");
                    g!();
                }

                g!("Ok({input} {{");
                for field in &ty.fields {
                    match field.position.as_str() {
                        "bucket" | "key" | "query" | "header" | "metadata" | "payload" => {
                            g!("{},", field.name);
                        }
                        _ => unimplemented!(),
                    }
                }
                g!("}})");

                g!("}}");
                g!();
            }
        }
        _ => unimplemented!(),
    }
}

fn codegen_field_de_query(field: &rust::StructField, rust_types: &RustTypes) {
    let query = field.http_query.as_deref().unwrap();

    let field_type = &rust_types[&field.type_];

    if let Some(ref separator) = field.query_joined {
        assert!(field.option_type);
        g!(
            "let {}: Option<{}> = http::parse_opt_query_joined(req, \"{}\", \"{}\");",
            field.name,
            field.type_,
            query,
            separator,
        );
    } else if let rust::Type::List(_) = field_type {
        panic!()
    } else if let rust::Type::Timestamp(ts_ty) = field_type {
        assert!(field.option_type);
        let fmt = ts_ty.format.as_deref().unwrap_or("DateTime");
        g!(
            "let {}: Option<{}> = http::parse_opt_query_timestamp(req, \"{}\", TimestampFormat::{})?;",
            field.name,
            field.type_,
            query,
            fmt
        );
    } else if field.option_type {
        g!(
            "let {}: Option<{}> = http::parse_opt_query(req, \"{}\")?;",
            field.name,
            field.type_,
            query
        );
    } else if let Some(ref default_value) = field.default_value {
        let literal = default_value_literal(default_value);
        g!(
            "let {}: {} = http::parse_opt_query(req, \"{}\")?.unwrap_or({});",
            field.name,
            field.type_,
            query,
            literal,
        );
    } else {
        g!("let {}: {} = http::parse_query(req, \"{}\")?;", field.name, field.type_, query,);
    }
}

fn codegen_op_http_de_multipart(op: &Operation, rust_types: &RustTypes) {
    assert_eq!(op.name, "PutObject");

    g!(
        "pub fn deserialize_http_multipart(req: &mut http::Request, m: http::Multipart) -> S3Result<{}> {{",
        op.input
    );

    g([
        "let bucket = http::unwrap_bucket(req);",
        "let key = http::parse_field_value(&m, \"key\")?.ok_or_else(|| invalid_request!(\"missing key\"))?;",
        "",
        "let body_stream = req.s3ext.post_object_stream.take().ok_or_else(|| s3_error!(InternalError, \"missing body stream\"))?;",
        "",
        "let content_length = body_stream",
        "    .remaining_length()",
        "    .exact()",
        "    .map(i64::try_from)",
        "    .transpose()",
        "    .map_err(|e| s3_error!(e, InvalidArgument, \"content-length overflow\"))?;",
        "let content_length = content_length.filter(|&n| n != 0);",
        "",
        "let body: Option<StreamingBlob> = Some(body_stream.into());",
        "",
    ]);

    let rust::Type::Struct(ty) = &rust_types[op.input.as_str()] else { panic!() };

    for field in &ty.fields {
        match field.position.as_str() {
            "bucket" | "key" | "payload" => {}
            "query" => {
                codegen_field_de_query(field, rust_types);
            }
            "header" => {
                let header = field.http_header.as_deref().unwrap();
                assert!(header.as_bytes().iter().all(|&x| x == b'-' || x.is_ascii_alphanumeric()));
                let header = header.to_ascii_lowercase();

                if header == "content-length" {
                    continue;
                }

                let field_type = &rust_types[field.type_.as_str()];

                if let rust::Type::Timestamp(ts_ty) = field_type {
                    assert!(field.option_type);
                    let fmt = ts_ty.format.as_deref().unwrap_or("HttpDate");
                    g!(
                        "let {}: Option<{}> = http::parse_field_value_timestamp(&m, \"{}\", TimestampFormat::{})?;",
                        field.name,
                        field.type_,
                        header,
                        fmt
                    );
                } else if field.option_type {
                    g!(
                        "let {}: Option<{}> = http::parse_field_value(&m, \"{}\")?;",
                        field.name,
                        field.type_,
                        header
                    );
                } else if let Some(ref default_value) = field.default_value {
                    g!(
                        "let {}: {} = http::parse_field_value(&m, \"{}\")?.unwrap_or({});",
                        field.name,
                        field.type_,
                        header,
                        default_value_literal(default_value)
                    );
                } else {
                    unimplemented!()
                }
            }
            "metadata" => {
                assert!(field.option_type);
                g!("let {}: Option<{}> = {{", field.name, field.type_);
                g!("    let mut metadata = {}::default();", field.type_);
                g([
                    "    for (name, value) in m.fields() {",
                    "        if let Some(key) = name.strip_prefix(\"x-amz-meta-\") {",
                    "            if key.is_empty() { continue; }",
                    "            metadata.insert(key.to_owned(), value.clone());",
                    "        }",
                    "    }",
                    "    if metadata.is_empty() { None } else { Some(metadata) }",
                    "};",
                ]);
            }
            _ => unimplemented!(),
        }
        g!();
    }

    g!("Ok({} {{", op.input);
    for field in &ty.fields {
        g!("{},", field.name);
    }
    g!("}})");
    g!("}}");
}

fn codegen_op_http_call(op: &Operation, rust_types: &RustTypes) {
    g!("#[async_trait::async_trait]");
    if op.is_minio {
        g!("#[cfg(feature = \"minio\")]");
    }
    g!("impl crate::ops::Operation for {} {{", op.name);

    g!("fn name(&self) -> &'static str {{");
    g!("\"{}\"", op.name);
    g!("}}");
    g!();

    g!("fn needs_full_body(&self) -> bool {{");
    g!("{}", needs_full_body(op, rust_types));
    g!("}}");
    g!();

    g!("fn has_request_payload(&self) -> bool {{");
    g!("{}", has_request_payload(op, rust_types));
    g!("}}");
    g!();

    g!("fn has_streaming_body(&self) -> bool {{");
    g!("{}", has_streaming_body(op, rust_types));
    g!("}}");
    g!();

    g!("async fn call(&self, ccx: &CallContext<'_>, req: &mut http::Request) -> S3Result<http::Response> {{");

    let method = op.name.to_snake_case();

    g!("let input = Self::deserialize_http(req)?;");
    g!("let mut s3_req = crate::ops::build_s3_request(input, req);");
    g!("let s3 = ccx.s3;");

    g!("if let Some(access) = ccx.access {{");
    g!("    access.{method}(&mut s3_req).await?;");
    g!("}}");

    if op.name == "GetObject" {
        g!("let overridden_headers = crate::ops::get_object::extract_overridden_response_headers(&s3_req)?;");
    }

    g!("let result = s3.{method}(s3_req).await;");

    g([
        "let s3_resp = match result {",
        "    Ok(val) => val,",
        "    Err(err) => return crate::ops::serialize_error_for_method(&req.method, err, false),",
        "};",
    ]);

    g!("let mut resp = Self::serialize_http(s3_resp.output)?;");
    g!("if let Some(status) = s3_resp.status {{");
    g!("    resp.status = status;");
    g!("}}");

    if op.name == "GetObject" {
        g!("resp.headers.extend(overridden_headers);");
        g!("crate::ops::get_object::merge_custom_headers(&mut resp, s3_resp.headers);");
    } else {
        g!("resp.headers.extend(s3_resp.headers);");
    }

    g!("resp.extensions.extend(s3_resp.extensions);");

    g!("if http::is_bodyless_status(resp.status) {{");
    g!("    http::strip_bodyless(&mut resp);");
    g!("}}");

    g!("Ok(resp)");

    g!("}}");

    g!("}}");
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(super) enum PathPattern {
    Root,
    Bucket,
    Object,
}

impl PathPattern {
    pub(super) fn parse(part: &str) -> Self {
        let path = match part.split_once('?') {
            None => part,
            Some((p, _)) => p,
        };

        assert!(path.starts_with('/'));

        if path == "/" {
            return Self::Root;
        }

        match path[1..].split_once('/') {
            None => Self::Bucket,
            Some(_) => Self::Object,
        }
    }

    fn query_tag(part: &str) -> Option<String> {
        let (_, q) = part.split_once('?')?;
        let qs: Vec<(String, String)> = serde_urlencoded::from_str(q).unwrap();
        assert!(qs.iter().filter(|(_, v)| v.is_empty()).count() <= 1);
        qs.into_iter().find(|(_, v)| v.is_empty()).map(|(n, _)| n)
    }

    fn query_patterns(part: &str) -> Vec<(String, String)> {
        let Some((_, q)) = part.split_once('?') else { return Vec::new() };
        let mut qs: Vec<(String, String)> = serde_urlencoded::from_str(q).unwrap();
        qs.retain(|(n, v)| n != "x-id" && v.is_empty().not());
        qs
    }

    pub(super) fn x_id_value(part: &str) -> Option<String> {
        let (_, q) = part.split_once('?')?;
        let qs: Vec<(String, String)> = serde_urlencoded::from_str(q).unwrap();
        qs.into_iter()
            .find(|(n, _)| n == "x-id")
            .map(|(_, v)| v)
            .filter(|v| v.is_empty().not())
    }
}

pub(super) struct Route<'a> {
    pub(super) op: &'a Operation,
    pub(super) query_tag: Option<String>,
    pub(super) query_patterns: Vec<(String, String)>,
    pub(super) x_id: Option<String>,
    pub(super) required_headers: Vec<&'a str>,
    pub(super) required_query_strings: Vec<&'a str>,
}

fn collect_routes<'a>(ops: &'a Operations, rust_types: &'a RustTypes) -> HashMap<String, HashMap<PathPattern, Vec<Route<'a>>>> {
    let mut ans: HashMap<String, HashMap<PathPattern, Vec<Route<'_>>>> = default();
    for op in ops.values() {
        // PostObject is resolved in ops::prepare() for multipart requests.
        // Do not put it into the generated router to avoid overlaps.
        if post_object::is_synthetic(&op.name) {
            continue;
        }
        let pat = PathPattern::parse(&op.http_uri);
        let map = ans.entry(op.http_method.clone()).or_default();
        let vec = map.entry(pat).or_default();

        vec.push(Route {
            op,
            query_tag: PathPattern::query_tag(&op.http_uri),
            query_patterns: PathPattern::query_patterns(&op.http_uri),
            x_id: PathPattern::x_id_value(&op.http_uri),

            required_headers: required_headers(op, rust_types),
            required_query_strings: required_query_strings(op, rust_types),
        });
    }
    for map in ans.values_mut() {
        for vec in map.values_mut() {
            vec.sort_by_key(|r| {
                let has_query_tag = r.query_tag.is_some();
                let has_query_patterns = r.query_patterns.is_empty().not();

                let priority = match (has_query_tag, has_query_patterns) {
                    (true, true) => 1,
                    (true, false) => 2,
                    (false, true) => 3,
                    (false, false) => 4,
                };

                (
                    priority,
                    Reverse(r.query_patterns.len()),
                    Reverse(r.required_query_strings.len()),
                    Reverse(r.required_headers.len()),
                    r.op.name.as_str(),
                )
            });
        }
    }
    ans
}

fn required_headers<'a>(op: &Operation, rust_types: &'a RustTypes) -> Vec<&'a str> {
    let input_type = &rust_types[op.input.as_str()];
    let rust::Type::Struct(ty) = input_type else { panic!() };

    let mut ans: Vec<&'a str> = default();
    for field in &ty.fields {
        let is_list = matches!(rust_types[field.type_.as_str()], rust::Type::List(_));

        let is_required = field.is_required || (field.option_type.not() && field.default_value.is_none() && is_list.not());

        if is_required && field.position == "header" {
            let header = field.http_header.as_deref().unwrap();
            ans.push(header);
        }
    }
    ans
}

fn required_query_strings<'a>(op: &Operation, rust_types: &'a RustTypes) -> Vec<&'a str> {
    let input_type = &rust_types[op.input.as_str()];
    let rust::Type::Struct(ty) = input_type else { panic!() };

    let mut ans: Vec<&'a str> = default();
    for field in &ty.fields {
        let is_required = field.option_type.not() && field.default_value.is_none();
        if is_required && field.position == "query" {
            let header = field.http_query.as_deref().unwrap();
            ans.push(header);
        }
    }
    ans
}

fn needs_full_body(op: &Operation, rust_types: &RustTypes) -> bool {
    if op.http_method == "GET" {
        return false;
    }

    let rust::Type::Struct(ty) = &rust_types[op.input.as_str()] else { panic!() };
    assert!(ty.xml_name.is_none());

    let has_xml_payload = ty.fields.iter().any(is_xml_payload);
    let has_string_payload = ty.fields.iter().any(|field| field.type_ == "Policy");
    has_xml_payload || has_string_payload
}

fn has_request_payload(op: &Operation, rust_types: &RustTypes) -> bool {
    let rust::Type::Struct(ty) = &rust_types[op.input.as_str()] else { return false };
    ty.fields.iter().any(|field| field.position == "payload")
}

/// Whether the operation streams a request body directly to the `S3`
/// implementation without buffering: its input carries a `StreamingBlob`
/// payload (`PutObject`, `UploadPart`, `WriteGetObjectResponse`).
fn has_streaming_body(op: &Operation, rust_types: &RustTypes) -> bool {
    if post_object::is_synthetic(&op.name) {
        // Synthetic operation: the body is the multipart file stream governed
        // by `post_object_max_file_size`, not the request body wrapped by
        // `put_object_max_size`.
        return false;
    }

    let rust::Type::Struct(ty) = &rust_types[op.input.as_str()] else { panic!() };
    ty.fields
        .iter()
        .any(|field| field.position == "payload" && field.type_ == "StreamingBlob")
}

/// Emit the single `resolve_route` over the union route set. MinIO-only
/// operations contribute inline `#[cfg(feature = "minio")]`-gated branches.
fn codegen_router(ops: &Operations, rust_types: &RustTypes) {
    write_dir_file(OPS_GENERATED_DIR, "router.rs", || {
        codegen_file_header();
        // The router uses fully-qualified `crate::ops::` paths for the
        // helpers; only the module re-exports (operation structs) are globbed.
        g([
            "use super::*;",
            "use crate::error::*;",
            "use crate::http;",
            "use crate::path::S3Path;",
            "",
        ]);
        codegen_router_inner(ops, rust_types);
    });
}

#[allow(clippy::too_many_lines)]
fn codegen_router_inner(ops: &Operations, rust_types: &RustTypes) {
    let routes = collect_routes(ops, rust_types);

    let methods = ["HEAD", "GET", "POST", "PUT", "DELETE"];
    assert_eq!(methods.len(), routes.keys().count());
    for method in routes.keys() {
        assert!(methods.contains(&method.as_str()));
    }

    // Top-level group resolvers (stable order: method x path shape) so
    // `resolve_route` stays a thin dispatcher.
    for &method in &methods {
        for pattern in [PathPattern::Root, PathPattern::Bucket, PathPattern::Object] {
            let Some(group) = routes[method].get(&pattern) else { continue };
            let group_snake = group_snake_name(pattern);
            let group_name = format!("resolve_{}_{}", method.to_lowercase(), group_snake);
            crate::v1::fvr::codegen_fvr_group(&group_name, group);
            g!("");
        }
    }

    g!("pub fn resolve_route(\
        req: &http::Request, \
        s3_path: &S3Path, \
        qs: Option<&http::OrderedQs>)\
         -> S3Result<&'static dyn crate::ops::Operation> {{");

    g!("match req.method {{");
    for &method in &methods {
        g!("hyper::Method::{method} => match s3_path {{");

        for pattern in [PathPattern::Root, PathPattern::Bucket, PathPattern::Object] {
            let s3_path_pattern = match pattern {
                PathPattern::Root => "S3Path::Root",
                PathPattern::Bucket => "S3Path::Bucket{ .. }",
                PathPattern::Object => "S3Path::Object{ .. }",
            };

            g!("{s3_path_pattern} => {{");
            match routes[method].get(&pattern) {
                None => g!("Err(crate::ops::unknown_operation())"),
                Some(group) => {
                    assert!(group.is_empty().not());
                    let group_snake = group_snake_name(pattern);
                    let group_name = format!("resolve_{}_{}", method.to_lowercase(), group_snake);
                    g!("{group_name}(req, qs)");
                }
            }
            g!("}}");
        }
        g!("}}");
    }
    g!("_ => Err(crate::ops::unknown_operation())");
    g!("}}");

    g!("}}");
}

fn group_snake_name(pattern: PathPattern) -> &'static str {
    match pattern {
        PathPattern::Root => "root",
        PathPattern::Bucket => "bucket",
        PathPattern::Object => "object",
    }
}
