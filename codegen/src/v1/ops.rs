use super::dto::RustTypes;
use super::rust::default_value_literal;
use super::xml::{is_xml_output, is_xml_payload};
use super::{dto, rust, smithy};
use super::{headers, o};

use crate::declare_codegen;

use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt::Write as _;
use std::format as f;
use std::ops::Not;

use heck::ToSnakeCase;
use scoped_writer::g;
use stdx::default::default;

#[derive(Debug)]
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
}

pub type Operations = BTreeMap<String, Operation>;

// TODO: handle these operations
pub const SKIPPED_OPS: &[&str] = &["CreateSession", "ListDirectoryBuckets"];

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
                assert_eq!(smithy_output.strip_suffix("Output").unwrap(), op_name);
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
        };
        insert(&mut operations, op_name, op);
    }

    // PostObject is a synthetic operation (multipart form upload) which is not present in the
    // upstream Smithy model. We add it here so downstream generators (trait/access/etc.) can treat
    // it like a normal operation.
    if operations.contains_key("PostObject").not() {
        let op_name = o("PostObject");
        let op = Operation {
            name: op_name.clone(),

            input: o("PostObjectInput"),
            output: o("PostObjectOutput"),

            // Placeholders (there is no Smithy-modeled input/output for PostObject).
            smithy_input: o("Unit"),
            smithy_output: o("Unit"),

            s3_unwrapped_xml_output: false,
            doc: None,

            http_method: o("POST"),
            http_uri: o("/{Bucket}"),
            http_code: 200,
        };
        insert(&mut operations, op_name, op);
    }

    operations
}

pub fn is_op_input(name: &str, ops: &Operations) -> bool {
    name.strip_suffix("Input").is_some_and(|x| ops.contains_key(x))
}

pub fn is_op_output(name: &str, ops: &Operations) -> bool {
    name.strip_suffix("Output").is_some_and(|x| ops.contains_key(x))
}

pub fn codegen(ops: &Operations, rust_types: &RustTypes) {
    declare_codegen!();

    for op in ops.values() {
        g!("// {}", op.name);
    }
    g!();

    g([
        "#![allow(clippy::declare_interior_mutable_const)]",
        "#![allow(clippy::borrow_interior_mutable_const)]",
        "#![allow(clippy::needless_pass_by_value)]",
        "#![allow(clippy::too_many_lines)]",
        "#![allow(clippy::collapsible_if)]",
        "#![allow(clippy::unnecessary_wraps)]",
        "",
        "use crate::dto::*;",
        "use crate::header::*;",
        "use crate::http;",
        "use crate::error::*;",
        "use crate::path::S3Path;",
        "use crate::ops::CallContext;",
        "",
        "use std::borrow::Cow;",
        "",
    ]);

    codegen_http(ops, rust_types);
    codegen_router(ops, rust_types);
}

fn status_code_name(code: u16) -> &'static str {
    match code {
        200 => "OK",
        204 => "NO_CONTENT",
        _ => unimplemented!(),
    }
}

fn codegen_http(ops: &Operations, rust_types: &RustTypes) {
    codegen_header_value(ops, rust_types);

    for op in ops.values() {
        if op.name == "PostObject" {
            continue;
        }
        g!("pub struct {};", op.name);
        g!();

        g!("impl {} {{", op.name);

        codegen_op_http_de(op, rust_types);
        codegen_op_http_ser(op, rust_types);

        g!("}}");
        g!();

        codegen_op_http_call(op);
        g!();
    }

    codegen_post_object_fork_op(rust_types);
}

fn codegen_post_object_fork_op(rust_types: &RustTypes) {
    // PostObject is a synthetic operation: same behavior as PutObject (multipart form upload),
    // but separated at the trait layer so implementations can distinguish PUT vs POST later.
    // Keep behavior identical for now by delegating serialization to PutObject.
    let Some(rust::Type::Struct(put_in)) = rust_types.get("PutObjectInput") else { return };
    let Some(rust::Type::Struct(_put_out)) = rust_types.get("PutObjectOutput") else { return };
    let Some(rust::Type::Struct(post_in)) = rust_types.get("PostObjectInput") else { return };
    let Some(rust::Type::Struct(_post_out)) = rust_types.get("PostObjectOutput") else { return };

    // Sanity: forked types should stay field-identical.
    assert_eq!(put_in.fields.len(), post_in.fields.len());
    for (a, b) in put_in.fields.iter().zip(post_in.fields.iter()) {
        assert_eq!(a.name, b.name);
        assert_eq!(a.type_, b.type_);
        assert_eq!(a.option_type, b.option_type);
    }

    g(["pub struct PostObject;", "", "impl PostObject {"]);
    g([
        "    pub fn deserialize_http(req: &mut http::Request) -> S3Result<PostObjectInput> {",
        "        let Some(m) = req.s3ext.multipart.take() else {",
        "            return Err(invalid_request!(\"missing multipart form\"));",
        "        };",
        "        let put_input = PutObject::deserialize_http_multipart(req, m)?;",
        "        Ok(put_object_input_into_post_object_input(put_input))",
        "    }",
        "",
        "    pub fn serialize_http(x: PostObjectOutput) -> S3Result<http::Response> {",
        "        PutObject::serialize_http(post_object_output_into_put_object_output(x))",
        "    }",
        "}",
        "",
    ]);

    g(["#[async_trait::async_trait]", "impl super::Operation for PostObject {"]);
    g(["    fn name(&self) -> &'static str {", "        \"PostObject\"", "    }", ""]);

    g([
        "    async fn call(&self, ccx: &CallContext<'_>, req: &mut http::Request) -> S3Result<http::Response> {",
        "        let post_input = Self::deserialize_http(req)?;",
        "        let put_input = post_object_input_into_put_object_input(post_input);",
        "        let mut put_req = super::build_s3_request(put_input, req);",
        "        let s3 = ccx.s3;",
        "        if let Some(access) = ccx.access {",
        "            // Keep backward-compatible behavior: POST object used to be gated by put_object access check.",
        "            access.put_object(&mut put_req).await?;",
        "        }",
        "        let mut post_req = put_req.map_input(put_object_input_into_post_object_input);",
        "        if let Some(access) = ccx.access {",
        "            // New hook for POST object (optional).",
        "            access.post_object(&mut post_req).await?;",
        "        }",
        "        let result = s3.post_object(post_req).await;",
        "        let s3_resp = match result {",
        "            Ok(val) => val,",
        "            Err(err) => return super::serialize_error(err, false),",
        "        };",
        "        // Keep identical HTTP response behavior to PutObject.",
        "        let mut resp = Self::serialize_http(s3_resp.output)?;",
        "        resp.headers.extend(s3_resp.headers);",
        "        resp.extensions.extend(s3_resp.extensions);",
        "        Ok(resp)",
        "    }",
        "}",
    ]);

    g!();
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

    for rust_type in str_enum_names.iter().map(|&x| &rust_types[x]) {
        let rust::Type::StrEnum(ty) = rust_type else { panic!() };

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
    }

    for rust_type in str_enum_names.iter().map(|&x| &rust_types[x]) {
        let rust::Type::StrEnum(ty) = rust_type else { panic!() };

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
                                g!(
                                    "let {}: Option<{}> = http::parse_opt_header_timestamp(req, &{}, TimestampFormat::{})?;",
                                    field.name,
                                    field.type_,
                                    header,
                                    fmt
                                );
                            } else if field.option_type {
                                if field.name == "checksum_algorithm" {
                                    g!(
                                        "let {}: Option<{}> = http::parse_checksum_algorithm_header(req)?;",
                                        field.name,
                                        field.type_,
                                    );
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

                if op.name == "PutObject" {
                    codegen_op_http_de_multipart(op, rust_types);
                }
            }
        }
        _ => unimplemented!(),
    }
    g!();
}

fn codegen_field_de_query(field: &rust::StructField, rust_types: &RustTypes) {
    let query = field.http_query.as_deref().unwrap();

    let field_type = &rust_types[&field.type_];

    if let rust::Type::List(_) = field_type {
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
        "let vec_stream = req.s3ext.vec_stream.take().expect(\"missing vec stream\");",
        "",
        "let content_length = i64::try_from(vec_stream.exact_remaining_length())",
        "    .map_err(|e| s3_error!(e, InvalidArgument, \"content-length overflow\"))?;",
        "let content_length = (content_length != 0).then_some(content_length);",
        "",
        "let body: Option<StreamingBlob> = Some(StreamingBlob::new(vec_stream));",
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

fn codegen_op_http_call(op: &Operation) {
    g!("#[async_trait::async_trait]");
    g!("impl super::Operation for {} {{", op.name);

    g!("fn name(&self) -> &'static str {{");
    g!("\"{}\"", op.name);
    g!("}}");
    g!();

    g!("async fn call(&self, ccx: &CallContext<'_>, req: &mut http::Request) -> S3Result<http::Response> {{");

    let method = op.name.to_snake_case();

    g!("let input = Self::deserialize_http(req)?;");
    g!("let mut s3_req = super::build_s3_request(input, req);");
    g!("let s3 = ccx.s3;");

    g!("if let Some(access) = ccx.access {{");
    g!("    access.{method}(&mut s3_req).await?;");
    g!("}}");

    if op.name == "GetObject" {
        g!("let overridden_headers = super::get_object::extract_overridden_response_headers(&s3_req)?;");
    }

    g!("let result = s3.{method}(s3_req).await;");

    g([
        "let s3_resp = match result {",
        "    Ok(val) => val,",
        "    Err(err) => return super::serialize_error(err, false),",
        "};",
    ]);

    g!("let mut resp = Self::serialize_http(s3_resp.output)?;");

    if op.name == "GetObject" {
        g!("resp.headers.extend(overridden_headers);");
        g!("super::get_object::merge_custom_headers(&mut resp, s3_resp.headers);");
    } else {
        g!("resp.headers.extend(s3_resp.headers);");
    }

    g!("resp.extensions.extend(s3_resp.extensions);");

    g!("Ok(resp)");

    g!("}}");
    g!("}}");
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum PathPattern {
    Root,
    Bucket,
    Object,
}

impl PathPattern {
    fn parse(part: &str) -> Self {
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
}

struct Route<'a> {
    op: &'a Operation,
    query_tag: Option<String>,
    query_patterns: Vec<(String, String)>,
    required_headers: Vec<&'a str>,
    required_query_strings: Vec<&'a str>,
    needs_full_body: bool,
}

fn collect_routes<'a>(ops: &'a Operations, rust_types: &'a RustTypes) -> HashMap<String, HashMap<PathPattern, Vec<Route<'a>>>> {
    let mut ans: HashMap<String, HashMap<PathPattern, Vec<Route<'_>>>> = default();
    for op in ops.values() {
        // PostObject is resolved in ops::prepare() for multipart requests.
        // Do not put it into the generated router to avoid overlaps.
        if op.name == "PostObject" {
            continue;
        }
        let pat = PathPattern::parse(&op.http_uri);
        let map = ans.entry(op.http_method.clone()).or_default();
        let vec = map.entry(pat).or_default();

        vec.push(Route {
            op,
            query_tag: PathPattern::query_tag(&op.http_uri),
            query_patterns: PathPattern::query_patterns(&op.http_uri),

            required_headers: required_headers(op, rust_types),
            required_query_strings: required_query_strings(op, rust_types),

            needs_full_body: needs_full_body(op, rust_types),
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

#[allow(clippy::too_many_lines)]
fn codegen_router(ops: &Operations, rust_types: &RustTypes) {
    let routes = collect_routes(ops, rust_types);

    let methods = ["HEAD", "GET", "POST", "PUT", "DELETE"];
    assert_eq!(methods.len(), routes.keys().count());
    for method in routes.keys() {
        assert!(methods.contains(&method.as_str()));
    }

    g!("pub fn resolve_route(\
        req: &http::Request, \
        s3_path: &S3Path, \
        qs: Option<&http::OrderedQs>)\
         -> S3Result<(&'static dyn super::Operation, bool)> {{");

    let succ = |route: &Route, return_: bool| {
        if return_ {
            g!(
                "return Ok((&{} as &'static dyn super::Operation, {}));",
                route.op.name,
                route.needs_full_body
            );
        } else {
            g!("Ok((&{} as &'static dyn super::Operation, {}))", route.op.name, route.needs_full_body);
        }
    };

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
                None => g!("Err(super::unknown_operation())"),
                Some(group) => {
                    // NOTE: To debug the routing order, uncomment the lines below.
                    // {
                    //     println!("{method} {pattern:?}");
                    //     println!();
                    //     for route in group {
                    //         println!(
                    //             "{:<80} qt={:<30} qp={:<30}, qs={:?}, hs={:?}",
                    //             route.op.name,
                    //             f!("{:?}", route.query_tag.as_deref()),
                    //             f!("{:?}", route.query_patterns),
                    //             route.required_query_strings,
                    //             route.required_headers
                    //         );
                    //         println!("{}", route.op.http_uri);
                    //         println!();
                    //     }
                    //     println!("\n\n\n");
                    // }

                    assert!(group.is_empty().not());
                    if group.len() == 1 {
                        let route = &group[0];
                        assert!(route.query_tag.is_none());
                        assert!(route.required_headers.is_empty());
                        assert!(route.required_query_strings.is_empty());
                        assert!(route.needs_full_body.not());
                        succ(route, false);
                    } else {
                        let is_final_op = |route: &Route| {
                            route.required_headers.is_empty()
                                && route.required_query_strings.is_empty()
                                && route.query_patterns.is_empty()
                                && route.query_tag.is_none()
                        };
                        let final_count = group.iter().filter(|r| is_final_op(r)).count();
                        assert!(final_count <= 1);

                        g!("if let Some(qs) = qs {{");
                        for route in group {
                            let has_query_tag = route.query_tag.is_some();
                            let has_query_patterns = route.query_patterns.is_empty().not();

                            let qp = route.query_patterns.as_slice();

                            if has_query_tag {
                                let tag = route.query_tag.as_deref().unwrap();
                                assert!(tag.as_bytes().iter().all(|&x| x == b'-' || x.is_ascii_alphabetic()), "{tag}");
                            }
                            if has_query_patterns {
                                assert!(qp.len() <= 1);
                            }

                            match (has_query_tag, has_query_patterns) {
                                (true, true) => {
                                    assert_eq!(route.op.name, "SelectObjectContent");

                                    let tag = route.query_tag.as_deref().unwrap();
                                    let (n, v) = qp.first().unwrap();

                                    g!("if qs.has(\"{tag}\") && super::check_query_pattern(qs, \"{n}\",\"{v}\") {{");
                                    succ(route, true);
                                    g!("}}");
                                }
                                (true, false) => {
                                    let tag = route.query_tag.as_deref().unwrap();

                                    // Special handling for operations that share the same query tag
                                    // but are differentiated by the presence of an 'id' parameter
                                    let needs_id_check = matches!(
                                        route.op.name.as_str(),
                                        "GetBucketAnalyticsConfiguration"
                                            | "GetBucketIntelligentTieringConfiguration"
                                            | "GetBucketInventoryConfiguration"
                                            | "GetBucketMetricsConfiguration"
                                    );
                                    let needs_no_id_check = matches!(
                                        route.op.name.as_str(),
                                        "ListBucketAnalyticsConfigurations"
                                            | "ListBucketIntelligentTieringConfigurations"
                                            | "ListBucketInventoryConfigurations"
                                            | "ListBucketMetricsConfigurations"
                                    );

                                    if needs_id_check {
                                        g!("if qs.has(\"{tag}\") && qs.has(\"id\") {{");
                                        succ(route, true);
                                        g!("}}");
                                    } else if needs_no_id_check {
                                        g!("if qs.has(\"{tag}\") && !qs.has(\"id\") {{");
                                        succ(route, true);
                                        g!("}}");
                                    } else {
                                        g!("if qs.has(\"{tag}\") {{");
                                        succ(route, true);
                                        g!("}}");
                                    }
                                }
                                (false, true) => {
                                    let (n, v) = qp.first().unwrap();
                                    g!("if super::check_query_pattern(qs, \"{n}\",\"{v}\") {{");
                                    succ(route, true);
                                    g!("}}");
                                }
                                (false, false) => {}
                            }
                        }
                        g!("}}");

                        for route in group {
                            let has_query_tag = route.query_tag.is_some();
                            let has_query_patterns = route.query_patterns.is_empty().not();

                            if has_query_tag || has_query_patterns {
                                continue;
                            }

                            let qs = route.required_query_strings.as_slice();
                            let hs = route.required_headers.as_slice();
                            assert!(qs.len() <= 2, "qs = {qs:?}");
                            assert!(hs.len() <= 2, "hs = {hs:?}");

                            if qs.is_empty() && hs.is_empty() {
                                continue;
                            }

                            let mut cond: String = default();
                            for q in qs {
                                if cond.is_empty().not() {
                                    cond.push_str(" && ");
                                }
                                //cond.push_str(&f!("qs.has(\"{q}\")"));
                                write!(cond, "qs.has(\"{q}\")").unwrap();
                            }
                            for h in hs {
                                if cond.is_empty().not() {
                                    cond.push_str(" && ");
                                }
                                write!(cond, "req.headers.contains_key(\"{h}\")").unwrap();
                            }

                            if qs.is_empty().not() {
                                g!("if let Some(qs) = qs");
                                g!("    && {cond} {{");
                                succ(route, true);
                                g!("}}");
                            } else {
                                g!("if {cond} {{");
                                succ(route, true);
                                g!("}}");
                            }
                        }

                        if final_count == 1 {
                            let route = group.last().unwrap();
                            assert!(is_final_op(route));
                            succ(route, false);
                        } else {
                            g!("Err(super::unknown_operation())");
                        }
                    }
                }
            }
            g!("}}");
        }

        g!("}}");
    }
    g!("_ => Err(super::unknown_operation())");
    g!("}}");

    g!("}}");
}
