// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

use super::o;
use super::smithy;

use crate::declare_codegen;

use std::collections::BTreeMap;
use std::ops::Not;

use heck::ToShoutySnakeCase;
use regex::Regex;
use s3s_model::error_codes;
use scoped_writer::g;
use stdx::default::default;

#[derive(Debug)]
struct Error {
    code: String,
    description: Vec<Option<String>>,
    status: Vec<Option<String>>,
}

type Errors = BTreeMap<String, Error>;

fn collect_errors(model: &smithy::Model) -> Errors {
    let error_code_doc = {
        let smithy::Shape::Structure(shape) = &model.shapes["com.amazonaws.s3#Error"] else { panic!() };
        shape.members["Code"].traits.doc().unwrap()
    };

    let pattern = Regex::new(r"<i>(.+?)</i> (.+)").unwrap();
    let code_pattern = Regex::new(r"<i>(.+?)</i> (.+?)</p>").unwrap();

    let mut errors: BTreeMap<String, Error> = default();

    let mut iter = error_code_doc.lines().map(str::trim);
    while let Some(line) = iter.next() {
        let code = {
            let Some(cap) = pattern.captures(line) else { continue };
            let tag = cap.get(1).unwrap().as_str();
            assert_eq!(tag, "Code:");
            o(code_pattern.captures(line).unwrap().get(2).unwrap().as_str().trim())
        };

        let description = loop {
            let Some(line) = iter.next() else { continue };
            let Some(cap) = pattern.captures(line) else { continue };
            let tag = cap.get(1).unwrap().as_str();
            if tag != "Description:" {
                break None;
            }
            let mut desc = String::new();
            let mut content = cap.get(2).unwrap().as_str();
            loop {
                match content.strip_suffix("</p>") {
                    Some(t) => {
                        if desc.is_empty().not() {
                            desc.push(' ');
                        }
                        desc.push_str(t);
                        break;
                    }
                    None => {
                        if desc.is_empty().not() {
                            desc.push(' ');
                        }
                        desc.push_str(content);
                        content = iter.next().unwrap();
                    }
                }
            }
            break Some(desc);
        };

        let status = loop {
            let Some(line) = iter.next() else { continue };

            if line.starts_with("<i>HTTP Status Code:</i> N/A") {
                break None;
            }

            if line.starts_with("<i>Code:</i> 409 Conflict") {
                break Some(o("409 Conflict"));
            }

            let Some(cap) = pattern.captures(line) else { continue };
            let tag = cap.get(1).unwrap().as_str();
            assert_eq!(tag, "HTTP Status Code:", "{line:?}");

            let mut status = String::new();
            let mut content = cap.get(2).unwrap().as_str();
            loop {
                match content.strip_suffix("</p>") {
                    Some(t) => {
                        status.push_str(t);
                        break;
                    }
                    None => {
                        status.push_str(content);
                        content = iter.next().unwrap();
                    }
                }
            }
            break Some(status);
        };

        let _ = loop {
            let Some(line) = iter.next() else { continue };
            let Some(cap) = pattern.captures(line) else { continue };
            break cap;
        };

        let err = errors.entry(code.clone()).or_insert_with(|| Error {
            code,
            description: default(),
            status: default(),
        });
        err.description.push(description);
        err.status.push(status);
    }

    patch_extra_errors(&mut errors);

    errors
}

// The AWS official error-code table (`data/s3_error_codes.json`) carries the current
// description text for every code. Descriptions from the Smithy model docs may contain
// stale wording or HTML remnants, so the JSON table takes precedence for the default
// message table (the enum doc comments keep the model-derived text unchanged).
fn load_json_desc() -> BTreeMap<String, String> {
    let extra = error_codes::load_json("data/s3_error_codes.json").unwrap();
    let mut json_desc: BTreeMap<String, String> = BTreeMap::new();
    for group in extra.values() {
        for ec in group {
            json_desc.entry(ec.code.clone()).or_insert_with(|| ec.description.clone());
        }
    }
    json_desc
}

// Curated messages for codes whose official description is misleading, context-specific
// or otherwise unsuitable as an error message. Entries are asserted non-empty at codegen
// time so a typo cannot silently produce an empty `<Message>` element.
const OVERRIDE_MESSAGES: &[(&str, &str)] = &[
    ("IllegalLocationConstraintException", "The specified location constraint is not valid."),
    ("InvalidArgument", "Invalid argument."),
    ("InvalidRequest", "Invalid request."),
    ("MissingAuthenticationToken", "The request was not signed."),
];

fn load_overrides() -> BTreeMap<String, String> {
    OVERRIDE_MESSAGES
        .iter()
        .map(|(code, msg)| {
            assert!(!msg.trim().is_empty(), "empty override message for {code}");
            ((*code).to_string(), (*msg).to_string())
        })
        .collect()
}

// Collapses all whitespace runs (including U+00C2, a mojibake of NBSP whose UTF-8 lead
// byte was decoded as Latin-1) into a single space and trims the edges. Error messages
// are emitted as single-line text: multi-line documentation text is not a message, and
// Display/log consumers process messages line-by-line.
fn normalize_message(desc: &str) -> String {
    let mut out = String::with_capacity(desc.len());
    let mut pending_space = false;
    for c in desc.chars() {
        if c.is_whitespace() || c == '\u{c2}' {
            if !out.is_empty() {
                pending_space = true;
            }
        } else if pending_space {
            out.push(' ');
            out.push(c);
            pending_space = false;
        } else {
            out.push(c);
        }
    }
    out
}

const NEUTRAL_DEFAULT_MESSAGE: &str = "The request failed.";

fn resolve_default_message(
    err: &Error,
    json_desc: &BTreeMap<String, String>,
    overrides: &BTreeMap<String, String>,
) -> Option<String> {
    if let Some(msg) = overrides.get(&err.code) {
        return Some(normalize_message(msg));
    }
    if let Some(msg) = json_desc.get(&err.code) {
        return Some(normalize_message(msg));
    }
    if let Some(Some(desc)) = err.description.first() {
        return Some(normalize_message(desc));
    }
    None
}

// https://github.com/Nugine/s3s/issues/224
fn patch_extra_errors(errors: &mut Errors) {
    {
        let extra = error_codes::load_json("data/s3_error_codes.json").unwrap();

        for group in extra.values() {
            for ec in group {
                if errors.contains_key(&ec.code) {
                    continue;
                }
                if ec.code == "503 SlowDown" {
                    continue;
                }
                if ec.code.contains('.') {
                    continue;
                }

                errors.insert(
                    ec.code.clone(),
                    Error {
                        code: ec.code.clone(),
                        description: vec![Some(ec.description.clone())],
                        status: vec![ec.http_status_code.map(|s| {
                            let status = http::StatusCode::from_u16(s).unwrap();
                            let reason = status.canonical_reason().unwrap();
                            format!("{s} {reason}")
                        })],
                    },
                );
            }
        }
    }
}

#[allow(clippy::too_many_lines)]
pub fn codegen(model: &smithy::Model) {
    let errors = collect_errors(model);

    let json_desc = load_json_desc();
    let overrides = load_overrides();

    declare_codegen!();

    g([
        "#![allow(clippy::doc_markdown)]",
        "#![allow(clippy::too_many_lines)]",
        "#![allow(clippy::unreadable_literal)]",
        "",
        "use bytestring::ByteString;",
        "use hyper::StatusCode;",
        "",
    ]);

    for err in errors.values() {
        g!("// {}", err.code);
    }
    g!();

    g!("#[derive(Debug, Clone, PartialEq, Eq)]");
    g!("#[non_exhaustive]");
    g!("pub enum S3ErrorCode {{");
    for err in errors.values() {
        if err.description.len() > 1 {
            assert_eq!(err.code, "InvalidRequest");
            for status in &err.status {
                assert_eq!(status.as_ref().unwrap(), "400 Bad Request");
            }
            for desc in &err.description {
                g!("/// + {}", desc.as_ref().unwrap());
            }
            g!("///");
            g!("/// HTTP Status Code: 400 Bad Request");
        } else {
            let desc = &err.description[0];
            let status = &err.status[0];

            if let Some(desc) = desc {
                for line in desc.lines() {
                    g!("/// {}", line);
                }
            }
            if let Some(status) = status {
                if desc.is_some() {
                    g!("///");
                }
                g!("/// HTTP Status Code: {status}");
            }
            if desc.is_some() || status.is_some() {
                g!("///");
            }
        }

        g!("{},", err.code);
        g!();
    }
    g!("Custom(ByteString),");
    g!("}}");
    g!();

    {
        let mut exact_code_map = phf_codegen::Map::new();
        let mut lowercase_code_map = phf_codegen::Map::new();
        let mut lowercased = BTreeMap::new();
        let mut lowercase_codes = Vec::new();

        for err in errors.values() {
            let code = err.code.as_str();
            exact_code_map.entry(code, format!("S3ErrorCode::{code}"));

            let lowercase = err.code.to_ascii_lowercase();
            if let Some(prev) = lowercased.insert(lowercase.clone(), code) {
                panic!("{prev} and {code} collide after ASCII lowercasing as {lowercase}");
            }
            lowercase_codes.push((lowercase, code));
        }

        for (lowercase, code) in &lowercase_codes {
            lowercase_code_map.entry(lowercase.as_str(), format!("S3ErrorCode::{code}"));
        }

        g!(
            "static S3_ERROR_CODE_MAP: phf::Map<&'static str, S3ErrorCode> = {};",
            exact_code_map.build()
        );
        g!();
        g!(
            "static S3_ERROR_CODE_LOWERCASE_MAP: phf::Map<&'static str, S3ErrorCode> = {};",
            lowercase_code_map.build()
        );
        g!();
    }

    {
        let mut msg_map = phf_codegen::Map::new();
        for err in errors.values() {
            let msg = resolve_default_message(err, &json_desc, &overrides)
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| NEUTRAL_DEFAULT_MESSAGE.to_string());
            msg_map.entry(err.code.as_str(), format!("{msg:?}"));
        }

        g!(
            "static S3_ERROR_CODE_DEFAULT_MESSAGE: phf::Map<&'static str, &'static str> = {};",
            msg_map.build()
        );
        g!();
    }

    g!("impl S3ErrorCode {{");

    {
        g!("pub(super) const STATIC_CODE_LIST: &'static [&'static str] = &[");
        for err in errors.values() {
            g!("\"{}\",", err.code);
        }
        g!("];");
        g!();

        g!("#[must_use]");
        g!("fn as_enum_tag(&self) -> usize {{");
        g!("match self {{");
        for (idx, err) in errors.values().enumerate() {
            g!("Self::{} => {},", err.code, idx);
        }
        g!("Self::Custom(_) => usize::MAX,");
        g!("}}");
        g!("}}");
        g!();

        g([
            "pub(crate) fn as_static_str(&self) -> Option<&'static str> {",
            "    Self::STATIC_CODE_LIST.get(self.as_enum_tag()).copied()",
            "}",
        ]);
        g!();
    }

    {
        g!("#[must_use]");
        g!("pub fn from_bytes(s: &[u8]) -> Option<Self> {{");
        g!("let s = std::str::from_utf8(s).ok()?;");
        g!("if let Some(code) = S3_ERROR_CODE_MAP.get(s) {{");
        g!("return Some(code.clone());");
        g!("}}");
        g!("let lowercase = s.to_ascii_lowercase();");
        g!("if let Some(code) = S3_ERROR_CODE_LOWERCASE_MAP.get(lowercase.as_str()) {{");
        g!("return Some(code.clone());");
        g!("}}");
        g!("Some(Self::Custom(s.into()))");
        g!("}}");
        g!();
    }

    {
        g!("#[allow(clippy::match_same_arms)]");
        g!("#[must_use]");
        g!("pub fn status_code(&self) -> Option<StatusCode> {{");

        g!("match self {{");
        for err in errors.values() {
            if err.status.len() > 1 {
                for status in &err.status {
                    assert_eq!(status.as_ref().unwrap(), "400 Bad Request");
                }
                g!("Self::{} => Some(StatusCode::BAD_REQUEST),", err.code);
                continue;
            }
            if let Some(Some(status)) = err.status.first() {
                let status_name = match &status[4..] {
                    "Moved Temporarily" => {
                        assert!(status.starts_with("307"));
                        o("TEMPORARY_REDIRECT")
                    }
                    "Requested Range NotSatisfiable" => {
                        assert!(status.starts_with("416"));
                        o("RANGE_NOT_SATISFIABLE")
                    }
                    "Slow Down" => {
                        assert!(status.starts_with("503"));
                        o("SERVICE_UNAVAILABLE")
                    }
                    x => x.to_shouty_snake_case(),
                };

                g!("Self::{} => Some(StatusCode::{}),", err.code, status_name);
                continue;
            }
            g!("Self::{} => None,", err.code);
        }
        g!("Self::Custom(_) => None,");
        g!("}}");

        g!("}}");
        g!();
    }

    {
        g!("#[must_use]");
        g!("pub fn default_message(&self) -> Option<&'static str> {{");
        g!("let s = self.as_static_str()?;");
        g!("S3_ERROR_CODE_DEFAULT_MESSAGE.get(s).copied()");
        g!("}}");
        g!();
    }

    g!("}}");
}
