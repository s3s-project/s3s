// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

use super::ops::Operations;
use super::rust::codegen_doc;

use crate::declare_codegen;
use crate::v2::post_object;

use heck::ToSnakeCase;
use scoped_writer::g;

pub fn codegen(ops: &Operations) {
    declare_codegen!();

    g([
        "use crate::dto::*;",
        "use crate::error::S3Result;",
        "use crate::protocol::S3Request;",
        "use crate::protocol::S3Response;",
        "",
        "/// An async trait which represents the S3 API",
        "#[async_trait::async_trait]",
        "pub trait S3: Send + Sync + 'static {",
        "",
    ]);

    for op in ops.values() {
        let method_name = op.name.to_snake_case();
        let input = &op.input;
        let output = &op.output;

        // The synthetic operation's default bridge is owned by the v2 module.
        if post_object::is_synthetic(&op.name) {
            post_object::codegen_trait_default();
            continue;
        }

        codegen_doc(op.doc.as_deref());

        if op.is_minio {
            g!("#[cfg(feature = \"minio\")]");
        }
        g!("async fn {method_name}(&self, _req: S3Request<{input}>) -> S3Result<S3Response<{output}>> {{");
        g!("Err(s3_error!(NotImplemented, \"{} is not implemented yet\"))", op.name);
        g!("}}");
        g!();
    }

    g!("}}");
    g!();
}
