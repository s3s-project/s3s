// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

//! The dto test family: string-enum accessors and conversions, operation input
//! builders and the `DtoExt` implementation of the generated dto types.
//!
//! Every case is generated from the model data of one feature configuration, and
//! a case whose text differs between the base and the union (`MinIO`) model is
//! emitted twice behind the same `#[cfg]` gates as the items of
//! `s3s::dto::generated`.

use crate::v1::dto::RustTypes;
use crate::v1::ops::Operations;
use crate::v1::rust;

use super::debug::FieldVal;
use super::debug::Mode;
use super::debug::Val;
use super::debug::expected_expr;
use super::debug::is_list_or_map;
use super::debug::push_pair;
use super::debug::spec;
use super::debug::val_expr;
use super::{codegen_file_header, is_minio_only, write_test_file};

use std::collections::BTreeSet;
use std::fmt::Write as _;

use heck::ToSnakeCase;
use scoped_writer::g;
use serde_json::Value;

/// Whether the type has a builder, i.e. it is the unified input of an operation.
fn has_builder(input: &str, ops: &Operations) -> bool {
    input.strip_suffix("Input").is_some_and(|op| ops.contains_key(op))
}

/// One case per string enum: every variant through `from_static`, `as_str`, the
/// derived `Debug`, `From<String>`, `FromStr` and the `Cow` conversion, plus a
/// value the model does not list.
fn str_enum_items(types: &RustTypes, name: &str) -> Vec<String> {
    let Some(rust::Type::StrEnum(ty)) = types.get(name) else {
        return Vec::new();
    };
    if ty.variants.is_empty() {
        return Vec::new();
    }

    let name = &ty.name;
    let count = ty.variants.len();
    let variants = ty
        .variants
        .iter()
        .map(|variant| format!("s3s::dto::{name}::{}", variant.name))
        .collect::<Vec<_>>()
        .join(", ");

    let case = format!(
        "#[test]\n\
         fn str_enum_{snake}() {{\n\
         \x20   let variants: [&'static str; {count}] = [{variants}];\n\
         \x20   for variant in variants {{\n\
         \x20       let value = s3s::dto::{name}::from_static(variant);\n\
         \x20       assert_eq!(value.as_str(), variant);\n\
         \x20       assert_eq!(format!(\"{{value:?}}\"), format!(\"{name}({{variant:?}})\"));\n\
         \x20       let from_string: s3s::dto::{name} = String::from(variant).into();\n\
         \x20       assert_eq!(from_string.as_str(), variant);\n\
         \x20       let from_str: s3s::dto::{name} = variant.parse().expect(\"infallible\");\n\
         \x20       assert_eq!(from_str.as_str(), variant);\n\
         \x20       let cow: std::borrow::Cow<'static, str> = s3s::dto::{name}::from_static(variant).into();\n\
         \x20       assert_eq!(cow, variant);\n\
         \x20   }}\n\
         \x20   let unknown: s3s::dto::{name} = \"s3s-unknown-value\".parse().expect(\"infallible\");\n\
         \x20   assert_eq!(unknown.as_str(), \"s3s-unknown-value\");\n\
         \x20   let unknown_from: s3s::dto::{name} = String::from(\"s3s-unknown-from-string\").into();\n\
         \x20   assert_eq!(unknown_from.as_str(), \"s3s-unknown-from-string\");\n\
         }}",
        snake = name.to_snake_case(),
    );
    vec![case]
}

/// The builder argument of a field: optional fields take an Option.
fn builder_arg(field: &FieldVal) -> String {
    let Some(val) = &field.val else {
        panic!("a builder sample field must hold a value: {}", field.name);
    };
    let expr = val_expr(val);
    if field.optional { format!("Some({expr})") } else { expr }
}

/// One case per operation input: every `set_*` setter, every consuming builder
/// method, `build()`, the inherent `builder()` entry point and the missing-field
/// error of `build()`.
fn builder_items(types: &RustTypes, ops: &Operations, op_name: &str) -> Vec<String> {
    let Some(op) = ops.get(op_name) else {
        return Vec::new();
    };
    let Some(rust::Type::Struct(input)) = types.get(&op.input) else {
        return Vec::new();
    };

    let full = spec(types, &op.input, Mode::Full, &op.input);
    let Val::Struct { fields, .. } = &full else {
        return Vec::new();
    };
    assert_eq!(fields.len(), input.fields.len(), "sample field count mismatch: {}", op.input);

    let name = &op.input;
    let snake = name.to_snake_case();
    let expected = expected_expr(types, &full);

    let mut body = String::new();
    if has_builder(name, ops) {
        let _ = writeln!(body, "    let mut builder = s3s::dto::{name}::builder();");
    } else {
        let _ = writeln!(body, "    let mut builder = s3s::dto::builders::{name}Builder::default();");
    }
    for field in fields {
        let _ = writeln!(body, "    builder.set_{}({});", field.name, builder_arg(field));
    }
    let _ = writeln!(body, "    let value = builder.build().expect(\"build {name}\");");
    let _ = writeln!(body, "    assert_eq!(format!(\"{{value:?}}\"), {expected});\n");

    let mut chain = String::new();
    for field in fields {
        let _ = write!(chain, ".{}({})", field.name, builder_arg(field));
    }
    let _ = writeln!(
        body,
        "    let value = s3s::dto::builders::{name}Builder::default(){chain}.build().expect(\"build {name}\");"
    );
    let _ = writeln!(body, "    assert_eq!(format!(\"{{value:?}}\"), {expected});");

    if let Some(required) = input
        .fields
        .iter()
        .find(|field| !field.option_type && field.default_value.is_none() && !is_list_or_map(types, &field.type_))
    {
        let message = format!("Missing field: {:?}", required.name);
        let _ = writeln!(
            body,
            "\n    let err = s3s::dto::builders::{name}Builder::default().build().unwrap_err();\n    assert_eq!(err.to_string(), {message:?});"
        );
    }

    vec![format!("#[test]\nfn builder_{snake}() {{\n{body}}}")]
}

/// One case per struct with fields: `DtoExt::ignore_empty_strings` must clear every
/// empty optional string-like field, and leave everything else untouched.
fn dto_ext_items(types: &RustTypes, name: &str) -> Vec<String> {
    let Some(rust::Type::Struct(ty)) = types.get(name) else {
        return Vec::new();
    };
    if ty.fields.is_empty() {
        return Vec::new();
    }

    let blank = spec(types, &ty.name, Mode::Blank, &ty.name);
    let clean = spec(types, &ty.name, Mode::Clean, &ty.name);
    let value = val_expr(&blank);
    let expected = expected_expr(types, &clean);
    let case = format!(
        "#[test]\n\
         fn dto_ext_{snake}() {{\n\
         \x20   let mut value = {value};\n\
         \x20   value.ignore_empty_strings();\n\
         \x20   assert_eq!(format!(\"{{value:?}}\"), {expected});\n\
         }}",
        snake = ty.name.to_snake_case(),
    );
    vec![case]
}

pub(super) fn codegen(ops: &Operations, rust_types_base: &RustTypes, rust_types_minio: &RustTypes) {
    let mut items: Vec<String> = Vec::new();

    for name in rust_types_minio.keys() {
        let base = str_enum_items(rust_types_base, name);
        let minio = str_enum_items(rust_types_minio, name);
        push_pair(&mut items, base, minio);
    }

    for op_name in ops.keys() {
        let is_minio = ops[op_name].is_minio;
        let base = if is_minio {
            Vec::new()
        } else {
            builder_items(rust_types_base, ops, op_name)
        };
        let minio = builder_items(rust_types_minio, ops, op_name);
        push_pair(&mut items, base, minio);
    }

    for name in rust_types_minio.keys() {
        let base = dto_ext_items(rust_types_base, name);
        let minio = dto_ext_items(rust_types_minio, name);
        push_pair(&mut items, base, minio);
    }

    for name in rust_types_minio.keys() {
        let base = if is_minio_only(name, rust_types_base, rust_types_minio) {
            Vec::new()
        } else {
            sealed_items(rust_types_base, name)
        };
        let minio = sealed_items(rust_types_minio, name);
        push_pair(&mut items, base, minio);
    }

    let cached_tags_base = cached_tags_items(rust_types_base);
    let cached_tags_minio = cached_tags_items(rust_types_minio);
    push_pair(&mut items, cached_tags_base, cached_tags_minio);

    let custom_base = types_needing_custom_default(rust_types_base, ops);
    let custom_minio = types_needing_custom_default(rust_types_minio, ops);
    for name in custom_base.union(&custom_minio) {
        let base = if custom_base.contains(name) {
            default_items(rust_types_base, name)
        } else {
            Vec::new()
        };
        let minio = if custom_minio.contains(name) {
            default_items(rust_types_minio, name)
        } else {
            Vec::new()
        };
        push_pair(&mut items, base, minio);
    }

    assert!(!items.is_empty(), "the dto family emitted no case");

    write_test_file("dto.rs", || {
        codegen_file_header(Some("dto"));
        g!("//! DTO accessors, conversions, builders and `DtoExt`, generated from the");
        g!("//! model data of both feature configurations.");
        g!();
        g!("#![allow(clippy::too_many_lines)]");
        g!();
        g!("use s3s::dto::DtoExt as _;");
        g!();
        for item in &items {
            g!("{}", item);
            g!();
        }
    });
}

/// One case per struct with a sealed cache field: the hand written `Clone` and
/// `PartialEq` implementations plus the `test_tags` entry point.
///
/// `Clone` drops the cache instead of copying it, `PartialEq` ignores it and
/// compares every other field, so each compared field gets its own probe.
fn sealed_items(types: &RustTypes, name: &str) -> Vec<String> {
    let Some(rust::Type::Struct(ty)) = types.get(name) else {
        return Vec::new();
    };
    if !ty.fields.iter().any(|field| field.position == "sealed") {
        return Vec::new();
    }

    let mut body = String::new();
    let _ = writeln!(body, "    let value = super::debug::{}();", super::debug::sample_fn(name));
    let _ = writeln!(
        body,
        "    let clone = value.clone();\n    assert!(clone == value, \"{name}: a clone must compare equal\");"
    );
    for field in &ty.fields {
        if field.position == "sealed" || !field.option_type {
            continue;
        }
        let field_name = &field.name;
        let _ = writeln!(
            body,
            "    let mut other = value.clone();\n    other.{field_name} = None;\n    assert!(value != other, \"{name}: the {field_name} field takes part in PartialEq\");"
        );
    }
    let compared: Vec<&str> = ty
        .fields
        .iter()
        .filter(|field| field.position != "sealed" && field.option_type)
        .map(|field| field.name.as_str())
        .collect();
    let _ = writeln!(body, "    let mut bare = value.clone();");
    for field in &compared {
        let _ = writeln!(body, "    bare.{field} = None;");
    }
    let _ = writeln!(
        body,
        "    assert!(bare.test_tags(&std::collections::HashMap::new()), \"{name}: a filter without tags accepts an untagged object\");"
    );
    let _ = writeln!(
        body,
        "    assert!(!value.test_tags(&std::collections::HashMap::new()), \"{name}: an untagged object fails a tag constraint\");"
    );

    vec![format!("#[test]\nfn sealed_{}() {{\n{body}}}", name.to_snake_case())]
}

/// The `CachedTags` case: the cache is skipped by the hand written `Clone` and
/// `PartialEq`, serializes as an empty map and resets to an empty cache.
fn cached_tags_items(types: &RustTypes) -> Vec<String> {
    if !types.contains_key("CachedTags") {
        return Vec::new();
    }
    let case = "#[test]\n\
                fn cached_tags_is_a_cache() {\n\
                \x20   let tags = s3s::dto::CachedTags::default();\n\
                \x20   let clone = tags.clone();\n\
                \x20   assert!(clone == tags, \"a cache compares equal regardless of its contents\");\n\
                \x20   assert_eq!(serde_json::to_string(&tags).expect(\"serialize a cache\"), \"{}\");\n\
                \x20   let parsed: s3s::dto::CachedTags =\n\
                \x20       serde_json::from_str(\"{\\\"key\\\":\\\"value\\\"}\").expect(\"deserialize a cache\");\n\
                \x20   assert!(parsed == tags, \"a cache deserializes to an empty cache\");\n\
                \x20   let mut reset = tags.clone();\n\
                \x20   reset.reset();\n\
                }";
    vec![case.to_owned()]
}
/// Whether the struct can derive `Default`, mirroring
/// `s3s_codegen::v1::dto::can_derive_default`: a field whose type has no
/// `Default` and no Rust-default value forces the hand written implementation.
fn can_derive_default(types: &RustTypes, ty: &rust::Struct) -> bool {
    ty.fields.iter().all(|field| {
        if field.option_type {
            return true;
        }
        match types.get(&field.type_) {
            Some(rust::Type::Provided(provided)) if provided.name == "CachedTags" => return true,
            Some(rust::Type::List(_) | rust::Type::Map(_)) => return true,
            Some(rust::Type::Alias(alias))
                if matches!(alias.type_.as_str(), "String" | "bool" | "i32" | "i64" | "f32" | "f64") =>
            {
                return true;
            }
            _ => {}
        }
        field.default_value.as_ref().is_some_and(is_rust_default)
    })
}

/// Whether a model default value is the Rust default of its type.
fn is_rust_default(value: &Value) -> bool {
    match value {
        Value::Bool(x) => !x,
        Value::Number(x) => x.as_i64() == Some(0),
        Value::String(x) => x.is_empty(),
        _ => false,
    }
}

/// Adds the types reachable from `name` that need a hand written `Default`,
/// mirroring `s3s_codegen::v1::dto::collect_struct_dependencies`.
fn collect_struct_dependencies(types: &RustTypes, name: &str, out: &mut BTreeSet<String>) {
    if out.contains(name) {
        return;
    }
    if let Some(rust::Type::Struct(ty)) = types.get(name)
        && !can_derive_default(types, ty)
    {
        out.insert(name.to_owned());
        for field in &ty.fields {
            if field.option_type {
                continue;
            }
            if matches!(types.get(&field.type_), Some(rust::Type::Struct(_))) {
                collect_struct_dependencies(types, &field.type_, out);
            }
        }
    }
}

/// The types whose `Default` is written by hand: the generated file carries an
/// `impl Default` for them instead of a derive.
fn types_needing_custom_default(types: &RustTypes, ops: &Operations) -> BTreeSet<String> {
    let mut out: BTreeSet<String> = BTreeSet::new();

    for (name, ty) in types {
        if name.ends_with("Configuration")
            && let rust::Type::Struct(ty) = ty
            && !can_derive_default(types, ty)
        {
            collect_struct_dependencies(types, name, &mut out);
        }
    }

    for op in ops.values() {
        if let Some(rust::Type::Struct(ty)) = types.get(&op.output)
            && !can_derive_default(types, ty)
        {
            collect_struct_dependencies(types, &op.output, &mut out);
        }
    }

    out
}

/// The value that the hand written `Default` builds: every optional field is
/// `None`, a string enum is the empty string, and every other required field is
/// the default of its type.
fn default_val(types: &RustTypes, name: &str) -> Val {
    match types.get(name) {
        Some(rust::Type::Struct(ty)) => Val::Struct {
            name: ty.name.clone(),
            fields: ty
                .fields
                .iter()
                .map(|field| FieldVal {
                    name: field.name.clone(),
                    optional: field.option_type,
                    val: if field.option_type {
                        None
                    } else {
                        Some(default_field_val(types, &field.type_))
                    },
                })
                .collect(),
        },
        _ => default_field_val(types, name),
    }
}

fn default_field_val(types: &RustTypes, name: &str) -> Val {
    match types.get(name) {
        Some(rust::Type::Struct(_)) => default_val(types, name),
        Some(rust::Type::StrEnum(ty)) => Val::Leaf {
            expr: format!("s3s::dto::{}::from_static(\"\")", ty.name),
            debug: format!("{}(\"\")", ty.name),
        },
        Some(rust::Type::Alias(alias)) => match alias.type_.as_str() {
            "String" => Val::Leaf {
                expr: "String::new()".to_owned(),
                debug: "\"\"".to_owned(),
            },
            "bool" => Val::Leaf {
                expr: "false".to_owned(),
                debug: "false".to_owned(),
            },
            "i32" | "i64" => Val::Leaf {
                expr: "0".to_owned(),
                debug: "0".to_owned(),
            },
            "f32" | "f64" => Val::Leaf {
                expr: "0.0".to_owned(),
                debug: "0.0".to_owned(),
            },
            _ => Val::Opaque(format!("s3s::dto::{name}::default()")),
        },
        Some(rust::Type::List(_)) => Val::Leaf {
            expr: "Vec::new()".to_owned(),
            debug: "[]".to_owned(),
        },
        Some(rust::Type::Map(_)) => Val::Leaf {
            expr: "std::collections::HashMap::new()".to_owned(),
            debug: "{}".to_owned(),
        },
        Some(rust::Type::Timestamp(_) | rust::Type::Provided(_) | rust::Type::StructEnum(_)) | None => {
            Val::Opaque(format!("s3s::dto::{name}::default()"))
        }
    }
}

/// One case per type whose `Default` is written by hand: the value must match
/// the rule of the implementation, asserted through its Debug output.
fn default_items(types: &RustTypes, name: &str) -> Vec<String> {
    let expected = expected_expr(types, &default_val(types, name));
    let case = format!(
        "#[test]\n\
         fn default_{snake}() {{\n\
         \x20   let value = s3s::dto::{name}::default();\n\
         \x20   assert_eq!(format!(\"{{value:?}}\"), {expected});\n\
         }}",
        snake = name.to_snake_case(),
    );
    vec![case]
}
