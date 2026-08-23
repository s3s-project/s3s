// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

//! Route micro-benchmarks.
//!
//! Ignored by default; run with:
//!
//! ```bash
//! cargo test -p s3s --release route_bench -- --ignored --nocapture
//! ```
//!
//! Methodology: per form, 1 warm-up round (10⁶ iters) then 5 measured rounds
//! of 2×10⁷ iterations; the smallest per-iteration mean is reported. Inputs
//! are pre-built and passed through [`std::hint::black_box`]. The first call
//! of each case is also asserted against the expected operation, guarding
//! against silent routing drift.

use crate::http::{OrderedQs, Request};
use crate::ops::generated::resolve_route;
use crate::path::S3Path;
use minstant::Instant;
use std::hint::black_box;

struct Case {
    name: &'static str,
    method: &'static str,
    path: S3Path,
    qs: Vec<(&'static str, &'static str)>,
    expect_op: &'static str,
}

fn make_request(method: &str) -> Request {
    let method: hyper::Method = method.parse().unwrap();
    Request::from(
        hyper::Request::builder()
            .method(method)
            .uri("http://localhost/bucket")
            .body(crate::http::Body::empty())
            .unwrap(),
    )
}

#[allow(clippy::cast_precision_loss)]
fn time_ns_per_op(f: &mut impl FnMut(), iters: u64) -> f64 {
    for _ in 0..1_000_000 {
        f();
    }
    let mut best = f64::INFINITY;
    for _ in 0..5 {
        let start = Instant::now();
        for _ in 0..iters {
            f();
        }
        let ns = start.elapsed().as_secs_f64() / iters as f64 * 1e9;
        best = best.min(ns);
    }
    best
}

fn cases() -> Vec<Case> {
    vec![
        Case {
            name: "GET Bucket (no qs) -> ListObjects",
            method: "GET",
            path: S3Path::Bucket { bucket: "bucket".into() },
            qs: vec![],
            expect_op: "ListObjects",
        },
        Case {
            name: "GET Bucket versionId -> ListObjects",
            method: "GET",
            path: S3Path::Bucket { bucket: "bucket".into() },
            qs: vec![("versionId", "abc")],
            expect_op: "ListObjects",
        },
        Case {
            name: "GET Bucket list-type=2 -> ListObjectsV2",
            method: "GET",
            path: S3Path::Bucket { bucket: "bucket".into() },
            qs: vec![("list-type", "2")],
            expect_op: "ListObjectsV2",
        },
        Case {
            name: "GET Bucket analytics -> ListBucketAnalyticsConfigurations",
            method: "GET",
            path: S3Path::Bucket { bucket: "bucket".into() },
            qs: vec![("analytics", "")],
            expect_op: "ListBucketAnalyticsConfigurations",
        },
        Case {
            name: "GET Bucket analytics&id -> GetBucketAnalyticsConfiguration",
            method: "GET",
            path: S3Path::Bucket { bucket: "bucket".into() },
            qs: vec![("analytics", ""), ("id", "1")],
            expect_op: "GetBucketAnalyticsConfiguration",
        },
        Case {
            name: "GET Bucket acl -> GetBucketAcl",
            method: "GET",
            path: S3Path::Bucket { bucket: "bucket".into() },
            qs: vec![("acl", "")],
            expect_op: "GetBucketAcl",
        },
        Case {
            name: "PUT Object (no qs) -> PutObject",
            method: "PUT",
            path: S3Path::Object {
                bucket: "bucket".into(),
                key: "key.txt".into(),
            },
            qs: vec![],
            expect_op: "PutObject",
        },
        Case {
            name: "PUT Object partNumber&uploadId -> UploadPart",
            method: "PUT",
            path: S3Path::Object {
                bucket: "bucket".into(),
                key: "key.txt".into(),
            },
            qs: vec![("partNumber", "1"), ("uploadId", "upload")],
            expect_op: "UploadPart",
        },
        Case {
            name: "GET Object partNumber&uploadId -> ListParts",
            method: "GET",
            path: S3Path::Object {
                bucket: "bucket".into(),
                key: "key.txt".into(),
            },
            qs: vec![("partNumber", "1"), ("uploadId", "upload")],
            expect_op: "ListParts",
        },
        Case {
            name: "HEAD Bucket -> HeadBucket",
            method: "HEAD",
            path: S3Path::Bucket { bucket: "bucket".into() },
            qs: vec![],
            expect_op: "HeadBucket",
        },
        Case {
            name: "GET Root -> ListBuckets",
            method: "GET",
            path: S3Path::Root,
            qs: vec![],
            expect_op: "ListBuckets",
        },
    ]
}

#[test]
#[ignore = "micro-benchmark; run with: cargo test -p s3s --release route_bench -- --ignored --nocapture"]
fn route_bench() {
    let iters = 20_000_000u64;
    println!("{:<52} {:>10}", "case", "ns/op");
    println!("{}", "-".repeat(63));

    for c in cases() {
        let req = make_request(c.method);
        let qs = OrderedQs::from_vec_unchecked(c.qs.iter().map(|(k, v)| ((*k).to_owned(), (*v).to_owned())).collect());
        let path = &c.path;

        // Sanity check: routing must match the expected operation.
        {
            let op = resolve_route(black_box(&req), black_box(path), Some(black_box(&qs))).unwrap();
            assert_eq!(op.name(), c.expect_op, "case `{}` routed to wrong op", c.name);
        }

        let mut run = || {
            let result = resolve_route(black_box(&req), black_box(path), Some(black_box(&qs)));
            black_box(result.ok());
        };

        let ns = time_ns_per_op(&mut run, iters);
        println!("{:<52} {:>10.1}", c.name, ns);
    }
}
