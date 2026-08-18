# AGENTS.md

Agent guidance for the **s3s** repository.

## Overview

s3s is an experimental Rust project providing an ergonomic adapter for building
S3-compatible services. It implements the Amazon S3 REST API as a generic `hyper`
service: HTTP requests are converted to S3 operation inputs, user-defined services
are called, and outputs or errors are converted back to HTTP responses.

## Architecture

- `s3s` — core crate: S3 REST API as a hyper service
- `s3s-aws` — aws-sdk-s3 integration and useful types
- `s3s-fs` — sample file-system implementation (testing/debugging)
- `s3s-model` — data types generated from AWS Smithy models
- `s3s-policy` — S3 policy handling
- `s3s-test` — testing utilities
- `s3s-proxy` — proxy for E2E testing
- `s3s-e2e` — end-to-end testing framework

## Development

- Tools: Rust (MSRV 1.96.0), `just`, `uv`, Docker (for E2E).
- Use `just` for all tasks — `just dev` (fetch/fmt/codegen/lint/test), `just crawl` +
  `just codegen` (regenerate from Smithy models), `just semver-checks`.
  Full list in `justfile`.
- Code is generated from AWS Smithy models (`data/`); after changing generation code,
  always run `just codegen` (must stay idempotent).
- Testing: unit tests, integration tests, and Docker-based E2E (MinIO, `s3s-fs`, MinT,
  Ceph s3-tests). Human-oriented workflow: `CONTRIBUTING.md`.
- Style: clippy strict lints (`all`, `pedantic`, `cargo` = deny); `unsafe` forbidden;
  ruff for Python; Conventional Commits.

## Working with the codebase

- Review existing tests for implementation patterns.
- Look at `s3s-fs` as a reference implementation.
- Modifying S3 operations: check if codegen is needed (`just codegen`), update tests,
  run the full test suite (`just test`), consider E2E impact.
- Adding features: follow the crate structure, add unit/integration tests, update docs
  if public APIs change, run `just dev`.
- Fixing bugs: add a reproducing test first, fix minimally, run the full suite.

## Where to look

- Development workflow, requirements, E2E testing → `CONTRIBUTING.md`
- Task automation → `justfile`
- CI jobs and triggers → `.github/workflows/`
- Smithy models and codegen → `data/`, `codegen/src/`

## Security

The `S3Service` and adapters have no built-in security protection. Consider HTTP body
length limits, rate limiting, and back pressure; services may be exposed to the Internet.

## Gotchas

- `codegen` emits two variants per generated file (`generated.rs` / `generated_minio.rs`),
  selected by `cfg_if!` — verify both feature variants.
- Prefer `` [`path::Type`] `` intra-doc links; `[`Type`](path::Type)` triggers
  `rustdoc::redundant_explicit_links`, bare `[Type]` triggers clippy `doc_markdown`.
