---

name: "code-coverage"
description: Measure and grow the line coverage of the s3s crate. Use when adding tests for generated code, when asked how much of s3s is covered, or when a coverage number, goal or regression is discussed.
license: "Apache-2.0"
---

# Code coverage

The `s3s` crate is measured with [cargo-llvm-cov](https://github.com/taiki-e/cargo-llvm-cov). The goal for the crate is 90% line coverage.

## Measure

```sh
cargo llvm-cov -p s3s --all-features --no-fail-fast --json --summary-only --output-path target/coverage-s3s.json
cargo run -p xtask -- coverage target/coverage-s3s.json
```

`xtask coverage` prints one row per package plus a `TOTAL` row, and aggregates the per-file counters of the JSON export. `--fail-under-lines 90` turns the same command into a gate; no CI job fails on coverage today, so a threshold is only enforced where it is passed explicitly.

## Which number counts

- The JSON summary is the reference: the line metric it prints is the one `--fail-under-lines` enforces.
- `--lcov` and the text report count lines slightly differently and print a higher percentage for the same profile. Name the source of a number, and compare like with like.
- The `branches` column stays at 0% unless the profile is built with `--branch`, which needs nightly.
- cargo-llvm-cov ignores `tests/` directories by default, so test bodies never enter the denominator. Keep it that way: tests belong in `crates/s3s/tests/`, not next to the code they exercise.

## Where tests for generated code live

In `crates/s3s/tests/generated/`: a single integration-test binary, auto-discovered from `tests/generated/main.rs`, whose files are written by `just codegen` from the Smithy models (`codegen/src/v1/gen_tests/`).

- Extend the generator instead of editing those files by hand: a model change then updates the code and its tests together, and the two cannot drift.
- The module loader and the file set are fixed by the generator; add a family by adding an emitter module, not by dropping a file into the directory.
- Emitters receive the base model and the MinIO model. Mirror the `#[cfg(feature = "minio")]` gates of the merged `generated.rs`, including the two branches of a type that the MinIO patch rewrites.
- Assertions must be falsifiable: compare whole values or exact strings rather than `is_empty()`-style checks, and prove each family with a red control — inject an error into the production side and watch the matching case fail.

## Where tests for hand-written code live

Next to the existing integration tests in `crates/s3s/tests/*.rs`.

## Growing the number

- Start from the biggest gaps. `xtask coverage` gives the per-package rows; read `data[0].files` of the export, or `cargo llvm-cov report`, for the per-file ones.
- Do not modify production code only to make a line coverable. Private items, defensive branches, branches decided by a `cfg!` constant and LLVM line-mapping artifacts are recorded as unreachable instead.
- Attempt a cheaper input before writing a line off as unreachable: the closing brace of a skipped `if let` is covered by feeding the empty value, and a required field by omitting it.
