// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

use super::smithy;

use crate::declare_codegen;

use std::collections::BTreeSet;

use heck::ToShoutySnakeCase;
use scoped_writer::g;
use stdx::default::default;

pub fn codegen(model: &smithy::Model) {
    let mut headers: BTreeSet<&str> = default();

    for (name, shape) in &model.shapes {
        if name.ends_with("Request") || name.ends_with("Output") {
            let smithy::Shape::Structure(sh) = shape else { panic!() };

            for member in sh.members.values() {
                if let Some(header) = member.traits.http_header() {
                    headers.insert(header);
                }
            }
        }
    }

    {
        headers.insert("x-amz-content-sha256");
        headers.insert("x-amz-date");
        headers.insert("authorization");
        headers.insert("host");
        headers.insert("x-amz-decoded-content-length");
        headers.insert("x-amz-request-id");
        headers.insert("x-amz-id-2");
    }

    declare_codegen!();

    g([
        "#![allow(clippy::declare_interior_mutable_const)]",
        "",
        "use hyper::header::HeaderName;",
        "",
    ]);

    for header in headers {
        let name = to_constant_name(header);
        if header.starts_with("x-minio") {
            g!("#[cfg(feature = \"minio\")]");
        }
        if header.starts_with("x-amz-") || header == "Content-MD5" || header.starts_with("x-minio") {
            let value = header.to_ascii_lowercase();
            g!("pub const {name}: HeaderName = HeaderName::from_static({value:?});",);
        } else {
            g!("pub use hyper::header::{name};");
        }
        g!();
    }
}

pub fn to_constant_name(header_name: &str) -> String {
    if header_name == "ETag" {
        "ETAG".into()
    } else {
        header_name.to_shouty_snake_case()
    }
}
