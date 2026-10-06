---

name: "testing"
description: Add or run tests in this repository. Use when placing a new test, when a test target is not picked up, when the feature sets matter, or when an end-to-end suite is involved.
license: "Apache-2.0"
---

# Testing

## Where a test goes

- Unit tests sit beside the code they exercise, in a `#[cfg(test)] mod tests` in the same file, with fixtures in a neighbouring directory (for example `crates/s3s/src/ops/tests/`).
- Integration tests live under `crates/<crate>/tests/`. Two layouts work: one file per target (`tests/dto.rs` becomes the `dto` target) and a directory with `main.rs` (`tests/protocol/main.rs` becomes the `protocol` target). Cargo discovers both, so a `[[test]]` declaration is only needed when the defaults are not enough.
- This repository declares one when a target needs `harness = false` (a custom harness, as `s3s-fs` does for its AWS and OpenDAL suites) or `required-features` (the HTTP/3 suite needs `http3`). Reach for a declaration only for those reasons.
- Tests for generated code are generated: see the `code-coverage` skill before adding one by hand under `crates/s3s/tests/generated/`.

## Running a subset

`cargo test -p <crate>`, `cargo test -p <crate> --test <target>`, and `--all-features` for the second feature set. `just test` is the workspace command (`--workspace --all-features --all-targets`). CI runs it on two toolchains, on the MSRV and on two platforms. Exercise both feature sets before calling a change done: a merged generated file has code behind both.

## What a test should assert

Compare whole values, exact strings or exact error variants, so that a wrong implementation fails for the reason the test names. A test that only checks "did not panic", "is not empty" or "returned Ok" cannot fail for the right reason; the `verification` skill describes the red control that proves a test can fail at all.

## Fixtures

Keep fixtures next to their tests (`crates/s3s/tests/fixtures/`, `crates/s3s/src/ops/tests/fixtures/`) unless the generator consumes them, in which case they belong under `data/`.

## Neighbouring suites

Fuzzing lives in its own workspace and has its own skill (`fuzz-testing`), as does the scheduled mutation sweep (`mutation-testing`). What follows is the part of the suite that runs like a test.

## End-to-end suites

The end-to-end suites are shell scripts under `scripts/` (silo, `s3s-fs`, `s3s-proxy`, mint, Ceph s3-tests, rclone, boto3) with their instructions in `CONTRIBUTING.md`. Two of them are gates with expected-failure baselines: `cargo run -p xtask -- report mint <log.json>` and `cargo run -p xtask -- report s3-tests <junit.xml>`, which keep the counters and the allow-list of known failures honest. CI runs them behind path filters, so a green pull request does not mean they ran.

## The wasm build

`crates/s3s-wasm` runs the workspace under WebAssembly with its own suite, covered by the `wasm-test` CI job; it is the cheapest way to catch a platform assumption.
