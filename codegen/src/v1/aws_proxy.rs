// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

use super::dto::RustTypes;
use super::ops::Operations;
use super::rust;

use crate::declare_codegen;

use std::format as f;

use heck::ToSnakeCase;
use scoped_writer::g;

pub fn codegen(ops: &Operations, rust_types: &RustTypes) {
    declare_codegen!();

    g([
        "use super::*;",
        "",
        "use crate::conv::{try_from_aws, try_into_aws};",
        "use crate::conv::{expires_into_aws, string_from_integer};",
        "",
        "use s3s::S3;",
        "use s3s::{S3Request, S3Response};",
        "use s3s::S3Result;",
        "",
        "use tracing::debug;",
        "",
    ]);

    g!("#[async_trait::async_trait]");
    g!("impl S3 for Proxy {{");

    for op in ops.values() {
        // PostObject is a synthetic API in s3s; aws-sdk-s3 has no corresponding operation.
        if op.name == "PostObject" {
            continue;
        }
        let method_name = op.name.to_snake_case();
        let s3s_input = f!("s3s::dto::{}", op.input);
        let s3s_output = f!("s3s::dto::{}", op.output);

        if op.is_minio {
            // MinIO-only extensions have no aws-sdk-s3 counterpart; the proxy
            // forwards them through the official MinIO SDK, delegating to a
            // same-named helper in `listen.rs`.
            g!("#[tracing::instrument(skip(self, req))]");
            g!("#[cfg(feature = \"minio\")]");
            g!("async fn {method_name}(&self, req: S3Request<{s3s_input}>) -> S3Result<S3Response<{s3s_output}>> {{");
            g!("super::listen::{method_name}(&self.minio, req).await");
            g!("}}");
            g!();
            continue;
        }

        g!("#[tracing::instrument(skip(self, req))]");
        g!("async fn {method_name}(&self, req: S3Request<{s3s_input}>) -> S3Result<S3Response<{s3s_output}>> {{");

        g!("let input = req.input;");
        g!("debug!(?input);");

        if op.smithy_input == "Unit" {
            g!("let result = self.client.{method_name}().send().await;");
        } else {
            g!("let mut b = self.client.{method_name}();");
            let rust::Type::Struct(ty) = &rust_types[op.input.as_str()] else { panic!() };

            let flattened_fields = if ty.name == "SelectObjectContentInput" {
                let rust::Type::Struct(flattened_ty) = &rust_types["SelectObjectContentRequest"] else { panic!() };
                flattened_ty.fields.as_slice()
            } else {
                &[]
            };

            let extension_headers = extension_header_fields(ty, flattened_fields);

            for field in ty.fields.iter().chain(flattened_fields) {
                if field.is_custom_extension {
                    continue;
                }

                let s3s_field_name = match ty.name.as_str() {
                    "SelectObjectContentInput" if field.name == "request" => continue,
                    "SelectObjectContentInput" if field.position == "xml" => f!("request.{}", field.name),
                    _ => field.name.clone(),
                };
                let aws_field_name = match ty.name.as_str() {
                    "SelectObjectContentInput" => field.name.as_str(),
                    _ => match s3s_field_name.as_str() {
                        "checksum_crc32c" => "checksum_crc32_c",
                        "checksum_crc64nvme" => "checksum_crc64_nvme",
                        "type_" => "type",
                        s => s,
                    },
                };

                // // hack
                // if op.name == "PutObject" && field.type_ == "ChecksumAlgorithm" {
                //     assert!(field.option_type);
                //     let default_val = "aws_sdk_s3::model::ChecksumAlgorithm::Sha256";
                //     let val = f!("try_into_aws(input.{s3s_field_name})?.or(Some({default_val}))");
                //     g!("b = b.set_{aws_field_name}({val});");
                //     continue;
                // }

                if field.type_ == "PartNumberMarker" || field.type_ == "NextPartNumberMarker" {
                    g!("b = b.set_{aws_field_name}(input.{s3s_field_name}.map(string_from_integer));");
                    continue;
                }

                if field.type_ == "Expires" {
                    // s3s keeps the raw string; the SDK input takes `DateTime`.
                    g!("b = b.set_{aws_field_name}(expires_into_aws(input.{s3s_field_name})?);");
                    continue;
                }

                if field.option_type {
                    g!("b = b.set_{aws_field_name}(try_into_aws(input.{s3s_field_name})?);");
                } else {
                    g!("b = b.set_{aws_field_name}(Some(try_into_aws(input.{s3s_field_name})?));");
                }
            }
            if !extension_headers.is_empty() {
                codegen_extension_headers(&extension_headers);
            }
            g!("let result = b.send().await;");
        }

        g([
            "match result {",
            "    Ok(output) => {",
            "        let headers = super::meta::build_headers(&output)?;",
            "        let output = try_from_aws(output)?;",
            "        debug!(?output);",
            "        Ok(S3Response::with_headers(output, headers))",
            "    },",
            "    Err(e) => Err(wrap_sdk_error!(e)),",
            "}",
        ]);

        g!("}}");
        g!();
    }

    g!("}}");
}

/// The `MinIO` extension members of an operation that are declared as HTTP
/// headers; they have no `aws-sdk-s3` counterpart and are forwarded verbatim.
fn extension_header_fields<'a>(ty: &'a rust::Struct, flattened: &'a [rust::StructField]) -> Vec<&'a rust::StructField> {
    ty.fields
        .iter()
        .chain(flattened)
        .filter(|field| field.is_custom_extension && field.http_header.is_some())
        .collect()
}

/// Emits the request customization that forwards `MinIO` extension members
/// declared as HTTP headers (for example `x-minio-force-delete`).
///
/// The proxy file is generated from the `MinIO` model only and compiled in both
/// feature configurations, so the block carries its own feature gate.
fn codegen_extension_headers(fields: &[&rust::StructField]) {
    g!("#[cfg(feature = \"minio\")]");
    g!("let b = {{");
    for field in fields {
        let name = &field.name;
        g!("    let {name} = input.{name};");
    }
    g!("    b.customize().mutate_request(move |req| {{");
    for field in fields {
        let header = field.http_header.as_deref().unwrap();
        let name = &field.name;
        if field.option_type {
            g!("        if let Some(value) = {name} {{");
            g!("            req.headers_mut().insert(\"{header}\", value.to_string());");
            g!("        }}");
        } else {
            g!("        req.headers_mut().insert(\"{header}\", {name}.to_string());");
        }
    }
    g!("    }})");
    g!("}};");
}
