// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

use super::ops::Operations;
use super::rust::codegen_doc;

use crate::declare_codegen;

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

        if op.name == "PostObject" {
            g([
                "/// POST Object (multipart form upload)",
                "///",
                "/// This is a synthetic method separated from `PutObject` so implementations can distinguish",
                "/// POST vs PUT.",
                "///",
                "/// The default implementation is a migration scaffold, not a correct POST implementation: it",
                "/// converts the request into a [`S3::put_object`] call, and the shape it forwards has no place",
                "/// for the form (its fields, the file name and the policy), the framing facts (boundary, tail",
                "/// length) or the `success_action_*` response semantics.",
                "///",
                "/// It is kept so that an implementation which only implements [`S3::put_object`] keeps serving",
                "/// POST requests while it migrates; the scaffold may be removed in a future minor release.",
                "///",
                "/// An implementation that cares about POST semantics should implement this method explicitly",
                "/// instead of relying on the default.",
            ]);
            g!("async fn post_object(&self, req: S3Request<PostObjectInput>) -> S3Result<S3Response<PostObjectOutput>> {{");
            g!("let resp = self.put_object(req.map_input(crate::dto::post_object_input_into_put_object_input)).await?;");
            g!("Ok(resp.map_output(crate::dto::put_object_output_into_post_object_output))");
            g!("}}");
            g!();
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
