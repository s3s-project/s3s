// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

use super::ops::Operations;

use crate::declare_codegen;

use heck::ToSnakeCase;
use scoped_writer::g;

pub fn codegen(ops: &Operations) {
    declare_codegen!();

    g([
        "#![allow(clippy::doc_markdown)]",
        "",
        "use super::S3AccessContext;",
        "",
        "use crate::dto::*;",
        "use crate::error::S3Result;",
        "use crate::protocol::S3Request;",
        "",
        "#[async_trait::async_trait]",
        "pub trait S3Access: Send + Sync + 'static {",
        "",
    ]);

    g([
        "/// Checks whether the current request has accesses to the resources.",
        "///",
        "/// This method is called before deserializing the operation input.",
        "///",
        "/// By default, this method rejects all anonymous requests",
        "/// and returns [`AccessDenied`](crate::S3ErrorCode::AccessDenied) error.",
        "///",
        "/// An access control provider can override this method to implement custom logic.",
        "///",
        "/// Common fields in the context:",
        "/// + [`cx.credentials()`](S3AccessContext::credentials)",
        "/// + [`cx.s3_path()`](S3AccessContext::s3_path)",
        "/// + [`cx.s3_op().name()`](crate::S3Operation::name)",
        "/// + [`cx.extensions_mut()`](S3AccessContext::extensions_mut)",
        "async fn check(&self, cx: &mut S3AccessContext<'_>) -> S3Result<()> {",
        "    super::default_check(cx)",
        "}",
    ]);

    for op in ops.values() {
        let method_name = op.name.to_snake_case();
        let input = &op.input;

        g!("/// Checks whether the {} request has accesses to the resources.", op.name);
        g!("/// ");
        g!("/// This method returns `Ok(())` by default.");
        g!("async fn {method_name}(&self, _req: &mut S3Request<{input}>) -> S3Result<()> {{");
        g!("Ok(())");
        g!("}}");
        g!();
    }

    g!("}}");
    g!();
}
