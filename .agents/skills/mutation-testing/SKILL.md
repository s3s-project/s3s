---

name: "mutation-testing"
description: Run or triage the mutation sweep in this repository. Use when a survivor needs a decision, when the sweep scope, budget or cursor is discussed, when the scheduled run fails, or when a new crate should join the mutation scope.
license: "Apache-2.0"
---

# Mutation testing

The `testing` skill covers where the tests this sweep exercises live, and `verification` covers the red control that proves a new test can fail; this skill is about running the sweep and deciding what to do with a survivor.

## Run a sweep locally

From the repository root, `just mutants-run [BUDGET_MINUTES] [FILES]` runs one budgeted sweep over the same rotation the scheduled run uses, and `cargo run -p xtask -- mutants plan` prints what that sweep would pick without mutating anything. One file is enough to iterate on: `just mutants-run 6 crates/s3s-multipart/src/error.rs`. Every unit prints the exact `cargo mutants` command it runs, including its `--file` and output directory, so that single line can be repeated by hand while a survivor is triaged, and `cargo mutants --file <path> --list` counts a file's mutants before spending time on it. Afterwards `cargo run -p xtask -- mutants report --out mutants.out --check` aggregates the outcome directories and fails on a survivor that is not accepted.

## What runs on its own

`.github/workflows/mutants.yml` runs `mutants run` once a day with a fifteen-minute budget. It is scheduled only: pull requests never trigger it, and no local recipe drives it either. The sweep keeps a cursor as a CI artifact, and losing it only restarts the rotation.

## Scope

The scope is the signing and parsing crates — `s3s-multipart`, `s3s-chunked`, `s3s-sigv2`, `s3s-sigv4`, `s3s-rfc2047` — listed in `SCOPE` in `xtask/src/mutants.rs`. It cannot be `examine_globs` in `.cargo/mutants.toml`: that option makes cargo-mutants ignore `--file`, which the per-file rotation needs. Extending the scope is an edit to `SCOPE` and a decision about cost, not a side effect.

## The rotation

Each run takes the files that are due: never swept first, then files whose content changed since their last sweep (a digest, never an mtime, because a checkout rewrites mtimes), then the oldest. A file is the unit; a file larger than the budget is started anyway and recorded as a partial sweep, so it makes progress instead of stalling the rotation.

## Triage a survivor

A survivor is a result, not a failure: cargo-mutants exit codes 0, 2 and 3 are all useful runs, and only 1, 4, 5, 6 and 70 mean the run itself broke. Every survivor gets one of two answers.

- A test gap: the mutant changes behaviour that nothing asserts. Write the test that fails for the right reason, then re-run that file.
- An equivalent mutant: the change cannot alter behaviour. Record it in the `ACCEPTED` table in `xtask/src/mutants.rs` with a status and a reason a reviewer can check. The table is code so that it is reviewed together with the sweep that produced it.

Do not reach for `exclude_globs` or `exclude_re` to hide a survivor; those options are for code that cannot be mutated meaningfully, such as generated files. `cargo run -p xtask -- mutants check` validates the configuration and the accepted list without mutating anything.

## Resources

A mutant can make a test allocate without limit: the sweep caps its own address space through `rlimit` where the platform has it, keeps cargo-mutants at two workers unless `CARGO_MUTANTS_JOBS` says otherwise, and bounds each run by the wall-clock budget. A run that has to be killed is a signal that the budget or the tests need attention, not that the cap should be raised quietly.
