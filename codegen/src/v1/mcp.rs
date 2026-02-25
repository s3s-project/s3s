use super::dto::RustTypes;
use super::ops::Operations;
use super::rust;

use crate::declare_codegen;

use heck::ToSnakeCase;
use scoped_writer::g;
use serde_json::Value;

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
        _ => false,
    }
}

fn input_has_default(input_name: &str, rust_types: &RustTypes) -> bool {
    rust_types.get(input_name).is_some_and(|ty| match ty {
        rust::Type::Struct(s) => can_derive_default(s, rust_types),
        _ => false,
    })
}

pub fn codegen(ops: &Operations, rust_types: &RustTypes) {
    declare_codegen!();

    g([
        "#![allow(clippy::too_many_lines)]",
        "#![allow(clippy::needless_pass_by_value)]",
        "#![allow(clippy::wildcard_imports)]",
        "",
        "use std::sync::Arc;",
        "",
        "use s3s::S3;",
        "use s3s::dto::*;",
        "use s3s::S3Request;",
        "",
        "use rmcp::model::Tool;",
        "use rmcp::model::CallToolResult;",
        "use rmcp::model::Content;",
        "use rmcp::model::JsonObject;",
        "",
        "fn empty_object_schema() -> Arc<JsonObject> {",
        "    let mut map = JsonObject::new();",
        "    map.insert(\"type\".to_owned(), serde_json::Value::String(\"object\".to_owned()));",
        "    Arc::new(map)",
        "}",
        "",
        "fn new_request<T>(input: T) -> S3Request<T> {",
        "    S3Request {",
        "        input,",
        "        method: http::Method::GET,",
        "        uri: http::Uri::default(),",
        "        headers: http::HeaderMap::new(),",
        "        extensions: http::Extensions::new(),",
        "        credentials: None,",
        "        region: None,",
        "        service: None,",
        "        trailing_headers: None,",
        "    }",
        "}",
        "",
    ]);

    // Generate tool_definitions() function
    g!("pub fn tool_definitions() -> Vec<Tool> {{");
    g!("let schema = empty_object_schema();");
    g!("vec![");
    for op in ops.values() {
        if op.name == "PostObject" {
            continue;
        }
        let method_name = op.name.to_snake_case();
        let desc = op.doc.as_deref().unwrap_or(&op.name);
        let desc = clean_html(desc);
        let desc = truncate(&desc, 200);
        g!("Tool::new(\"{method_name}\", \"{desc}\", Arc::clone(&schema)),");
    }
    g!("]");
    g!("}}");
    g!();

    // Generate dispatch() function
    g!("pub async fn dispatch(s3: &dyn S3, name: &str) -> Result<CallToolResult, rmcp::ErrorData> {{");
    g!("match name {{");
    for op in ops.values() {
        if op.name == "PostObject" {
            continue;
        }
        let method_name = op.name.to_snake_case();
        let input = &op.input;
        let has_default = input_has_default(input, rust_types);
        g!("\"{method_name}\" => {{");
        if has_default {
            g!("let input = {input}::default();");
            g!("let req = new_request(input);");
            g!("match s3.{method_name}(req).await {{");
            g!("Ok(resp) => Ok(CallToolResult::success(vec![Content::text(format!(\"{{:#?}}\", resp.output))])),");
            g!("Err(e) => Ok(CallToolResult::error(vec![Content::text(format!(\"S3 error: {{e}}\"))])),");
            g!("}}");
        } else {
            g!(
                "Ok(CallToolResult::error(vec![Content::text(\"{} requires parameters that are not yet supported\".to_string())]))",
                op.name
            );
        }
        g!("}}");
    }
    g!("_ => Err(rmcp::ErrorData::invalid_params(format!(\"unknown tool: {{name}}\"), None)),");
    g!("}}");
    g!("}}");
}

fn clean_html(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let mut in_tag = false;
    for c in s.chars() {
        if c == '<' {
            in_tag = true;
        } else if c == '>' {
            in_tag = false;
        } else if !in_tag {
            result.push(c);
        }
    }
    // Collapse whitespace
    let parts: Vec<&str> = result.split_whitespace().collect();
    let joined = parts.join(" ");
    // Escape characters that would break string literals
    joined.replace('\\', "\\\\").replace('"', "\\\"")
}

fn truncate(s: &str, max_len: usize) -> String {
    if s.len() <= max_len {
        s.to_owned()
    } else {
        let truncated = &s[..max_len.saturating_sub(3)];
        format!("{truncated}...")
    }
}
