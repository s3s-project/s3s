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

use super::CallContext;
use super::resolve_oir;
use crate::config::{S3Config, S3ConfigProvider, StaticConfigProvider};
use crate::http::{OrderedQs, QsLookup, Request};
use crate::ops::generated::resolve_operation_by_id;
use crate::ops::generated::resolve_route;
use crate::path::S3Path;
use crate::s3_trait::S3;
use minstant::Instant;
use std::hint::black_box;
use std::sync::Arc;

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

/// OIR (`x-id`) routing case.
struct OirCase {
    name: &'static str,
    method: &'static str,
    path: S3Path,
    /// Query pairs, including the `x-id` signal.
    qs: Vec<(&'static str, &'static str)>,
    expect_op: &'static str,
}

#[allow(clippy::too_many_lines)]
fn oir_cases() -> Vec<OirCase> {
    vec![
        OirCase {
            name: "FP GET Bucket (no qs) x-id=ListObjects",
            method: "GET",
            path: S3Path::Bucket { bucket: "bucket".into() },
            qs: vec![("x-id", "ListObjects")],
            expect_op: "ListObjects",
        },
        OirCase {
            name: "FP GET Bucket versionId x-id=ListObjects",
            method: "GET",
            path: S3Path::Bucket { bucket: "bucket".into() },
            qs: vec![("versionId", "abc"), ("x-id", "ListObjects")],
            expect_op: "ListObjects",
        },
        OirCase {
            name: "FP GET Bucket list-type=2 x-id=ListObjectsV2",
            method: "GET",
            path: S3Path::Bucket { bucket: "bucket".into() },
            qs: vec![("list-type", "2"), ("x-id", "ListObjectsV2")],
            expect_op: "ListObjectsV2",
        },
        OirCase {
            name: "FP GET Bucket analytics x-id=ListBucketAnalyticsConfigurations",
            method: "GET",
            path: S3Path::Bucket { bucket: "bucket".into() },
            qs: vec![("analytics", ""), ("x-id", "ListBucketAnalyticsConfigurations")],
            expect_op: "ListBucketAnalyticsConfigurations",
        },
        OirCase {
            name: "FP GET Bucket analytics&id x-id=GetBucketAnalyticsConfiguration",
            method: "GET",
            path: S3Path::Bucket { bucket: "bucket".into() },
            qs: vec![("analytics", ""), ("id", "1"), ("x-id", "GetBucketAnalyticsConfiguration")],
            expect_op: "GetBucketAnalyticsConfiguration",
        },
        OirCase {
            name: "FP GET Bucket acl x-id=GetBucketAcl",
            method: "GET",
            path: S3Path::Bucket { bucket: "bucket".into() },
            qs: vec![("acl", ""), ("x-id", "GetBucketAcl")],
            expect_op: "GetBucketAcl",
        },
        OirCase {
            name: "FP PUT Object x-id=PutObject",
            method: "PUT",
            path: S3Path::Object {
                bucket: "bucket".into(),
                key: "key.txt".into(),
            },
            qs: vec![("x-id", "PutObject")],
            expect_op: "PutObject",
        },
        OirCase {
            name: "FP PUT Object partNumber&uploadId x-id=UploadPart",
            method: "PUT",
            path: S3Path::Object {
                bucket: "bucket".into(),
                key: "key.txt".into(),
            },
            qs: vec![("partNumber", "1"), ("uploadId", "upload"), ("x-id", "UploadPart")],
            expect_op: "UploadPart",
        },
        OirCase {
            name: "FP GET Object x-id=GetObject",
            method: "GET",
            path: S3Path::Object {
                bucket: "bucket".into(),
                key: "key.txt".into(),
            },
            qs: vec![("x-id", "GetObject")],
            expect_op: "GetObject",
        },
        OirCase {
            name: "FP GET Object partNumber&uploadId x-id=ListParts",
            method: "GET",
            path: S3Path::Object {
                bucket: "bucket".into(),
                key: "key.txt".into(),
            },
            qs: vec![("partNumber", "1"), ("uploadId", "upload"), ("x-id", "ListParts")],
            expect_op: "ListParts",
        },
        OirCase {
            name: "FP GET Root x-id=ListBuckets",
            method: "GET",
            path: S3Path::Root,
            qs: vec![("x-id", "ListBuckets")],
            expect_op: "ListBuckets",
        },
        OirCase {
            name: "FP HEAD Bucket x-id=HeadBucket",
            method: "HEAD",
            path: S3Path::Bucket { bucket: "bucket".into() },
            qs: vec![("x-id", "HeadBucket")],
            expect_op: "HeadBucket",
        },
    ]
}

struct NoopS3;

#[async_trait::async_trait]
impl S3 for NoopS3 {}

struct CtxParts {
    s3: Arc<dyn S3>,
    config: Arc<dyn S3ConfigProvider>,
}

fn ctx() -> CtxParts {
    CtxParts {
        s3: Arc::new(NoopS3),
        config: Arc::new(StaticConfigProvider::default()),
    }
}

fn ccx(parts: &CtxParts) -> CallContext<'_> {
    CallContext {
        s3: &parts.s3,
        config: &parts.config,
        host: None,
        auth: None,
        access: None,
        route: None,
        validation: None,
    }
}

/// `S3Path` is not `Clone`; rebuild one for the request from a case.
fn rebuild_s3_path(p: &S3Path) -> S3Path {
    match p {
        S3Path::Root => S3Path::Root,
        S3Path::Bucket { bucket } => S3Path::Bucket { bucket: bucket.clone() },
        S3Path::Object { bucket, key } => S3Path::Object {
            bucket: bucket.clone(),
            key: key.clone(),
        },
    }
}

#[test]
#[ignore = "micro-benchmark; run with: cargo test -p s3s --release route_bench -- --ignored --nocapture"]
fn oir_bench() {
    let iters = 20_000_000u64;
    let parts = ctx();
    let ccx = ccx(&parts);
    // Fair, peer comparison: FVR shape routing and the OIR fast path are
    // measured on the *identical* request input (the qs includes the `x-id`
    // signal). Columns:
    // - FVR route: shape routing (single qs scan + feature probe + hit).
    // - OIR route: OIR signal consumption + dispatch (`lookup("x-id")` +
    //   `resolve_operation_by_id`) — no config snapshot, no `s3_path` prep;
    //   the peer of FVR route (both do one single-pass qs scan).
    // - OIR fast:  `resolve_oir` with a pre-fetched config (as in
    //   `prepare`, which snapshots once for the whole request). The shared
    //   config cost itself is the `snapshot` column.
    // - OIR lookup: dispatch only (no qs scan at all).
    // - snapshot:   `config.snapshot()` alone — the one shared config cost
    //   `prepare` pays per request.
    println!(
        "{:<42} {:>10} {:>10} {:>10} {:>10} {:>10}",
        "case", "FVR route", "OIR route", "OIR fast", "OIR lookup", "snapshot"
    );
    println!("{}", "-".repeat(100));

    // One snapshot for the whole bench, mirroring `prepare`.
    let config = ccx.config.snapshot();

    for c in oir_cases() {
        let mut req = make_request(c.method);
        req.s3ext.s3_path = Some(rebuild_s3_path(&c.path));
        let qs = OrderedQs::from_vec_unchecked(c.qs.iter().map(|(k, v)| ((*k).to_owned(), (*v).to_owned())).collect());
        req.s3ext.qs = Some(qs);

        // Sanity check: both the OIR path and the shape router must resolve
        // the same input (qs including `x-id`) to the expected operation.
        {
            let op = resolve_oir(black_box(&req), black_box(&config))
                .unwrap()
                .expect("OIR path should resolve");
            assert_eq!(op.name(), c.expect_op, "case `{}` resolved to wrong op", c.name);

            let op = resolve_route(black_box(&req), black_box(&c.path), req.s3ext.qs.as_ref()).expect("router should resolve");
            assert_eq!(op.name(), c.expect_op, "case `{}` routed to wrong op", c.name);
        }

        // FVR: shape routing over the identical qs. The `x-id` key is scanned
        // as a plain query key — the real cost of consuming a request that
        // carries the signal.
        let mut run = || {
            let result = resolve_route(black_box(&req), black_box(&c.path), black_box(req.s3ext.qs.as_ref()));
            black_box(result.ok());
        };
        let fvr_ns = time_ns_per_op(&mut run, iters);

        // OIR: route-only — single-pass `x-id` extraction + dispatch, no
        // config snapshot (a cost every request pays regardless of routing)
        // and no `s3_path` prep. The peer of FVR route.
        let mut run = || {
            let signal = req.s3ext.qs.as_ref().and_then(|qs| match qs.lookup("x-id") {
                QsLookup::Single(v) => Some(v),
                QsLookup::Absent | QsLookup::Duplicate => None,
            });
            let result = signal.and_then(|v| resolve_operation_by_id(black_box(c.method), black_box(&c.path), black_box(v)));
            black_box(result);
        };
        let oir_route_ns = time_ns_per_op(&mut run, iters);

        // OIR: full fast path (x-id extraction + dispatch; config pre-fetched
        // as `prepare` does). The shared snapshot cost is the last column.
        let mut run = || {
            let result = resolve_oir(black_box(&req), black_box(&config));
            black_box(result.ok());
        };
        let oir_ns = time_ns_per_op(&mut run, iters);

        // OIR: pure lookup only (no signal extraction, no config snapshot).
        // Isolates the dispatch portion of the OIR path.
        let mut run = || {
            let result = resolve_operation_by_id(black_box(c.method), black_box(&c.path), black_box(c.expect_op));
            black_box(result);
        };
        let lookup_ns = time_ns_per_op(&mut run, iters);

        // Config snapshot alone (shared cost paid by every request path).
        let mut run = || {
            let c = black_box(ccx.config.snapshot());
            black_box(c.operation_id_routing);
        };
        let snap_ns = time_ns_per_op(&mut run, iters);

        println!(
            "{:<42} {:>10.1} {:>10.1} {:>10.1} {:>10.1} {:>10.1}",
            c.name, fvr_ns, oir_route_ns, oir_ns, lookup_ns, snap_ns
        );
    }
}

/// Decomposes `config.snapshot()`: dyn-trait vtable call, `Arc::clone` atomic
/// increment, and the heap field read. Isolates why the shared config access
/// costs ~9-10 ns in the OIR bench.
#[test]
#[ignore = "micro-benchmark; run with: cargo test -p s3s --release route_bench -- --ignored --nocapture"]
fn snapshot_bench() {
    fn measure(iters: u64, name: &str, mut f: impl FnMut()) {
        let ns = time_ns_per_op(&mut f, iters);
        println!("{name:<38} {ns:>10.1}");
    }

    let iters = 20_000_000u64;
    let parts = ctx();
    let ccx = ccx(&parts);
    let cfg: Arc<S3Config> = Arc::new(S3Config::default());

    println!("{:<38} {:>10}", "snapshot decomposition", "ns/op");
    println!("{}", "-".repeat(50));

    // Baseline: what `resolve_oir` does (`config` is `&Arc<dyn …>`).
    measure(iters, "1 dyn snapshot + field read", || {
        let c = black_box(ccx.config.snapshot());
        black_box(c.operation_id_routing);
    });
    // Same work, but through the static type — isolates the dyn dispatch.
    measure(iters, "2 static Arc clone + field read", || {
        let c = black_box(Arc::clone(&cfg));
        black_box(c.operation_id_routing);
    });
    // dyn snapshot without touching the heap through the Arc.
    measure(iters, "3 dyn snapshot only", || {
        let c = black_box(ccx.config.snapshot());
        black_box(Arc::as_ptr(&c));
    });
    // Heap field read with no clone at all.
    measure(iters, "4 field read only", || {
        black_box(cfg.operation_id_routing);
    });
    // Static `Arc::clone` alone (atomic increment, no heap access).
    measure(iters, "5 static Arc clone only", || {
        let c = black_box(Arc::clone(&cfg));
        black_box(Arc::as_ptr(&c));
    });
}
