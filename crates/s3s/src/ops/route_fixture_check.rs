// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

//! Equivalence fixtures for [`crate::ops::generated::resolve_route`].
//!
//! `route_fixtures.json` records, for each request sample, the operation the
//! router must return together with the operation's
//! [`Operation::needs_full_body`](super::Operation::needs_full_body) flag. The
//! data was captured from the current generated router and must keep passing
//! against any future router implementation.
//!
//! # Data format
//!
//! `route_fixtures.json` is a list of request samples. Each entry records one
//! routing decision: the inputs handed to `resolve_route` and the expected
//! answer (operation name plus `needs_full_body` flag).
//!
//! | field | meaning |
//! |---|---|
//! | `method` | HTTP method |
//! | `path` | path shape: `Root`, `Bucket` or `Object`; the checker substitutes fixed concrete values (`bucket`, `key.txt`) |
//! | `qs` | query pairs `[[k, v], ...]`; `[]` means the query string is present but empty, `null` means it is absent entirely. Pairs are stored as a list because duplicate keys are significant (see the `duplicate:` cases). The checker feeds them through [`OrderedQs`](crate::http::OrderedQs), which stable-sorts by key, so cross-key order carries no meaning and values must already be URL-decoded |
//! | `headers` | the request headers consulted by the router (e.g. `x-amz-copy-source`) |
//! | `expect` | expected operation name; `__NOT_IMPLEMENTED__` means the router must reject the request. This is a frozen answer, not a placeholder (placeholders use the `__CAPTURE__` prefix, see below) |
//! | `nfb` | expected `needs_full_body` flag |
//! | `note` | provenance label: which generated rule (or variant) the sample exercises |
//!
//! # Note conventions
//!
//! The `note` field is documentation only; the checker never parses it. It
//! mirrors the generated router's rule conditions:
//!
//! - `if:<cond>`: a minimal sample satisfying the rule `<cond>`
//! - `noise:<cond>`: the same sample plus a noise key (`prefix=zz`) that must not change the outcome
//! - `neg-hit:<cond>`: a sample that specifically takes the negated arm of `<cond>` (e.g. `qs.has("analytics") && !qs.has("id")`)
//! - `wrong-pattern:<key>`: a pattern rule fails because the value is wrong, so routing falls through
//! - `duplicate:<key>`: a pattern rule fails because the key appears more than once, so routing falls through
//! - `fallback-empty-qs`: no rule matched; the default operation of the (method, path) group is expected
//! - `tail:`: a tail rule of the generated chain (HEAD groups)
//! - `|null-qs` suffix: the same sample with the query string absent (`qs: null`)
//!
//! # Capture mode
//!
//! With `ROUTE_FIXTURE_CAPTURE=1` the test rewrites placeholder expectations
//! (`expect` prefixed with `__CAPTURE__`) with the current router's answer and
//! writes the file back. After an intentional router change, reset the affected
//! entries to `__CAPTURE__` placeholders and re-capture; all other entries —
//! including frozen rejects (`__NOT_IMPLEMENTED__`) — are left untouched and
//! skipped. A dedicated prefix keeps the two distinguishable, so capture never
//! silently rewrites frozen expectations.
//!
//! # Placement
//!
//! This module lives inside the crate because `resolve_route` and the request
//! types it takes are crate-private, so an integration test could not call
//! them. Same rationale as the `route_bench` module.

use crate::error::S3Result;
use crate::http::{OrderedQs, Request};
use crate::path::S3Path;
use std::sync::OnceLock;

const FIXTURES_JSON: &str = include_str!("route_fixtures.json");

struct Fixture {
    method: String,
    path: String,
    qs: Option<Vec<(String, String)>>,
    headers: std::collections::HashMap<String, String>,
    expect_op: String,
    expect_nfb: bool,
}

fn fixtures() -> &'static Vec<Fixture> {
    static CACHE: OnceLock<Vec<Fixture>> = OnceLock::new();
    CACHE.get_or_init(|| {
        #[derive(serde::Deserialize)]
        struct Raw {
            method: String,
            path: String,
            qs: Option<Vec<(String, String)>>,
            #[serde(default)]
            headers: std::collections::HashMap<String, String>,
            expect: String,
            nfb: bool,
        }
        let raw: Vec<Raw> = serde_json::from_str(FIXTURES_JSON).unwrap();
        raw.into_iter()
            .map(|r| Fixture {
                method: r.method,
                path: r.path,
                qs: r.qs,
                headers: r.headers,
                expect_op: r.expect,
                expect_nfb: r.nfb,
            })
            .collect()
    })
}

fn s3_path(path: &str) -> S3Path {
    match path {
        "Root" => S3Path::Root,
        "Bucket" => S3Path::Bucket { bucket: "bucket".into() },
        "Object" => S3Path::Object {
            bucket: "bucket".into(),
            key: "key.txt".into(),
        },
        other => panic!("unknown path kind in fixture: {other}"),
    }
}

fn make_request(method: &str, headers: &std::collections::HashMap<String, String>) -> Request {
    let mut builder = hyper::Request::builder()
        .method(method.parse::<hyper::Method>().unwrap())
        .uri("http://localhost/bucket");
    for (k, v) in headers {
        builder = builder.header(k, v);
    }
    Request::from(builder.body(crate::http::Body::empty()).unwrap())
}

fn resolve_current(fx: &Fixture) -> S3Result<&'static dyn super::Operation> {
    let req = make_request(&fx.method, &fx.headers);
    let path = s3_path(&fx.path);
    let qs = fx.qs.as_ref().map(|pairs| OrderedQs::from_vec_unchecked(pairs.clone()));
    crate::ops::generated::resolve_route(&req, &path, qs.as_ref())
}

#[test]
fn route_fixtures_match() {
    let capture = std::env::var("ROUTE_FIXTURE_CAPTURE").is_ok_and(|v| v == "1");
    let fixtures = fixtures();
    let mut raw: Option<Vec<serde_json::Value>> = capture.then(|| serde_json::from_str(FIXTURES_JSON).unwrap());
    let mut mismatches = Vec::new();

    for (idx, fx) in fixtures.iter().enumerate() {
        let (got_op, got_nfb) = match resolve_current(fx) {
            Ok(op) => (op.name().to_string(), op.needs_full_body()),
            Err(_) => ("__NOT_IMPLEMENTED__".to_string(), false),
        };

        if capture {
            if !fx.expect_op.starts_with("__CAPTURE__") {
                continue;
            }
            let entry = raw.as_mut().unwrap().get_mut(idx).unwrap();
            entry["expect"] = serde_json::json!(got_op);
            entry["nfb"] = serde_json::json!(got_nfb);
        } else {
            if got_op != fx.expect_op {
                mismatches.push(format!("{} {} {:?}: got {}, want {}", fx.method, fx.path, fx.qs, got_op, fx.expect_op));
            }
            if got_nfb != fx.expect_nfb {
                mismatches.push(format!(
                    "{} {} {:?}: needs_full_body got {}, want {}",
                    fx.method, fx.path, fx.qs, got_nfb, fx.expect_nfb
                ));
            }
        }
    }

    if capture {
        std::fs::write(
            concat!(env!("CARGO_MANIFEST_DIR"), "/src/ops/route_fixtures.json"),
            serde_json::to_string_pretty(raw.as_ref().unwrap()).unwrap(),
        )
        .unwrap();
        println!("captured {} fixtures", fixtures.len());
        return;
    }

    assert!(mismatches.is_empty(), "route fixture mismatches:\n{}", mismatches.join("\n"));
}
