use super::o;
use super::ops::{Operations, SKIPPED_OPS, is_op_input, is_op_output};
use super::rust::codegen_doc;
use super::smithy::SmithyTraitsExt;
use super::{rust, smithy};

use crate::declare_codegen;
use crate::v1::Patch;

use std::borrow::Cow;
use std::collections::BTreeMap;
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

        // ETag-related header types that should be aliased to ETag instead of String
        let etag_alias_types = [
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

        if etag_alias_types.contains(&rs_shape_name.as_str()) {
            if let smithy::Shape::String(shape) = shape {
                let ty = rust::Type::alias(&rs_shape_name, "ETag", shape.traits.doc());
                insert(rs_shape_name, ty);
                continue;
            }
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
                for (field_name, field) in &shape.members {
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

                        camel_name: field_name.clone(),

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

pub fn codegen(rust_types: &RustTypes, ops: &Operations, patch: Option<Patch>) {
    declare_codegen!();

    g([
        "#![allow(clippy::empty_structs_with_brackets)]",
        "#![allow(clippy::too_many_lines)]",
        "",
        "use super::*;",
        "use crate::error::S3Result;",
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
                codegen_str_enum(ty, rust_types);
            }
            rust::Type::Struct(ty) => {
                codegen_struct(ty, rust_types, ops);
            }
            rust::Type::StructEnum(ty) => {
                codegen_struct_enum(ty, rust_types);
            }
            rust::Type::Timestamp(ty) => {
                codegen_doc(ty.doc.as_deref());
                g!("pub type {} = Timestamp;", ty.name);
            }
        }
        g!();
    }

    codegen_tests(ops);
    codegen_builders(rust_types, ops);

    codegen_dto_ext(rust_types);

    if matches!(patch, Some(Patch::Minio)) {
        super::minio::codegen_in_dto();
    }
}

fn codegen_struct(ty: &rust::Struct, rust_types: &RustTypes, ops: &Operations) {
    codegen_doc(ty.doc.as_deref());

    {
        let derives = struct_derives(ty, rust_types, ops);
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

    if is_op_input(&ty.name, ops) {
        g!("impl {} {{", ty.name);

        g!("#[must_use]");
        g!("pub fn builder() -> builders::{}Builder {{", ty.name);
        g!("default()");
        g!("}}");

        g!("}}");
    }
}

fn codegen_str_enum(ty: &rust::StrEnum, _rust_types: &RustTypes) {
    codegen_doc(ty.doc.as_deref());
    g!("#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]");
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

fn codegen_struct_enum(ty: &rust::StructEnum, rust_types: &RustTypes) {
    codegen_doc(ty.doc.as_deref());

    // Check if all variants can be serialized
    let can_serde = ty.variants.iter().all(|v| {
        // Check if the variant type can be serialized
        match rust_types.get(&v.type_) {
            Some(rust::Type::Struct(s)) => can_derive_serde(s, rust_types),
            _ => true, // Assume other types can be serialized
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

    g!("pub enum {} {{", ty.name);

    for variant in &ty.variants {
        codegen_doc(variant.doc.as_deref());
        g!("    {}({}),", variant.name, variant.type_);
    }

    g!("}}");
}

fn codegen_tests(ops: &Operations) {
    g([
        "#[cfg(test)]",
        "mod tests {",
        "use super::*;",
        "",
        "fn require_default<T: Default>() {}",
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

    g!("}}");
}

fn struct_derives(ty: &rust::Struct, rust_types: &RustTypes, ops: &Operations) -> Vec<&'static str> {
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

    // Add Serialize and Deserialize to all struct types except operation inputs/outputs
    // and types that can't be serialized (have Body, streaming blobs, sealed fields, etc.)
    let is_operation = is_op_input(&ty.name, ops) || is_op_output(&ty.name, ops);
    if can_derive_serde(ty, rust_types) && !is_operation {
        derives.push("Serialize");
        derives.push("Deserialize");
    }
    derives
}

fn can_derive_serde(ty: &rust::Struct, _rust_types: &RustTypes) -> bool {
    ty.fields.iter().all(|field| {
        if field.position == "sealed" {
            return false;
        }
        if field.position == "s3s" {
            return false;
        }
        // Body, StreamingBlob, and event streams can't be serialized with regular serde
        if matches!(field.type_.as_str(), "Body" | "StreamingBlob" | "SelectObjectContentEventStream") {
            return false;
        }
        true
    })
}

fn can_derive_clone(ty: &rust::Struct, _rust_types: &RustTypes) -> bool {
    ty.fields.iter().all(|field| {
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
                        g!("if let Some(ref val) = self.{} {{", field.name);
                        g!("    if val.as_str() == \"\" {{");
                        g!("        self.{} = None;", field.name);
                        g!("    }}");
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
