// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

mod rust;
mod smithy;
mod utils;

mod access;
mod dto;
mod error;
mod headers;
mod minio;
mod ops;
mod order;
mod postprocess;
mod s3_trait;
mod sts;
mod xml;

mod aws_conv;
mod aws_proxy;

use std::fs::File;
use std::io::BufWriter;

pub use self::utils::o;

fn write_file(path: &str, f: impl FnOnce()) {
    let mut writer = BufWriter::new(File::create(path).unwrap());
    scoped_writer::scoped(&mut writer, f);
}

fn write_dir_file(dir: &str, name: &str, f: impl FnOnce()) {
    std::fs::create_dir_all(dir).unwrap();
    write_file(&format!("{dir}/{name}"), f);
}

#[derive(Debug, Clone, Copy)]
enum Patch {
    Minio,
}

pub fn run() {
    let base = inner_run(None);
    let minio = inner_run(Some(Patch::Minio));
    // Sanity: the base operation set must be a subset of the union, and the
    // `is_minio` flag must exactly mark the operations that only exist in the
    // MinIO model variant.
    let base_names: std::collections::BTreeSet<_> = base.ops.keys().collect();
    let minio_names: std::collections::BTreeSet<_> = minio.ops.keys().collect();
    assert!(base_names.is_subset(&minio_names), "base ops must be a subset of minio ops");
    let minio_only: std::collections::BTreeSet<_> = minio_names.difference(&base_names).copied().collect();
    let flagged: std::collections::BTreeSet<_> = minio.ops.iter().filter(|(_, op)| op.is_minio).map(|(name, _)| name).collect();
    assert_eq!(flagged, minio_only, "is_minio flag must mark exactly the minio-only operations");

    // ops 以 union（minio 全集）模型单次生成，base/minio 差异在 codegen 内内联门控。
    ops::codegen(&minio.ops, &base.rust_types, &minio.rust_types);
    postprocess();
}

struct ModelData {
    ops: ops::Operations,
    rust_types: dto::RustTypes,
}

fn inner_run(code_patch: Option<Patch>) -> ModelData {
    let model = {
        let mut s3_model = smithy::Model::load_json("data/s3.json").unwrap();

        let mut sts_model = smithy::Model::load_json("data/sts.json").unwrap();
        sts::reduce(&mut sts_model);
        s3_model.shapes.append(&mut sts_model.shapes);

        if matches!(code_patch, Some(Patch::Minio)) {
            minio::patch(&mut s3_model);
        }

        s3_model
    };

    let ops = ops::collect_operations(&model);
    let rust_types = dto::collect_rust_types(&model, &ops);

    let suffix = match code_patch {
        Some(Patch::Minio) => "_minio",
        None => "",
    };

    {
        let path = format!("crates/s3s/src/dto/generated{suffix}.rs");
        write_file(&path, || dto::codegen(&rust_types, &ops, code_patch));
    }

    {
        let path = "crates/s3s/src/header/generated.rs";
        write_file(path, || headers::codegen(&model));
    }

    {
        let path = "crates/s3s/src/error/generated.rs";
        write_file(path, || error::codegen(&model));
    }

    {
        let path = format!("crates/s3s/src/xml/generated{suffix}.rs");
        write_file(&path, || xml::codegen(&ops, &rust_types));
    }

    {
        let path = "crates/s3s/src/s3_trait.rs";
        write_file(path, || s3_trait::codegen(&ops));
    }

    {
        let path = "crates/s3s/src/access/generated.rs";
        write_file(path, || access::codegen(&ops));
    }

    {
        let path = format!("crates/s3s-aws/src/conv/generated{suffix}.rs");
        write_file(&path, || aws_conv::codegen(&ops, &rust_types));
    }

    {
        let path = "crates/s3s-aws/src/proxy/generated.rs";
        write_file(path, || aws_proxy::codegen(&ops, &rust_types));
    }

    ModelData { ops, rust_types }
}

/// Merge each `generated.rs` / `generated_minio.rs` pair into a single file:
/// identical logic groups are kept once, differing groups are gated with
/// `#[cfg(feature = "minio")]`, and the `_minio` file is removed.
pub fn postprocess() {
    postprocess::run(&[
        ("crates/s3s/src/dto/generated.rs", "crates/s3s/src/dto/generated_minio.rs"),
        ("crates/s3s/src/xml/generated.rs", "crates/s3s/src/xml/generated_minio.rs"),
        ("crates/s3s-aws/src/conv/generated.rs", "crates/s3s-aws/src/conv/generated_minio.rs"),
    ]);
}
