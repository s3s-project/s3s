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

    declare_codegen!();

    g([
        "#![allow(clippy::doc_markdown)]",
        "#![allow(clippy::too_many_lines)]",
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

    g!("impl S3ErrorCode {{");

    {
        g!("const STATIC_CODE_LIST: &'static [&'static str] = &[");
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

        g!("match s {{");
        for err in errors.values() {
            g!("b\"{}\" => Some(Self::{}),", err.code, err.code);
        }
        g!("_ => std::str::from_utf8(s).ok().map(|s| Self::Custom(s.into()))");
        g!("}}");

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

    g!("}}");
}
