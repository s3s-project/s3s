use super::o;
use super::ops::{Operations, SKIPPED_OPS, is_op_input};
use super::order;
use super::rust::codegen_doc;
use super::smithy::SmithyTraitsExt;
use super::{rust, smithy};

use crate::declare_codegen;
use crate::v1::Patch;

use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet};
use std::ops::Not;

use heck::{ToShoutySnakeCase, ToSnakeCase};
use scoped_writer::g;
use serde_json::Value;
use stdx::default::default;

pub fn to_type_name(shape_name: &str) -> &str {
    let Some((_, name)) = shape_name.split_once('#') else { panic!() };
    name
}

pub type RustTypes = BTreeMap<String, rust::Type>;

#[deny(clippy::shadow_unrelated)]
#[allow(clippy::too_many_lines)]
pub fn collect_rust_types(model: &smithy::Model, ops: &Operations) -> RustTypes {
    let mut space: BTreeMap<String, rust::Type> = default();
    let mut insert = |k: String, v: rust::Type| assert!(space.insert(k, v).is_none());

    for (shape_name, shape) in &model.shapes {
        let rs_shape_name = match to_type_name(shape_name) {
            "SelectObjectContentEventStream" => o("SelectObjectContentEvent"), // rename
            s => s.to_owned(),
        };

        let provided_types = [
            "Body",          //
            "StreamingBlob", //
            "CopySource",    //
            "Range",         //
            "ContentType",   //
            "Event",         //
            "CachedTags",    //
            "ETag",          //
        ];

        // ETag-related header types that should be aliased to ETagCondition instead of String
        // These headers support both ETags and the wildcard "*" value
        let etag_condition_alias_types = [
            "IfMatch",               //
            "IfNoneMatch",           //
            "CopySourceIfMatch",     //
            "CopySourceIfNoneMatch", //
        ];

        if provided_types.contains(&rs_shape_name.as_str()) {
            let ty = rust::Type::provided(&rs_shape_name);
            insert(rs_shape_name, ty);
            continue;
        }

        if etag_condition_alias_types.contains(&rs_shape_name.as_str())
            && let smithy::Shape::String(shape) = shape
        {
            let ty = rust::Type::alias(&rs_shape_name, "ETagCondition", shape.traits.doc());
            insert(rs_shape_name, ty);
            continue;
        }

        match shape {
            smithy::Shape::Boolean(shape) => {
                let ty = rust::Type::alias(&rs_shape_name, "bool", shape.traits.doc());
                insert(rs_shape_name, ty);
            }
            smithy::Shape::Integer(shape) => {
                let ty = rust::Type::alias(&rs_shape_name, "i32", shape.traits.doc());
                insert(rs_shape_name, ty);
            }
            smithy::Shape::Long(shape) => {
                let ty = rust::Type::alias(&rs_shape_name, "i64", shape.traits.doc());
                insert(rs_shape_name, ty);
            }
            smithy::Shape::String(shape) => {
                let ty = rust::Type::alias(&rs_shape_name, "String", shape.traits.doc());
                insert(rs_shape_name, ty);
            }
            smithy::Shape::Timestamp(shape) => {
                let format = shape.traits.timestamp_format().map(|s| match s {
                    "date-time" => "DateTime",
                    "http-date" => "HttpDate",
                    "epoch-seconds" => "EpochSeconds",
                    _ => unimplemented!(),
                });
                let ty = rust::Type::Timestamp(rust::Timestamp {
                    name: rs_shape_name.clone(),
                    format: format.map(o),
                    doc: shape.traits.doc().map(o),
                });
                insert(rs_shape_name, ty);
            }
            smithy::Shape::Blob(_) => {
                unimplemented!();
            }
            smithy::Shape::List(shape) => {
                let ty = rust::Type::List(rust::List {
                    name: rs_shape_name.clone(),
                    member: rust::ListMember {
                        type_: to_type_name(&shape.member.target).to_owned(),
                        xml_name: shape.member.traits.xml_name().map(o),
                    },
                    doc: shape.traits.doc().map(o),
                });
                insert(rs_shape_name, ty);
            }
            smithy::Shape::Map(shape) => {
                let ty = rust::Type::Map(rust::Map {
                    name: rs_shape_name.clone(),
                    key_type: to_type_name(&shape.key.target).to_owned(),
                    value_type: to_type_name(&shape.value.target).to_owned(),
                    doc: shape.traits.doc().map(o),
                });
                insert(rs_shape_name, ty);
            }
            smithy::Shape::Enum(shape) => {
                let mut variants = Vec::new();
                for (variant_name, variant) in &shape.members {
                    let rs_variant_name = match variant_name.as_str() {
                        "CRC32C" => o("CRC32C"),
                        _ => variant_name.to_shouty_snake_case(),
                    };

                    let value = variant.traits.enum_value().unwrap().to_owned();
                    assert!(value.is_ascii());

                    let variant = rust::StrEnumVariant {
                        name: rs_variant_name,
                        value,
                        doc: variant.traits.doc().map(o),
                    };
                    variants.push(variant);
                }
                let ty = rust::Type::StrEnum(rust::StrEnum {
                    name: rs_shape_name.clone(),
                    variants,
                    doc: shape.traits.doc().map(o),
                    is_custom_extension: shape.traits.minio(),
                });
                insert(rs_shape_name, ty);
            }
            smithy::Shape::Structure(shape) => {
                let mut fields = Vec::new();
                let member_list: Vec<(&str, &smithy::StructureMember)> =
                    if let Some(order) = order::struct_member_order(&rs_shape_name) {
                        let order_set: BTreeSet<&str> = order.iter().copied().collect();
                        let mut list = Vec::new();
                        for &name in order {
                            if let Some(field) = shape.members.get(name) {
                                list.push((name, field));
                            }
                        }
                        for (name, field) in &shape.members {
                            if !order_set.contains(name.as_str()) {
                                list.push((name.as_str(), field));
                            }
                        }
                        list
                    } else {
                        shape.members.iter().map(|(k, v)| (k.as_str(), v)).collect()
                    };
                for (field_name, field) in member_list {
                    let rs_field_name = if field_name == "Type" {
                        "type_".into()
                    } else {
                        field_name.to_snake_case()
                    };

                    let field_type = to_type_name(&field.target).to_owned();

                    let default_value = field.traits.default_value().map(o);
                    let is_required = field.traits.required();

                    let is_op_input = rs_shape_name.strip_suffix("Request").is_some_and(|op| ops.contains_key(op));

                    let option_type = 'optional: {
                        if field_type == "StreamingBlob" && default_value.as_ref().unwrap() == "" {
                            break 'optional true;
                        }
                        if is_op_input && is_required.not() {
                            break 'optional true;
                        }
                        is_required.not() && default_value.is_none()
                    };

                    let position = {
                        let mut position = "xml";
                        if field.traits.http_header().is_some() {
                            position = "header";
                        }
                        if field.traits.http_query().is_some() {
                            position = "query";
                        }
                        if field.traits.http_payload() {
                            position = "payload";
                        }

                        if field.traits.http_label().is_some() {
                            match field_type.as_str() {
                                "BucketName" => position = "bucket",
                                "ObjectKey" => position = "key",
                                _ => unimplemented!(),
                            }
                        }

                        if field_type == "Metadata" {
                            assert_eq!(field.traits.http_prefix_headers(), Some("x-amz-meta-"));
                            position = "metadata";
                        }

                        if field.traits.sealed() {
                            position = "sealed";
                        }

                        o(position)
                    };

                    let field = rust::StructField {
                        name: rs_field_name,
                        type_: field_type,
                        doc: field.traits.doc().map(o),

                        camel_name: field_name.to_owned(),

                        option_type,
                        default_value,
                        is_required,

                        position,

                        http_header: field.traits.http_header().map(o),
                        http_query: field.traits.http_query().map(o),
                        xml_name: field.traits.xml_name().map(o),
                        xml_flattened: field.traits.xml_flattened(),

                        is_xml_attr: field.traits.xml_attr(),
                        xml_namespace_uri: field.traits.xml_namespace_uri().map(o),
                        xml_namespace_prefix: field.traits.xml_namespace_prefix().map(o),

                        is_custom_extension: field.traits.minio(),

                        custom_in_derive_debug: None,
                    };
                    fields.push(field);
                }
                let ty = rust::Type::Struct(rust::Struct {
                    name: rs_shape_name.clone(),
                    fields,
                    doc: shape.traits.doc().map(ToOwned::to_owned),

                    xml_name: shape.traits.xml_name().map(o),
                    is_error_type: shape.traits.error().is_some(),
                    is_custom_extension: shape.traits.minio(),
                });
                insert(rs_shape_name, ty);
            }
            smithy::Shape::Union(shape) => {
                let mut variants = Vec::new();
                for (variant_name, variant) in &shape.members {
                    let variant = rust::StructEnumVariant {
                        name: variant_name.clone(),
                        type_: to_type_name(&variant.target).to_owned(),
                        doc: variant.traits.doc().map(o),
                    };
                    variants.push(variant);
                }
                let ty = rust::Type::StructEnum(rust::StructEnum {
                    name: rs_shape_name.clone(),
                    variants,
                    doc: shape.traits.doc().map(o),
                });
                insert(rs_shape_name, ty);
            }
            smithy::Shape::Operation(_) => {}
            smithy::Shape::Service(_) => {}
        }
    }

    patch_types(&mut space);
    unify_operation_types(ops, &mut space);

    // POST Object is not a Smithy-modeled operation in the upstream S3 model.
    // We still want to distinguish it from PutObject at the trait layer.
    // Fork the unified DTO types so behavior can stay identical,
    // while leaving room to extend PostObject* with POST-only fields later.
    for (src, dst) in [("PutObjectInput", "PostObjectInput"), ("PutObjectOutput", "PostObjectOutput")] {
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

    // Add POST Object specific fields to PostObjectInput
    if let Some(rust::Type::Struct(post_in)) = space.get_mut("PostObjectInput") {
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

    space
}

fn patch_types(space: &mut RustTypes) {
    // patch CompleteMultipartUploadOutput
    {
        let Some(rust::Type::Struct(ty)) = space.get_mut("CompleteMultipartUploadOutput") else { panic!() };
        ty.fields.push(rust::StructField {
            name: o("future"),
            type_: o("BoxFuture<'static, S3Result<CompleteMultipartUploadOutput>>"),
            option_type: true,
            position: o("s3s"),
            doc: Some(o("A future that resolves to the upload output or an error. This field is used to implement AWS-like keep-alive behavior.")),
            custom_in_derive_debug: Some(o("&\"<BoxFuture<'static, S3Result<CompleteMultipartUploadOutput>>>\"")),    
            ..rust::StructField::default()
        });
    }

    // patch PartNumberMarker
    // FIXME: https://github.com/awslabs/aws-sdk-rust/issues/1318
    {
        let Some(rust::Type::Alias(ty)) = space.get_mut("PartNumberMarker") else { panic!() };
        assert_eq!(ty.type_, "String");
        "i32".clone_into(&mut ty.type_);

        let Some(rust::Type::Alias(ty)) = space.get_mut("NextPartNumberMarker") else { panic!() };
        assert_eq!(ty.type_, "String");
        "i32".clone_into(&mut ty.type_);
    }

    // patch Tag
    {
        let Some(rust::Type::Struct(ty)) = space.get_mut("Tag") else { panic!() };
        for field in &mut ty.fields {
            if field.name == "key" {
                field.is_required = false;
                field.option_type = true;
            }
            if field.name == "value" {
                field.is_required = false;
                field.option_type = true;
            }
        }
    }

    // patch LifecycleExpiration
    {
        let Some(rust::Type::Struct(ty)) = space.get_mut("LifecycleExpiration") else { panic!() };
        for field_name in ["days", "expired_object_delete_marker"] {
            let field = ty.fields.iter_mut().find(|x| x.name == field_name).unwrap();
            field.default_value = None;
            field.option_type = true;
        }
    }

    // patch SelectObjectContent input
    {
        let Some(rust::Type::Struct(mut ty)) = space.remove("SelectObjectContentRequest") else { panic!() };
        let request = rust::Struct {
            name: ty.name.clone(),
            fields: ty.fields.iter().filter(|x| x.position == "xml").cloned().collect(),
            doc: ty.doc.clone(),
            xml_name: None,
            is_error_type: false,
            is_custom_extension: false,
        };

        ty.fields.iter().for_each(|x| assert!(x.name != "request"));

        ty.fields.retain(|x| x.position != "xml");
        ty.fields.push(rust::StructField {
            name: o("request"),
            type_: request.name.clone(),
            doc: None,
            camel_name: request.name.clone(),
            option_type: false,
            default_value: None,
            is_required: false,
            position: o("payload"),
            http_header: None,
            http_query: None,
            xml_name: Some(request.name.clone()),
            xml_flattened: false,
            is_xml_attr: false,
            xml_namespace_uri: None,
            xml_namespace_prefix: None,
            is_custom_extension: false,
            custom_in_derive_debug: None,
        });
        ty.name = o("SelectObjectContentInput");

        space.insert("SelectObjectContentInput".into(), rust::Type::Struct(ty));
        space.insert("SelectObjectContentRequest".into(), rust::Type::Struct(request));
    }
}

fn unify_operation_types(ops: &Operations, space: &mut RustTypes) {
    for op in SKIPPED_OPS {
        space.remove(&format!("{op}Request"));
        space.remove(&format!("{op}Output"));
    }

    // unify operation input type
    for op in ops.values() {
        if op.name == "PostObject" {
            continue;
        }
        if op.name == "SelectObjectContent" {
            continue;
        }
        let input_ty = if op.smithy_input == "Unit" {
            rust::Struct {
                name: op.input.clone(),
                fields: default(),
                doc: None,
                xml_name: None,
                is_error_type: false,
                is_custom_extension: false,
            }
        } else {
            assert!(op.smithy_input.ends_with("Request"));
            let Some(rust::Type::Struct(mut ty)) = space.remove(&op.smithy_input) else { panic!() };
            ty.name.clone_from(&op.input); // rename type
            ty
        };
        assert!(space.insert(op.input.clone(), rust::Type::Struct(input_ty)).is_none());
    }

    // unify operation output type
    for op in ops.values() {
        if op.name == "PostObject" {
            continue;
        }
        let output_ty = if op.smithy_output == "Unit" {
            rust::Struct {
                name: op.output.clone(),
                fields: default(),
                doc: None,
                xml_name: None,
                is_error_type: false,
                is_custom_extension: false,
            }
        } else {
            if op.smithy_output == op.output {
                continue;
            }
            assert_eq!(op.name, "GetBucketNotificationConfiguration");
            assert_eq!(op.output, "GetBucketNotificationConfigurationOutput");
            let rust::Type::Struct(ref origin) = space[&op.smithy_output] else { panic!() };
            let mut ty = origin.clone();
            ty.name.clone_from(&op.output); // duplicate type
            assert!(origin.xml_name.is_none());
            ty.xml_name = Some(origin.name.clone());
            ty
        };
        assert!(space.insert(op.output.clone(), rust::Type::Struct(output_ty)).is_none());
    }
}

fn collect_types_needing_serde(rust_types: &RustTypes) -> BTreeSet<String> {
    let mut types_needing_serde = BTreeSet::new();

    // Start with Configuration types and special types
    for name in rust_types.keys() {
        if name.ends_with("Configuration") || name == "Tag" || name == "Tagging" {
            collect_type_dependencies(name, rust_types, &mut types_needing_serde);
        }
    }

    types_needing_serde
}

fn collect_types_needing_custom_default(rust_types: &RustTypes) -> BTreeSet<String> {
    let mut types_needing_custom_default = BTreeSet::new();

    // Start with Configuration types that can't derive Default
    for (name, rust_type) in rust_types {
        if name.ends_with("Configuration")
            && let rust::Type::Struct(ty) = rust_type
            && !can_derive_default(ty, rust_types)
        {
            // Add this type and all its struct dependencies
            collect_struct_dependencies(name, rust_types, &mut types_needing_custom_default);
        }
    }

    types_needing_custom_default
}

fn collect_struct_dependencies(type_name: &str, rust_types: &RustTypes, result: &mut BTreeSet<String>) {
    // Avoid infinite recursion
    if result.contains(type_name) {
        return;
    }

    // Only add this type if it can't derive Default
    if let Some(rust::Type::Struct(s)) = rust_types.get(type_name)
        && !can_derive_default(s, rust_types)
    {
        result.insert(type_name.to_owned());

        // Recursively add struct dependencies that also can't derive Default
        for field in &s.fields {
            // Skip optional fields and list/map types (they already have Default)
            if field.option_type {
                continue;
            }

            if let Some(field_type) = rust_types.get(&field.type_) {
                match field_type {
                    rust::Type::Struct(_) => {
                        collect_struct_dependencies(&field.type_, rust_types, result);
                    }
                    rust::Type::List(_) | rust::Type::Map(_) => {
                        // Lists and maps already have Default, skip
                    }
                    _ => {}
                }
            }
        }
    }
}

fn collect_type_dependencies(type_name: &str, rust_types: &RustTypes, result: &mut BTreeSet<String>) {
    // Avoid infinite recursion
    if result.contains(type_name) {
        return;
    }

    result.insert(type_name.to_owned());

    // Get the type and recursively add dependencies
    if let Some(rust_type) = rust_types.get(type_name) {
        match rust_type {
            rust::Type::Struct(s) => {
                for field in &s.fields {
                    // Skip non-serializable fields
                    if matches!(field.type_.as_str(), "Body" | "StreamingBlob" | "SelectObjectContentEventStream") {
                        continue;
                    }
                    collect_type_dependencies(&field.type_, rust_types, result);
                }
            }
            rust::Type::List(list) => {
                collect_type_dependencies(&list.member.type_, rust_types, result);
            }
            rust::Type::StructEnum(e) => {
                for variant in &e.variants {
                    collect_type_dependencies(&variant.type_, rust_types, result);
                }
            }
            _ => {}
        }
    }
}

pub fn codegen(rust_types: &RustTypes, ops: &Operations, patch: Option<Patch>) {
    declare_codegen!();

    // Collect types that need serde derives (Configuration types and their dependencies)
    let types_needing_serde = collect_types_needing_serde(rust_types);

    // Collect types that need custom Default implementations
    let types_needing_custom_default = collect_types_needing_custom_default(rust_types);

    g([
        "#![allow(clippy::empty_structs_with_brackets)]",
        "#![allow(clippy::too_many_lines)]",
        "",
        "use super::*;",
        "use crate::error::S3Result;",
        "use crate::post_policy::PostPolicy;",
        "",
        "use std::borrow::Cow;",
        "use std::convert::Infallible;",
        "use std::fmt;",
        "use std::str::FromStr;",
        "",
        "use futures::future::BoxFuture;",
        "use stdx::default::default;",
        "use serde::{Serialize, Deserialize};",
        "",
    ]);

    for rust_type in rust_types.values() {
        match rust_type {
            rust::Type::Alias(ty) => {
                codegen_doc(ty.doc.as_deref());
                g!("pub type {} = {};", ty.name, ty.type_);
            }
            rust::Type::Provided(_) => {}
            rust::Type::List(ty) => {
                codegen_doc(ty.doc.as_deref());
                g!("pub type {} = List<{}>;", ty.name, ty.member.type_);
            }
            rust::Type::Map(ty) => {
                codegen_doc(ty.doc.as_deref());
                g!("pub type {} = Map<{}, {}>;", ty.name, ty.key_type, ty.value_type);
            }
            rust::Type::StrEnum(ty) => {
                let needs_serde = types_needing_serde.contains(&ty.name);
                codegen_str_enum(ty, rust_types, needs_serde);
            }
            rust::Type::Struct(ty) => {
                let needs_serde = types_needing_serde.contains(&ty.name);
                let needs_custom_default = types_needing_custom_default.contains(&ty.name);
                codegen_struct(ty, rust_types, ops, needs_serde, needs_custom_default);
            }
            rust::Type::StructEnum(ty) => {
                let needs_serde = types_needing_serde.contains(&ty.name);
                codegen_struct_enum(ty, rust_types, needs_serde);
            }
            rust::Type::Timestamp(ty) => {
                codegen_doc(ty.doc.as_deref());
                g!("pub type {} = Timestamp;", ty.name);
            }
        }
        g!();
    }

    codegen_tests(ops, rust_types);
    codegen_builders(rust_types, ops);

    codegen_dto_ext(rust_types);
    codegen_post_object_mapping_helpers(rust_types);

    if matches!(patch, Some(Patch::Minio)) {
        super::minio::codegen_in_dto();
    }
}

fn codegen_post_object_mapping_helpers(rust_types: &RustTypes) {
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

fn codegen_struct(ty: &rust::Struct, rust_types: &RustTypes, ops: &Operations, needs_serde: bool, needs_custom_default: bool) {
    codegen_doc(ty.doc.as_deref());

    {
        let derives = struct_derives(ty, rust_types, ops, needs_serde);
        if !derives.is_empty() {
            g!("#[derive({})]", derives.join(", "));
        }
    }

    // g!("#[non_exhaustive]"); // TODO: builder?

    g!("pub struct {} {{", ty.name);
    for field in &ty.fields {
        codegen_doc(field.doc.as_deref());
        if field.option_type {
            g!("    pub {}: Option<{}>,", field.name, field.type_);
        } else {
            g!("    pub {}: {},", field.name, field.type_);
        }
    }
    g!("}}");
    g!();

    g!("impl fmt::Debug for {} {{", ty.name);
    g!("fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {{");
    g!("let mut d = f.debug_struct(\"{}\");", ty.name);
    for field in &ty.fields {
        if let Some(name) = &field.custom_in_derive_debug {
            assert!(field.option_type);
            g!("if self.{}.is_some() {{", field.name);
            g!("d.field(\"{}\", {});", field.name, name);
            g!("}}");
        } else if field.option_type {
            g!("if let Some(ref val) = self.{} {{", field.name);
            g!("d.field(\"{}\", val);", field.name);
            g!("}}");
        } else {
            g!("d.field(\"{0}\", &self.{0});", field.name);
        }
    }
    g!("d.finish_non_exhaustive()");
    g!("}}");
    g!("}}");
    g!();

    if ty.fields.iter().any(|field| field.position == "sealed") {
        g!("#[allow(clippy::clone_on_copy)]");
        g!("impl Clone for {} {{", ty.name);
        g!("fn clone(&self) -> Self {{");
        g!("Self {{");
        for field in &ty.fields {
            if field.position == "sealed" {
                g!("{}: default(),", field.name);
            } else {
                g!("{}: self.{}.clone(),", field.name, field.name);
            }
        }
        g!("}}");
        g!("}}");
        g!("}}");

        g!("impl PartialEq for {} {{", ty.name);
        g!("fn eq(&self, other: &Self) -> bool {{");
        for field in &ty.fields {
            if field.position == "sealed" {
                continue;
            }
            g!("if self.{} != other.{} {{", field.name, field.name);
            g!("return false;");
            g!("}}");
        }
        g!("true");
        g!("}}");
        g!("}}");
    }

    // Add custom Default implementation for types that need it
    if needs_custom_default {
        codegen_custom_default(ty, rust_types);
    }

    if is_op_input(&ty.name, ops) {
        g!("impl {} {{", ty.name);

        g!("#[must_use]");
        g!("pub fn builder() -> builders::{}Builder {{", ty.name);
        g!("default()");
        g!("}}");

        g!("}}");
    }
}

fn codegen_custom_default(ty: &rust::Struct, rust_types: &RustTypes) {
    g!("impl Default for {} {{", ty.name);
    g!("fn default() -> Self {{");
    g!("Self {{");
    for field in &ty.fields {
        if field.option_type {
            g!("{}: None,", field.name);
        } else if let Some(rust_type) = rust_types.get(&field.type_) {
            match rust_type {
                rust::Type::List(_) | rust::Type::Map(_) => {
                    g!("{}: default(),", field.name);
                }
                rust::Type::Alias(_) => {
                    // Type aliases to primitives implement Default
                    g!("{}: default(),", field.name);
                }
                rust::Type::StrEnum(_) => {
                    // StrEnum types need a string value, use empty string
                    g!("{}: String::new().into(),", field.name);
                }
                rust::Type::Struct(_) => {
                    // Try to use Default::default() for structs
                    g!("{}: default(),", field.name);
                }
                _ => {
                    g!("{}: default(),", field.name);
                }
            }
        } else {
            // Unknown type, try Default::default()
            g!("{}: default(),", field.name);
        }
    }
    g!("}}");
    g!("}}");
    g!("}}");
    g!();
}

fn codegen_str_enum(ty: &rust::StrEnum, _rust_types: &RustTypes, needs_serde: bool) {
    codegen_doc(ty.doc.as_deref());
    if needs_serde {
        g!("#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]");
    } else {
        g!("#[derive(Debug, Clone, PartialEq, Eq)]");
    }
    g!("pub struct {}(Cow<'static, str>);", ty.name);
    g!();

    g!("impl {} {{", ty.name);
    {
        for variant in &ty.variants {
            codegen_doc(variant.doc.as_deref());
            g!("pub const {}: &'static str = \"{}\";", variant.name, variant.value);
            g!();
        }

        g([
            "#[must_use]",
            "pub fn as_str(&self) -> &str {",
            "&self.0",
            "}",
            "", //
        ]);

        g([
            "#[must_use]",
            "pub fn from_static(s: &'static str) -> Self {",
            "Self(Cow::from(s))",
            "}",
            "",
        ]);
    }
    g!("}}");
    g!();

    g!("impl From<String> for {} {{", ty.name);
    g!("fn from(s: String) -> Self {{");
    g!("Self(Cow::from(s))");
    g!("}}");
    g!("}}");
    g!();

    g!("impl From<{}> for Cow<'static, str> {{", ty.name);
    g!("fn from(s: {}) -> Self {{", ty.name);
    g!("s.0");
    g!("}}");
    g!("}}");
    g!();

    g!("impl FromStr for {} {{", ty.name);
    g!("type Err = Infallible;");
    g!("fn from_str(s: &str) -> Result<Self, Self::Err> {{");
    g!("Ok(Self::from(s.to_owned()))");
    g!("}}");
    g!("}}");
}

fn codegen_struct_enum(ty: &rust::StructEnum, rust_types: &RustTypes, needs_serde: bool) {
    codegen_doc(ty.doc.as_deref());

    if needs_serde {
        // Check if all variants can be serialized
        let can_serde = ty.variants.iter().all(|v| {
            // Check for known non-serializable types
            if matches!(v.type_.as_str(), "Body" | "StreamingBlob" | "SelectObjectContentEventStream") {
                return false;
            }

            // Check if the variant type can be serialized
            match rust_types.get(&v.type_) {
                Some(rust::Type::Struct(s)) => can_derive_serde(s, rust_types),
                _ => true,
            }
        });

        if can_serde {
            g!("#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]");
            g!("#[non_exhaustive]");
            g!("#[serde(rename_all = \"PascalCase\")]");
        } else {
            g!("#[derive(Debug, Clone, PartialEq)]");
            g!("#[non_exhaustive]");
        }
    } else {
        g!("#[derive(Debug, Clone, PartialEq)]");
        g!("#[non_exhaustive]");
    }

    g!("pub enum {} {{", ty.name);

    for variant in &ty.variants {
        codegen_doc(variant.doc.as_deref());
        g!("    {}({}),", variant.name, variant.type_);
    }

    g!("}}");
}

fn codegen_tests(ops: &Operations, rust_types: &RustTypes) {
    g([
        "#[cfg(test)]",
        "mod tests {",
        "use super::*;",
        "",
        "fn require_default<T: Default>() {}",
        "fn require_clone<T: Clone>() {}",
        "",
    ]);

    {
        g!("#[test]");
        g!("fn test_default() {{");
        for op in ops.values() {
            g!("require_default::<{}>();", op.output);
        }
        g!("}}");
    }

    {
        g!("#[test]");
        g!("fn test_clone() {{");
        for op in ops.values() {
            if let Some(rust::Type::Struct(ty)) = rust_types.get(&op.input) {
                if can_derive_clone(ty, rust_types) {
                    g!("require_clone::<{}>();", op.input);
                }
            }
            if let Some(rust::Type::Struct(ty)) = rust_types.get(&op.output) {
                if can_derive_clone(ty, rust_types) {
                    g!("require_clone::<{}>();", op.output);
                }
            }
        }
        g!("}}");
    }

    g!("}}");
}

fn struct_derives(ty: &rust::Struct, rust_types: &RustTypes, _ops: &Operations, needs_serde: bool) -> Vec<&'static str> {
    let mut derives = Vec::new();
    let can_clone = can_derive_clone(ty, rust_types);
    if can_clone {
        derives.push("Clone");
    }
    if can_derive_default(ty, rust_types) {
        derives.push("Default");
    }
    if can_derive_partial_eq(ty, rust_types) {
        derives.push("PartialEq");
    }

    // Add Serialize and Deserialize only to types that are needed for Configuration serialization
    if needs_serde && can_derive_serde(ty, rust_types) {
        derives.push("Serialize");
        derives.push("Deserialize");
    }
    derives
}

fn can_derive_serde(ty: &rust::Struct, rust_types: &RustTypes) -> bool {
    ty.fields.iter().all(|field| {
        if field.position == "sealed" {
            // Allow sealed CachedTags fields since they have custom Serialize/Deserialize implementation
            if field.type_ != "CachedTags" {
                return false;
            }
        }
        if field.position == "s3s" {
            return false;
        }
        // Body, StreamingBlob, and event streams can't be serialized with regular serde
        // Note: CachedTags is now serializable with custom implementation
        if matches!(field.type_.as_str(), "Body" | "StreamingBlob" | "SelectObjectContentEventStream") {
            return false;
        }

        // Check if the field's type can be serialized recursively
        if let Some(field_ty) = rust_types.get(&field.type_) {
            match field_ty {
                rust::Type::Struct(s) => {
                    if !can_derive_serde(s, rust_types) {
                        return false;
                    }
                }
                rust::Type::List(list) => {
                    // Check if the list element type can be serialized
                    if let Some(rust::Type::Struct(s)) = rust_types.get(&list.member.type_)
                        && !can_derive_serde(s, rust_types)
                    {
                        return false;
                    }
                }
                _ => {}
            }
        }

        true
    })
}

fn can_derive_clone(ty: &rust::Struct, _rust_types: &RustTypes) -> bool {
    ty.fields.iter().all(|field| {
        // Sealed fields need custom Clone implementation
        if field.position == "sealed" {
            return false;
        }
        if field.position == "s3s" {
            return false;
        }
        if matches!(field.type_.as_str(), "StreamingBlob" | "SelectObjectContentEventStream") {
            return false;
        }
        true
    })
}

fn can_derive_partial_eq(ty: &rust::Struct, _rust_types: &RustTypes) -> bool {
    ty.fields.iter().all(|field| {
        // Sealed fields need custom PartialEq implementation
        if field.position == "sealed" {
            return false;
        }
        if field.position == "s3s" {
            return false;
        }
        if matches!(field.type_.as_str(), "StreamingBlob" | "SelectObjectContentEventStream") {
            return false;
        }
        true
    })
}

fn can_derive_default(ty: &rust::Struct, rust_types: &RustTypes) -> bool {
    ty.fields.iter().all(|field| {
        if field.option_type {
            return true;
        }

        match &rust_types[&field.type_] {
            rust::Type::Provided(ty) => {
                if ty.name == "CachedTags" {
                    return true;
                }
            }
            rust::Type::List(_) => return true,
            rust::Type::Map(_) => return true,
            rust::Type::Alias(alias_ty) => {
                // Type aliases to primitive types that have Default
                if matches!(alias_ty.type_.as_str(), "String" | "bool" | "i32" | "i64" | "f32" | "f64") {
                    return true;
                }
            }
            _ => {}
        }

        field.default_value.as_ref().is_some_and(is_rust_default)
    })
}

fn is_rust_default(v: &Value) -> bool {
    match v {
        Value::Bool(x) => !x,
        Value::Number(x) => x.as_i64() == Some(0),
        Value::String(x) => x.is_empty(),
        _ => unimplemented!("{v:#?}"),
    }
}

fn codegen_builders(rust_types: &RustTypes, ops: &Operations) {
    g([
        "pub mod builders {", //
        "#![allow(clippy::missing_errors_doc)]",
        "",
        "use super::*;",
        "pub use super::build_error::BuildError;",
        "",
    ]);

    for op in ops.values() {
        let rust::Type::Struct(ty) = &rust_types[&op.input] else { continue };
        codegen_struct_builder(ty, rust_types);
        g!();
    }

    g!("}}");
}

fn is_list_or_map(name: &str, rust_types: &RustTypes) -> bool {
    matches!(&rust_types[name], rust::Type::List(_) | rust::Type::Map(_))
}

fn codegen_struct_builder(ty: &rust::Struct, rust_types: &RustTypes) {
    g!("/// A builder for [`{}`]", ty.name);

    g!("#[derive(Default)]");
    g!("pub struct {}Builder {{", ty.name);

    for field in &ty.fields {
        if field.option_type {
            g!("{}: Option<{}>,", field.name, field.type_);
            g!();
            continue;
        }

        if is_list_or_map(&field.type_, rust_types) {
            g!("{}: {},", field.name, field.type_);
            g!();
            continue;
        }

        if let Some(ref v) = field.default_value {
            assert!(is_rust_default(v));
            g!("{}: {},", field.name, field.type_);
            g!();
            continue;
        }

        g!("{}: Option<{}>,", field.name, field.type_);
        g!();
    }

    g!("}}");
    g!();

    g!("impl {}Builder {{", ty.name);

    for field in &ty.fields {
        let field_name = field.name.as_str();

        let struct_field_type = if field.option_type {
            Cow::Owned(format!("Option<{}>", field.type_))
        } else {
            Cow::Borrowed(&field.type_)
        };

        let needs_wrap = !(field.option_type || field.default_value.is_some() || is_list_or_map(&field.type_, rust_types));

        g!("pub fn set_{field_name}(&mut self, field: {struct_field_type}) -> &mut Self {{");

        if needs_wrap {
            g!("    self.{field_name} = Some(field);");
        } else {
            g!("    self.{field_name} = field;");
        }

        g!("self");

        g!("}}");
        g!();
    }

    for field in &ty.fields {
        let field_name = field.name.as_str();

        let struct_field_type = if field.option_type {
            Cow::Owned(format!("Option<{}>", field.type_))
        } else {
            Cow::Borrowed(&field.type_)
        };

        let needs_wrap = !(field.option_type || field.default_value.is_some() || is_list_or_map(&field.type_, rust_types));

        g!("#[must_use]");
        g!("pub fn {field_name}(mut self, field: {struct_field_type}) -> Self {{");

        if needs_wrap {
            g!("    self.{field_name} = Some(field);");
        } else {
            g!("    self.{field_name} = field;");
        }

        g!("self");

        g!("}}");
        g!();
    }

    {
        g!("pub fn build(self) -> Result<{}, BuildError> {{", ty.name);

        for field in &ty.fields {
            let field_name = field.name.as_str();

            if field.option_type || field.default_value.is_some() || is_list_or_map(&field.type_, rust_types) {
                g!("let {field_name} = self.{field_name};");
            } else {
                g!("let {field_name} = self.{field_name}.ok_or_else(|| BuildError::missing_field({field_name:?}))?;");
            }
        }

        g!("Ok({} {{", ty.name);
        for field in &ty.fields {
            g!("{},", field.name);
        }
        g!("}})");

        g!("}}");
        g!();
    }

    g!("}}");
    g!();
}

fn codegen_dto_ext(rust_types: &RustTypes) {
    g!("pub trait DtoExt {{");
    g!("    /// Modifies all empty string fields from `Some(\"\")` to `None`");
    g!("    fn ignore_empty_strings(&mut self);");
    g!("}}");

    for ty in rust_types.values() {
        let rust::Type::Struct(ty) = ty else { continue };

        if ty.fields.is_empty() {
            continue;
        }

        g!("impl DtoExt for {} {{", ty.name);
        g!("    fn ignore_empty_strings(&mut self) {{");
        for field in &ty.fields {
            let Some(field_ty) = rust_types.get(&field.type_) else { continue };

            match field_ty {
                rust::Type::Alias(field_ty) => {
                    if field.option_type && field_ty.type_ == "String" {
                        g!("if self.{}.as_deref() == Some(\"\") {{", field.name);
                        g!("    self.{} = None;", field.name);
                        g!("}}");
                    }
                }
                rust::Type::StrEnum(_) => {
                    if field.option_type {
                        g!("if let Some(ref val) = self.{}", field.name);
                        g!("    && val.as_str() == \"\" {{");
                        g!("    self.{} = None;", field.name);
                        g!("}}");
                    }
                }
                rust::Type::Struct(field_ty) => {
                    if field_ty.fields.is_empty() {
                        continue;
                    }
                    if field.option_type {
                        g!("if let Some(ref mut val) = self.{} {{", field.name);
                        g!("val.ignore_empty_strings();");
                        g!("}}");
                    } else {
                        g!("self.{}.ignore_empty_strings();", field.name);
                    }
                }
                _ => {}
            }
        }
        g!("}}");
        g!("}}");
    }
}
