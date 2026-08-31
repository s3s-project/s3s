// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

//! Operation-intent routing (OIR) codegen: emits `resolve_operation_by_id`,
//! the (method, path shape) partitioned lookup over the official operation
//! names keyed by the `x-id` query value.

use super::ops::{OPS_GENERATED_DIR, Operations, PathPattern, codegen_file_header};
use super::write_dir_file;
use scoped_writer::g;
use std::collections::BTreeMap;

/// Emits the OIR operation lookup: `resolve_operation_by_id` backed by
/// per-`(HTTP method, path shape)` dispatch over the operation names (the
/// `x-id` query value).
///
/// The synthetic `PostObject` is excluded; MinIO-only operations participate
/// (a client may send `x-id` uniformly) and their references are emitted
/// under `#[cfg(feature = "minio")]`. Each partition is a `match` over the
/// full operation names, so a hit is returned only when the name equals a
/// registered operation name.
///
/// The lookup properties are re-checked by the generated tests in
/// `oir_lookup_tests`.
pub(super) fn codegen_oir(ops: &Operations) {
    let buckets = collect_oir_buckets(ops);

    write_dir_file(OPS_GENERATED_DIR, "oir.rs", || {
        codegen_file_header();
        g!();
        g!("#![allow(clippy::unreadable_literal)]");
        g!();
        g!("use crate::ops::*;");
        g!("use crate::path::S3Path;");
        g!();

        g!("/// OIR operation lookup: (method, path shape) dispatch then a");
        g!("/// `match` over the declared operation name.");
        g!("///");
        g!("/// The (method, path shape) partition is the request-shape check; the");
        g!("/// `match` arms compare the full name, so a hit is returned only when");
        g!("/// `name` is exactly a registered operation name. Required query");
        g!("/// strings / headers / query tags are validated later by");
        g!("/// `deserialize_http`.");
        g!("pub fn resolve_operation_by_id(");
        g!("    method: &str,");
        g!("    s3_path: &S3Path,");
        g!("    name: &str,");
        g!(") -> Option<&'static dyn crate::ops::Operation> {{");
        g!("    match (method, s3_path) {{");
        for bucket in &buckets {
            let path_pattern = match bucket.path {
                PathPattern::Root => "S3Path::Root",
                PathPattern::Bucket => "S3Path::Bucket { .. }",
                PathPattern::Object => "S3Path::Object { .. }",
            };
            // A single-name MinIO-only bucket gates the whole arm; in a
            // mixed bucket only the MinIO-only arms are gated.
            let arm_minio = bucket.names.len() == 1 && bucket.names[0].1;
            if arm_minio {
                g!("#[cfg(feature = \"minio\")]");
            }
            g!("        (\"{}\", {path_pattern}) => {{", bucket.method);
            if bucket.names.len() == 1 {
                let (name, _) = &bucket.names[0];
                g!("            (name == \"{name}\").then_some(&{name} as &'static dyn crate::ops::Operation)");
            } else {
                g!("            match name {{");
                for (name, is_minio) in &bucket.names {
                    if *is_minio {
                        g!("                #[cfg(feature = \"minio\")]");
                    }
                    g!("                \"{name}\" => Some(&{name} as &'static dyn crate::ops::Operation),");
                }
                g!("                _ => None,");
                g!("            }}");
            }
            g!("        }}");
        }
        g!("        _ => None,");
        g!("    }}");
        g!("}}");
        g!();

        codegen_oir_lookup_tests(&buckets);
    });
}

/// One `(HTTP method, path shape)` bucket of the OIR lookup.
struct OirBucket {
    method: String,
    path: PathPattern,
    /// (operation name, whether MinIO-only) pairs, sorted.
    names: Vec<(String, bool)>,
}

/// Collects the OIR buckets (names grouped by (method, path shape)).
/// Excludes the synthetic `PostObject`; MinIO-only operations participate
/// (with their `is_minio` flag) and are emitted under `#[cfg(feature = "minio")]`.
fn collect_oir_buckets(ops: &Operations) -> Vec<OirBucket> {
    let mut groups: BTreeMap<(String, PathPattern), Vec<(String, bool)>> = BTreeMap::new();
    for op in ops.values() {
        if op.name == "PostObject" {
            continue;
        }
        // The official `x-id` query value must equal the operation name so
        // that the lookup can be keyed by it directly.
        if let Some(x_id) = PathPattern::x_id_value(&op.http_uri) {
            assert_eq!(x_id, op.name, "x-id {x_id:?} does not match op name {:?}", op.name);
        }
        let path = PathPattern::parse(&op.http_uri);
        groups
            .entry((op.http_method.clone(), path))
            .or_default()
            .push((op.name.clone(), op.is_minio));
    }

    groups
        .into_iter()
        .map(|((method, path), names)| OirBucket { method, path, names })
        .collect()
}

/// Emits the generated perfect-hash property tests guarding `resolve_operation_by_id`
/// against regression (hand edits or model drift without regeneration).
#[allow(clippy::too_many_lines)]
fn codegen_oir_lookup_tests(buckets: &[OirBucket]) {
    g!("#[cfg(test)]");
    g!("mod oir_lookup_tests {{");
    g!("    #![allow(");
    g!("        clippy::unwrap_used,");
    g!("        clippy::expect_used,");
    g!("        clippy::panic,");
    g!("        clippy::unreachable,");
    g!("        clippy::indexing_slicing");
    g!("    )]");
    g!();
    g!("    use super::*;");
    g!("    use crate::path::S3Path;");
    g!();
    g!("    fn path_of(kind: &str) -> S3Path {{");
    g!("        match kind {{");
    g!("            \"Root\" => S3Path::Root,");
    g!("            \"Bucket\" => S3Path::Bucket {{ bucket: \"bucket\".into() }},");
    g!("            \"Object\" => S3Path::Object {{ bucket: \"bucket\".into(), key: \"key\".into() }},");
    g!("            _ => unreachable!(\"unknown path kind\"),");
    g!("        }}");
    g!("    }}");
    g!();
    g!("    // Every registered (method, path kind, operation name).");
    g!("    const CASES: &[(&str, &str, &str)] = &[");
    for bucket in buckets {
        let kind = match bucket.path {
            PathPattern::Root => "Root",
            PathPattern::Bucket => "Bucket",
            PathPattern::Object => "Object",
        };
        for (name, is_minio) in &bucket.names {
            if *is_minio {
                g!("        #[cfg(feature = \"minio\")]");
            }
            g!("        (\"{}\", \"{kind}\", \"{name}\"),", bucket.method);
        }
    }
    g!("    ];");
    g!();
    g!("    fn mutate_letter(name: &str, idx: usize) -> String {{");
    g!("        name.chars()");
    g!("            .enumerate()");
    g!("            .map(|(i, ch)| if i == idx {{ if ch == 'a' {{ 'b' }} else {{ 'a' }} }} else {{ ch }})");
    g!("            .collect()");
    g!("    }}");
    g!();
    g!("    #[test]");
    g!("    fn every_registered_name_resolves_to_itself() {{");
    g!("        // Completeness + injectivity: the generated perfect hash maps");
    g!("        // each registered name to its own operation.");
    g!("        for &(method, kind, name) in CASES {{");
    g!("            let op = resolve_operation_by_id(method, &path_of(kind), name)");
    g!("                .unwrap_or_else(|| panic!(\"no match for {{method}} {{kind}} {{name}}\"));");
    g!("            assert_eq!(op.name(), name, \"mismatch for {{method}} {{kind}} {{name}}\");");
    g!("        }}");
    g!("    }}");
    g!();
    g!("    #[test]");
    g!("    fn resolved_operation_always_matches_the_input_name() {{");
    g!("        // The mandatory equality confirm after the hash probe: a hit is");
    g!("        // only valid when the full input name equals the expected value.");
    g!("        // Mutating any registered name must never resolve to a different");
    g!("        // operation.");
    g!("        for &(method, kind, name) in CASES {{");
    g!("            for idx in 0..name.len() {{");
    g!("                let mutated = mutate_letter(name, idx);");
    g!("                if let Some(op) = resolve_operation_by_id(method, &path_of(kind), &mutated) {{");
    g!("                    assert_eq!(op.name(), mutated);");
    g!("                }}");
    g!("            }}");
    g!("        }}");
    g!("    }}");
    g!();
    g!("    #[test]");
    g!("    fn wrong_shape_is_rejected() {{");
    g!("        // A registered name queried under a different (method, path)");
    g!("        // shape must not resolve: the declaration does not match the");
    g!("        // request.");
    g!("        let shapes = [");
    g!("            (\"GET\", \"Root\"),");
    g!("            (\"GET\", \"Bucket\"),");
    g!("            (\"GET\", \"Object\"),");
    g!("            (\"PUT\", \"Bucket\"),");
    g!("            (\"PUT\", \"Object\"),");
    g!("            (\"DELETE\", \"Bucket\"),");
    g!("            (\"DELETE\", \"Object\"),");
    g!("            (\"POST\", \"Bucket\"),");
    g!("            (\"POST\", \"Object\"),");
    g!("        ];");
    g!("        for &(method, kind, name) in CASES {{");
    g!("            for &(other_method, other_kind) in &shapes {{");
    g!("                if other_method == method && other_kind == kind {{");
    g!("                    continue;");
    g!("                }}");
    g!("                assert!(");
    g!("                    resolve_operation_by_id(other_method, &path_of(other_kind), name).is_none(),");
    g!("                    \"{{name}} resolved under {{other_method}} {{other_kind}}\"");
    g!("                );");
    g!("            }}");
    g!("        }}");
    g!("    }}");
    g!();
    g!("    #[test]");
    g!("    fn unknown_names_are_rejected() {{");
    g!("        for name in [\"\", \"NoSuchOperation\", \"Get\", \"Put\", \"x-id\", \"ListBucketX\"] {{");
    g!("            for &(method, kind) in &[(\"GET\", \"Bucket\"), (\"GET\", \"Object\"), (\"PUT\", \"Object\")] {{");
    g!("                assert!(");
    g!("                    resolve_operation_by_id(method, &path_of(kind), name).is_none(),");
    g!("                    \"{{name:?}} resolved under {{method}} {{kind}}\"");
    g!("                );");
    g!("            }}");
    g!("        }}");
    g!("    }}");
    g!("}}");
}
