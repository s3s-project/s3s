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
        "use crate::conv::string_from_integer;",
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
        // Skip PostObject - AWS SDK doesn't have this operation
        if op.name == "PostObject" {
            continue;
        }

        let method_name = op.name.to_snake_case();
        let s3s_input = f!("s3s::dto::{}", op.input);
        let s3s_output = f!("s3s::dto::{}", op.output);

        g!("#[tracing::instrument(skip(self, req))]");
        g!("async fn {method_name}(&self, req: S3Request<{s3s_input}>) -> S3Result<S3Response<{s3s_output}>> {{");

        g!("let input = req.input;");
        g!("debug!(?input);");

        if op.smithy_input == "Unit" {
            g!("let result = self.0.{method_name}().send().await;");
        } else {
            g!("let mut b = self.0.{method_name}();");
            let rust::Type::Struct(ty) = &rust_types[op.input.as_str()] else { panic!() };

            let flattened_fields = if ty.name == "SelectObjectContentInput" {
                let rust::Type::Struct(flattened_ty) = &rust_types["SelectObjectContentRequest"] else { panic!() };
                flattened_ty.fields.as_slice()
            } else {
                &[]
            };

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

                if field.option_type {
                    g!("b = b.set_{aws_field_name}(try_into_aws(input.{s3s_field_name})?);");
                } else {
                    g!("b = b.set_{aws_field_name}(Some(try_into_aws(input.{s3s_field_name})?));");
                }
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
