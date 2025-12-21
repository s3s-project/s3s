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

doc:
    RUSTDOCFLAGS="--cfg docsrs" cargo +nightly doc --open --no-deps --all-features

crawl:
    uv run data/crawl.py update

codegen:
    cargo run -p s3s-codegen
    cargo fmt --all
    cargo check

install name *ARGS:
    uv run ./scripts/install.py {{name}} {{ARGS}}

# ------------------------------------------------

sync-version:
    cargo set-version -p s3s            0.13.0-alpha
    cargo set-version -p s3s-aws        0.13.0-alpha
    cargo set-version -p s3s-model      0.13.0-alpha
    cargo set-version -p s3s-policy     0.13.0-alpha
    cargo set-version -p s3s-test       0.13.0-alpha
    cargo set-version -p s3s-proxy      0.13.0-alpha
    cargo set-version -p s3s-fs         0.13.0-alpha
    cargo set-version -p s3s-e2e        0.13.0-alpha

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
    just crawl
    just assert_unchanged
