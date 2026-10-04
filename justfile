dev:
    just fetch
    just fmt
    just codegen
    just lint
    just test

fetch:
    uv sync
    cargo fetch

fmt:
    uvx ruff format
    cargo fmt --all

lint:
    uvx ruff check
    cargo clippy --workspace --all-features --all-targets

test:
    cargo test --workspace --all-features --all-targets

semver-checks:
    cargo semver-checks

doc:
    RUSTDOCFLAGS="--cfg docsrs" cargo +nightly doc --open --no-deps --all-features

crawl:
    cargo run -p xtask -- crawl update

spdx-check:
    cargo run -p xtask -- spdx check

spdx-apply:
    cargo run -p xtask -- spdx apply

link-license:
    cargo run -p xtask -- link-license

link-license-check:
    cargo run -p xtask -- link-license --check

codegen:
    cargo run -p s3s-codegen
    cargo fmt --all
    cargo check

install-fs *ARGS:
    cargo install --path crates/s3s-fs --locked {{ARGS}} --features binary --force

install-proxy *ARGS:
    cargo install --path crates/s3s-proxy --locked {{ARGS}} --features minio --force

install-e2e *ARGS:
    touch crates/s3s-e2e/build.rs
    cargo install --path crates/s3s-e2e --locked {{ARGS}} --force

install-all:
    cargo fetch
    just install-fs --offline
    just install-proxy --offline
    just install-e2e --offline

coverage *ARGS:
    cargo llvm-cov -p s3s -p s3s-chunked -p s3s-sigv2 -p s3s-sigv4 -p s3s-test --all-features --html {{ARGS}}

# ------------------------------------------------

sync-version:
    cargo set-version -p s3s            0.17.0
    cargo set-version -p s3s-sigv2      0.17.0
    cargo set-version -p s3s-sigv4      0.17.0
    cargo set-version -p s3s-rfc2047    0.17.0
    cargo set-version -p s3s-aws        0.17.0
    cargo set-version -p s3s-model      0.17.0
    cargo set-version -p s3s-policy     0.17.0
    cargo set-version -p s3s-test       0.17.0
    cargo set-version -p s3s-proxy      0.17.0
    cargo set-version -p s3s-fs         0.17.0
    cargo set-version -p s3s-e2e        0.17.0
    cargo set-version -p s3s-http3      0.17.0-alpha.1
    cargo set-version -p s3s-multipart  0.17.0
    cargo set-version -p s3s-chunked    0.18.0-alpha.2

# ------------------------------------------------

assert_unchanged:
    #!/bin/bash -ex
    [[ -z "$(git status -s)" ]] # https://stackoverflow.com/a/9393642

ci-rust:
    cargo fmt --all --check
    cargo clippy --workspace --all-features --all-targets -- -D warnings
    just test
    just codegen
    just assert_unchanged

ci-python:
    uvx ruff format --check
    uvx ruff check

# --- mutation testing (cargo-mutants) ------------------------------------------
# Sweeps cap the address space: a mutant can make a test allocate without limit.

# What the next sweep would run, without mutating anything.
mutants-plan budget="15":
    cargo run -q -p xtask -- mutants plan --budget-minutes {{budget}}

# One budgeted sweep of the files that are due.
mutants-run budget="15" files="":
    cargo run -q -p xtask -- mutants run --budget-minutes {{budget}} --files "{{files}}"

