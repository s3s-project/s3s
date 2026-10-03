// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

//! The Debug test family: one fully populated sample value per generated dto
//! type, with the Debug output asserted against the text derived from the model
//! data.
//!
//! The family also owns the sample functions of the generated test binary: one
//! `pub(super) fn sample_<type>()` per dto struct, reused by the other families
//! through `super::debug`. A sample value fills every optional field, so every
//! branch of a generated `Debug` implementation runs, and the expected text is
//! derived from the same model data that produced the implementation.

use crate::v1::dto::RustTypes;
use crate::v1::ops::Operations;
use crate::v1::rust;

use super::{codegen_file_header, is_minio_only, write_test_file};

use std::fmt::Write as _;

use heck::ToSnakeCase;
use scoped_writer::g;

/// A fixed instant used for every timestamp-typed field.
const TIMESTAMP: &str =
    "s3s::dto::Timestamp::parse(s3s::dto::TimestampFormat::DateTime, \"1985-04-12T23:20:50.520Z\").expect(\"valid timestamp\")";

/// Which sample value of a type is built.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Mode {
    /// Every optional field is `Some`; string-typed optional fields are not empty.
    Full,
    /// Like `Mode::Full`, but optional string-like fields hold the empty string.
    Blank,
    /// Like `Mode::Blank`, with those fields already set to `None`, i.e. the value
    /// that `DtoExt::ignore_empty_strings` must produce.
    Clean,
}

/// One field of a sample struct value.
pub(super) struct FieldVal {
    pub(super) name: String,
    pub(super) optional: bool,
    /// None keeps the field at None; only used for optional fields.
    pub(super) val: Option<Val>,
}

/// A sample value of one dto type.
pub(super) enum Val {
    /// A value whose Debug output is known exactly.
    Leaf { expr: String, debug: String },
    /// A value whose Debug output is rendered at run time.
    Opaque(String),
    /// A struct literal, expanded in place.
    Struct { name: String, fields: Vec<FieldVal> },
    /// A call to the sample function of a struct type.
    Nested { name: String, mode: Mode },
    /// A struct enum variant built from a sample of its payload.
    Variant { name: String, variant: String, inner: Box<Val> },
    /// A list with one element.
    List(Vec<Val>),
    /// A map with one entry.
    Map(Vec<(Val, Val)>),
}

/// One part of an assembled Debug output.
enum Part {
    Text(String),
    Child(Frag),
}

/// The expected Debug text of a sample value.
enum Frag {
    /// The exact text.
    Lit(String),
    /// A Rust expression evaluating to the text.
    Expr(String),
}

/// Name of the sample function of a struct type.
pub(super) fn sample_fn(name: &str) -> String {
    format!("sample_{}", name.to_snake_case())
}

/// Whether a field type is turned into `None` by `DtoExt::ignore_empty_strings`.
fn is_blankable(types: &RustTypes, name: &str) -> bool {
    match types.get(name) {
        Some(rust::Type::Alias(ty)) => ty.type_ == "String",
        Some(rust::Type::StrEnum(_)) => true,
        _ => false,
    }
}

/// Whether the type is a list or a map, i.e. a builder field with a default.
pub(super) fn is_list_or_map(types: &RustTypes, name: &str) -> bool {
    matches!(types.get(name), Some(rust::Type::List(_) | rust::Type::Map(_)))
}

fn is_struct(types: &RustTypes, name: &str) -> bool {
    matches!(types.get(name), Some(rust::Type::Struct(_)))
}

/// A stable, platform independent hash of a seed string.
fn hash(seed: &str) -> usize {
    let mut state = 0xcbf2_9ce4_8422_2325_u64;
    for byte in seed.as_bytes() {
        state ^= u64::from(*byte);
        state = state.wrapping_mul(0x0000_0100_0000_01b3);
    }
    usize::try_from(state % 1_000_003).unwrap()
}

fn number_literal(cases: &[&str], seed: &str) -> String {
    cases[hash(seed) % cases.len()].to_owned()
}

/// A string leaf seeded by the field name, so sibling fields hold distinct values.
fn alias_val(ty: &rust::Alias, seed: &str) -> Val {
    match ty.type_.as_str() {
        "String" => {
            let text = format!("{seed}-a<&>\"中");
            Val::Leaf {
                expr: format!("String::from({text:?})"),
                debug: format!("{text:?}"),
            }
        }
        "bool" => {
            let value = hash(seed).is_multiple_of(2);
            Val::Leaf {
                expr: value.to_string(),
                debug: value.to_string(),
            }
        }
        "i32" => Val::Leaf {
            expr: number_literal(&["i32::MIN", "-1", "0", "1", "i32::MAX"], seed),
            debug: number_literal(&["-2147483648", "-1", "0", "1", "2147483647"], seed),
        },
        "i64" => Val::Leaf {
            expr: number_literal(&["i64::MIN", "-1", "0", "1", "i64::MAX"], seed),
            debug: number_literal(&["-9223372036854775808", "-1", "0", "1", "9223372036854775807"], seed),
        },
        "ETagCondition" => Val::Opaque("s3s::dto::ETagCondition::Any".to_owned()),
        other => panic!("unsupported alias target: {other}"),
    }
}

/// A string enum leaf: the variant is picked by the seed so sibling fields differ.
fn str_enum_val(ty: &rust::StrEnum, seed: &str) -> Val {
    let variant = &ty.variants[hash(seed) % ty.variants.len()];
    let value = &variant.value;
    Val::Leaf {
        expr: format!("s3s::dto::{}::from_static({value:?})", ty.name),
        debug: format!("{}({value:?})", ty.name),
    }
}

/// A leaf of a type that the model does not describe.
fn special_val(name: &str, seed: &str) -> Val {
    match name {
        "String" => {
            let text = format!("{seed}-a<&>\"中");
            Val::Leaf {
                expr: format!("String::from({text:?})"),
                debug: format!("{text:?}"),
            }
        }
        "bool" => Val::Leaf {
            expr: "true".to_owned(),
            debug: "true".to_owned(),
        },
        "i32" => Val::Leaf {
            expr: "i32::MAX".to_owned(),
            debug: "2147483647".to_owned(),
        },
        "i64" => Val::Leaf {
            expr: "i64::MIN".to_owned(),
            debug: "-9223372036854775808".to_owned(),
        },
        "ETagCondition" => Val::Opaque("s3s::dto::ETagCondition::Any".to_owned()),
        "PostPolicy" => Val::Opaque(
            "s3s::post_policy::PostPolicy { expiration: s3s::dto::Timestamp::default(), conditions: vec![] }".to_owned(),
        ),
        "SelectObjectContentEventStream" => {
            Val::Opaque("s3s::dto::SelectObjectContentEventStream::new(futures::stream::empty())".to_owned())
        }
        other => panic!("no sample value for the unknown type: {other}"),
    }
}

/// A leaf of a type provided by the crate itself.
fn provided_val(name: &str) -> Val {
    match name {
        "Body" => Val::Opaque("s3s::dto::Body::from_static(b\"body\")".to_owned()),
        "StreamingBlob" => Val::Opaque("s3s::dto::StreamingBlob::from_bytes(s3s::dto::Body::from_static(b\"blob\"))".to_owned()),
        "CopySource" => Val::Opaque(
            "s3s::dto::CopySource::Bucket { bucket: \"bucket\".into(), key: \"key\".into(), version_id: Some(\"v1\".into()) }"
                .to_owned(),
        ),
        "Range" => Val::Opaque("s3s::dto::Range::Int { first: 0, last: Some(4) }".to_owned()),
        "ContentType" => Val::Leaf {
            expr: "String::from(\"text/plain\")".to_owned(),
            debug: "\"text/plain\"".to_owned(),
        },
        "Event" => Val::Opaque("s3s::dto::Event::from(String::from(\"s3:ObjectCreated:Put\"))".to_owned()),
        "CachedTags" => Val::Opaque("s3s::dto::CachedTags::default()".to_owned()),
        "ETag" => Val::Opaque("s3s::dto::ETag::Strong(String::from(\"etag\"))".to_owned()),
        other => panic!("no sample value for the provided type: {other}"),
    }
}

/// A sample value of the named type; struct-typed fields become calls to the
/// sample function of their type, so a sample function stays one level deep.
fn value_of(types: &RustTypes, name: &str, mode: Mode, seed: &str) -> Val {
    if is_struct(types, name) {
        return Val::Nested {
            name: name.to_owned(),
            mode,
        };
    }
    spec(types, name, mode, seed)
}

fn field_val(types: &RustTypes, field: &rust::StructField, mode: Mode) -> FieldVal {
    let name = field.name.clone();
    let optional = field.option_type;
    let blankable = optional && is_blankable(types, &field.type_);

    let val = if let Some(custom) = &field.custom_in_derive_debug {
        // The implementation prints a literal string, e.g. a future. The text
        // between the quotes of the literal is what its Debug output holds.
        let literal = custom.strip_prefix('&').unwrap_or(custom);
        let text = literal.strip_prefix('"').and_then(|x| x.strip_suffix('"')).unwrap_or(literal);
        Some(Val::Leaf {
            expr: "Box::pin(async { Ok::<_, s3s::S3Error>(s3s::dto::CompleteMultipartUploadOutput::default()) })".to_owned(),
            debug: format!("{text:?}"),
        })
    } else if blankable && mode == Mode::Blank {
        Some(blank_val(types, &field.type_))
    } else if blankable && mode == Mode::Clean {
        // A nested value keeps its sample: the implementation only clears the
        // empty strings of a value, so a sample with non-empty strings survives
        // the recursive call.
        None
    } else {
        Some(value_of(types, &field.type_, Mode::Full, &name))
    };

    FieldVal { name, optional, val }
}

/// The empty-string version of a blankable field type.
fn blank_val(types: &RustTypes, name: &str) -> Val {
    match types.get(name) {
        Some(rust::Type::Alias(ty)) if ty.type_ == "String" => Val::Leaf {
            expr: "String::new()".to_owned(),
            debug: "\"\"".to_owned(),
        },
        Some(rust::Type::StrEnum(ty)) => Val::Leaf {
            expr: format!("s3s::dto::{}::from_static(\"\")", ty.name),
            debug: format!("{}(\"\")", ty.name),
        },
        _ => panic!("type is not blankable: {name}"),
    }
}

/// The sample value of a type name in the given mode.
pub(super) fn spec(types: &RustTypes, name: &str, mode: Mode, seed: &str) -> Val {
    match types.get(name) {
        Some(rust::Type::Struct(ty)) => Val::Struct {
            name: ty.name.clone(),
            fields: ty.fields.iter().map(|field| field_val(types, field, mode)).collect(),
        },
        Some(rust::Type::StrEnum(ty)) => str_enum_val(ty, seed),
        Some(rust::Type::StructEnum(ty)) => {
            let variant = &ty.variants[0];
            Val::Variant {
                name: ty.name.clone(),
                variant: variant.name.clone(),
                inner: Box::new(value_of(types, &variant.type_, Mode::Full, &variant.name)),
            }
        }
        Some(rust::Type::Timestamp(_)) => Val::Opaque(TIMESTAMP.to_owned()),
        Some(rust::Type::Alias(ty)) => alias_val(ty, seed),
        Some(rust::Type::List(ty)) => Val::List(vec![value_of(types, &ty.member.type_, Mode::Full, name)]),
        Some(rust::Type::Map(ty)) => Val::Map(vec![(
            value_of(types, &ty.key_type, Mode::Full, "key"),
            value_of(types, &ty.value_type, Mode::Full, "value"),
        )]),
        Some(rust::Type::Provided(ty)) => provided_val(&ty.name),
        None => special_val(name, seed),
    }
}

/// The Rust expression building the sample value.
pub(super) fn val_expr(val: &Val) -> String {
    match val {
        Val::Leaf { expr, .. } | Val::Opaque(expr) => expr.clone(),
        Val::Struct { name, fields } => {
            let mut out = format!("s3s::dto::{name} {{");
            for field in fields {
                match &field.val {
                    None => {
                        let _ = write!(out, " {}: None,", field.name);
                    }
                    Some(inner) => {
                        let expr = val_expr(inner);
                        if field.optional {
                            let _ = write!(out, " {}: Some({expr}),", field.name);
                        } else {
                            let _ = write!(out, " {}: {expr},", field.name);
                        }
                    }
                }
            }
            out.push_str(" }");
            out
        }
        Val::Nested { name, mode } => match mode {
            Mode::Full => format!("super::debug::{}()", sample_fn(name)),
            Mode::Blank | Mode::Clean => panic!("nested sample mode {mode:?} is not emitted: {name}"),
        },
        Val::Variant { name, variant, inner } => format!("s3s::dto::{name}::{variant}({})", val_expr(inner)),
        Val::List(items) => {
            let items: Vec<String> = items.iter().map(val_expr).collect();
            format!("vec![{}]", items.join(", "))
        }
        Val::Map(pairs) => {
            let pairs: Vec<String> = pairs
                .iter()
                .map(|(key, value)| format!("({}, {})", val_expr(key), val_expr(value)))
                .collect();
            format!("std::collections::HashMap::from([{}])", pairs.join(", "))
        }
    }
}

fn quote(text: &str) -> String {
    format!("{text:?}")
}

fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '{' => out.push_str("{{"),
            '}' => out.push_str("}}"),
            other => out.push(other),
        }
    }
    out
}

fn compose(parts: Vec<Part>) -> Frag {
    let literal = parts.iter().all(|part| match part {
        Part::Text(_) => true,
        Part::Child(Frag::Lit(_)) => true,
        Part::Child(Frag::Expr(_)) => false,
    });
    if literal {
        let mut text = String::new();
        for part in parts {
            match part {
                Part::Text(part) | Part::Child(Frag::Lit(part)) => text.push_str(&part),
                Part::Child(Frag::Expr(_)) => unreachable!(),
            }
        }
        return Frag::Lit(text);
    }

    let mut template = String::new();
    let mut args: Vec<String> = Vec::new();
    for part in parts {
        match part {
            Part::Text(part) | Part::Child(Frag::Lit(part)) => template.push_str(&escape(&part)),
            Part::Child(Frag::Expr(expr)) => {
                let _ = write!(template, "{{{}}}", args.len());
                args.push(expr);
            }
        }
    }
    Frag::Expr(format!("format!({}, {})", quote(&template), args.join(", ")))
}

fn child(text: String) -> Part {
    Part::Text(text)
}

/// The expected Debug text of a sample value: an exact literal when every leaf
/// is known, otherwise a format! expression that renders the opaque leaves at
/// run time.
pub(super) fn expected_expr(types: &RustTypes, val: &Val) -> String {
    match debug_frag(types, val) {
        Frag::Lit(text) => quote(&text),
        Frag::Expr(expr) => expr,
    }
}

fn debug_frag(types: &RustTypes, val: &Val) -> Frag {
    match val {
        Val::Leaf { debug, .. } => Frag::Lit(debug.clone()),
        Val::Opaque(expr) => Frag::Expr(format!("format!(\"{{:?}}\", {expr})")),
        Val::Struct { name, fields } => {
            let mut parts = vec![child(format!("{name} {{ "))];
            let mut printed = 0_usize;
            for field in fields {
                let Some(inner) = &field.val else { continue };
                if printed > 0 {
                    parts.push(child(", ".to_owned()));
                }
                printed += 1;
                parts.push(child(format!("{}: ", field.name)));
                parts.push(Part::Child(debug_frag(types, inner)));
            }
            if printed == 0 {
                parts.push(child(".. }".to_owned()));
            } else {
                parts.push(child(", .. }".to_owned()));
            }
            compose(parts)
        }
        Val::Nested { name, mode } => debug_frag(types, &spec(types, name, *mode, name)),
        Val::Variant { variant, inner, .. } => {
            let parts = vec![
                child(format!("{variant}(")),
                Part::Child(debug_frag(types, inner)),
                child(")".to_owned()),
            ];
            compose(parts)
        }
        Val::List(items) => {
            let mut parts = vec![child("[".to_owned())];
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    parts.push(child(", ".to_owned()));
                }
                parts.push(Part::Child(debug_frag(types, item)));
            }
            parts.push(child("]".to_owned()));
            compose(parts)
        }
        Val::Map(pairs) => {
            let mut parts = vec![child("{".to_owned())];
            for (index, (key, value)) in pairs.iter().enumerate() {
                if index > 0 {
                    parts.push(child(", ".to_owned()));
                }
                parts.push(Part::Child(debug_frag(types, key)));
                parts.push(child(": ".to_owned()));
                parts.push(Part::Child(debug_frag(types, value)));
            }
            parts.push(child("}".to_owned()));
            compose(parts)
        }
    }
}

pub(super) fn gate(minio: bool, item: &str) -> String {
    let cfg = if minio {
        "#[cfg(feature = \"minio\")]"
    } else {
        "#[cfg(not(feature = \"minio\"))]"
    };
    format!("{cfg}\n{item}")
}

/// Pushes the base and the union variant of one type, gating them when they differ.
pub(super) fn push_pair(out: &mut Vec<String>, base: Vec<String>, minio: Vec<String>) {
    if base.is_empty() && minio.is_empty() {
        return;
    }
    if base == minio {
        out.extend(base);
        return;
    }
    out.extend(base.into_iter().map(|item| gate(false, &item)));
    out.extend(minio.into_iter().map(|item| gate(true, &item)));
}

/// The generated items of one dto type: the sample function and the Debug case.
fn emit(types: &RustTypes, name: &str) -> Vec<String> {
    let Some(ty) = types.get(name) else { return Vec::new() };
    match ty {
        rust::Type::Struct(ty) => emit_struct(types, ty),
        rust::Type::StructEnum(ty) => emit_struct_enum(types, ty),
        _ => Vec::new(),
    }
}

fn emit_struct(types: &RustTypes, ty: &rust::Struct) -> Vec<String> {
    let full = spec(types, &ty.name, Mode::Full, &ty.name);
    let fn_name = sample_fn(&ty.name);
    let body = val_expr(&full);
    let sample = format!("pub(super) fn {fn_name}() -> s3s::dto::{} {{\n    {body}\n}}", ty.name);
    let expected = expected_expr(types, &full);
    let case = format!(
        "#[test]\nfn debug_struct_{snake}() {{\n    let value = {fn_name}();\n    assert_eq!(format!(\"{{value:?}}\"), {expected});\n}}",
        snake = ty.name.to_snake_case(),
    );
    vec![sample, case]
}

fn emit_struct_enum(types: &RustTypes, ty: &rust::StructEnum) -> Vec<String> {
    let mut cases = String::new();
    for variant in &ty.variants {
        let val = Val::Variant {
            name: ty.name.clone(),
            variant: variant.name.clone(),
            inner: Box::new(value_of(types, &variant.type_, Mode::Full, &variant.name)),
        };
        let expr = val_expr(&val);
        let expected = expected_expr(types, &val);
        let _ = writeln!(cases, "    assert_eq!(format!(\"{{:?}}\", {expr}), {expected});");
    }
    let case = format!("#[test]\nfn debug_struct_enum_{snake}() {{\n{cases}}}", snake = ty.name.to_snake_case());
    vec![case]
}

pub(super) fn codegen(ops: &Operations, rust_types_base: &RustTypes, rust_types_minio: &RustTypes) {
    let _ = ops;
    let mut items: Vec<String> = Vec::new();
    for name in rust_types_minio.keys() {
        let base = emit(rust_types_base, name);
        if is_minio_only(name, rust_types_base, rust_types_minio) {
            assert!(base.is_empty(), "a union-only type must not have a base case: {name}");
        }
        let minio = emit(rust_types_minio, name);
        push_pair(&mut items, base, minio);
    }

    assert!(!items.is_empty(), "the Debug family emitted no case");

    write_test_file("debug.rs", || {
        codegen_file_header(Some("debug"));
        g!("//! Debug: one fully populated sample value per dto type, with the");
        g!("//! expected output derived from the model data and asserted verbatim.");
        g!("//! Each sample_* function builds the values reused by the other families.");
        g!();
        g!("#![allow(clippy::too_many_lines)]");
        g!();
        for item in &items {
            g!("{}", item);
            g!();
        }
    });
}
