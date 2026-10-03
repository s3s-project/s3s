// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

//! The Debug smoke test family: one value per selected type, formatted once,
//! with the expected `Debug` output asserted verbatim.
//!
//! Two shapes are selected, because both can be constructed without a general
//! value generator while their `Debug` output stays fully determined:
//!
//! - a string enum built with `from_static`: the derived `Debug` prints the
//!   tuple struct as `Name("value")`;
//! - a struct whose fields are all optional, built with `Default`: its manual
//!   `Debug` impl skips every `None` field and ends with `Name { .. }`.
//!
//! A case is emitted only for a type that the base and the union (`minio`)
//! model describe identically: such a type is available under every feature
//! combination and has the same `Debug` output in both builds. Types that only
//! the union describes are emitted under `#[cfg(feature = "minio")]`, mirroring
//! the gate on the item in `s3s::dto::generated`.

use crate::v1::dto::RustTypes;
use crate::v1::ops::Operations;
use crate::v1::rust;

use super::{codegen_file_header, is_minio_only, write_test_file};

use heck::ToSnakeCase;
use scoped_writer::g;

/// Number of shared cases emitted per shape.
const SHARED_ENUM_CASES: usize = 6;
const SHARED_STRUCT_CASES: usize = 6;

/// Number of union-only (`minio`) cases emitted per shape.
const MINIO_ENUM_CASES: usize = 3;
const MINIO_STRUCT_CASES: usize = 3;

/// One generated case: how to build the value and what `Debug` must print.
enum Case {
    StrEnum { name: String, value: String, expected: String },
    StructDefault { name: String, expected: String },
}

pub(super) fn codegen(_ops: &Operations, rust_types_base: &RustTypes, rust_types_minio: &RustTypes) {
    let shared = select(
        rust_types_minio
            .iter()
            .filter(|(name, ty)| rust_types_base.get(name.as_str()) == Some(*ty)),
        SHARED_ENUM_CASES,
        SHARED_STRUCT_CASES,
    );
    let minio_only = select(
        rust_types_minio
            .iter()
            .filter(|(name, _)| is_minio_only(name, rust_types_base, rust_types_minio)),
        MINIO_ENUM_CASES,
        MINIO_STRUCT_CASES,
    );
    assert!(!shared.is_empty(), "no shared dto type selected for the Debug smoke family");

    write_test_file("debug.rs", || {
        codegen_file_header(Some("debug"));
        g!("//! Debug smoke: selected dto values formatted once, with the expected");
        g!("//! output asserted verbatim. The expected value is derived from the model");
        g!("//! data, so a wrong `Debug` implementation fails these tests.");
        g!();

        for case in &shared {
            emit_case(case);
            g!();
        }

        if !minio_only.is_empty() {
            g!("// A type that only the union (`minio`) model describes is gated with");
            g!("// the same feature as the item in `s3s::dto::generated`.");
            g!();
            for case in &minio_only {
                g!("#[cfg(feature = \"minio\")]");
                emit_case(case);
                g!();
            }
        }
    });
}

/// Selects cases from `types` in map order (type name), taking at most
/// `enum_limit` string enums and `struct_limit` all-optional structs.
fn select<'a>(types: impl Iterator<Item = (&'a String, &'a rust::Type)>, enum_limit: usize, struct_limit: usize) -> Vec<Case> {
    let mut cases = Vec::new();
    let mut enums = 0;
    let mut structs = 0;

    for (name, ty) in types {
        if enums == enum_limit && structs == struct_limit {
            break;
        }

        match ty {
            rust::Type::StrEnum(ty) if enums < enum_limit => {
                let Some(variant) = ty.variants.first() else { continue };
                enums += 1;
                let value = variant.value.clone();
                cases.push(Case::StrEnum {
                    name: name.clone(),
                    expected: format!("{name}({value:?})"),
                    value,
                });
            }
            rust::Type::Struct(ty) if structs < struct_limit && ty.fields.iter().all(|field| field.option_type) => {
                structs += 1;
                cases.push(Case::StructDefault {
                    name: name.clone(),
                    expected: format!("{name} {{ .. }}"),
                });
            }
            _ => {}
        }
    }

    cases
}

fn emit_case(case: &Case) {
    match case {
        Case::StrEnum { name, value, expected } => {
            g!("#[test]");
            g!("fn debug_str_enum_{}() {{", name.to_snake_case());
            g!("let value = s3s::dto::{name}::from_static({value:?});");
            g!("assert_eq!(format!(\"{{value:?}}\"), {expected:?});");
            g!("}}");
        }
        Case::StructDefault { name, expected } => {
            g!("#[test]");
            g!("fn debug_struct_default_{}() {{", name.to_snake_case());
            g!("let value = s3s::dto::{name}::default();");
            g!("assert_eq!(format!(\"{{value:?}}\"), {expected:?});");
            g!("}}");
        }
    }
}
