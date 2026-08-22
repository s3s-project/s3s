// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 Nugine

use super::smithy;
use super::utils::o;

use std::mem;
use std::ops::Not;

use heck::ToUpperCamelCase;

pub const NAMES: &[&str] = &[
    "AssumeRoleResponse",
    "AssumedRoleUser",
    "Credentials",
    "nonNegativeIntegerType",
    "sourceIdentityType",
    "arnType",
    "accessKeyIdType",
    "accessKeySecretType",
    "dateType",
    "tokenType",
    "assumedRoleIdType",
];

pub fn reduce(model: &mut smithy::Model) {
    for (shape_name, mut shape) in mem::take(&mut model.shapes) {
        let Some((_, name)) = shape_name.split_once('#') else { panic!() };
        if NAMES.contains(&name).not() {
            continue;
        }

        let Some((_, name)) = shape_name.split_once('#') else { panic!() };
        let new_name = match name {
            "AssumeRoleResponse" => o("AssumeRoleOutput"),
            _ if name.as_bytes()[0].is_ascii_lowercase() => name.to_upper_camel_case(),
            _ => o(name),
        };

        if let smithy::Shape::Structure(ref mut shape) = shape {
            for member in shape.members.values_mut() {
                let Some((_, name)) = member.target.split_once('#') else { panic!() };
                let new_name = match name {
                    _ if name.as_bytes()[0].is_ascii_lowercase() => name.to_upper_camel_case(),
                    _ => continue,
                };
                member.target = member.target.replace(name, &new_name);
            }
            if name == "AssumeRoleResponse" {
                shape.traits.set("smithy.api#xmlName", name.into());
            }
        }

        let new_shape_name = format!("com.amazonaws.s3#{new_name}");
        assert!(model.shapes.insert(new_shape_name, shape).is_none());
    }
}
