---

name: "verification"
description: Settle a claim about a change with evidence that can be recomputed. Use when a change claims to fix or improve something, when a gate result has to be trusted, when a test is meant to prove behaviour, or when a port or a rewrite must be shown equivalent.
license: "Apache-2.0"
---

# Verification

A claim about a change is settled by an artifact somebody else can recompute: a command and its exit code, a count, a tree, a diff, a file. Reading the diff again is not verification, and "should work" is not a result.

## Two controls

Every check needs both directions.

- **Red control** — inject the failure the check claims to catch and watch it fail. Rename the field a test compares, break the branch a parser is supposed to reject, corrupt the input: if nothing goes red, the check proves nothing.
- **Positive control** — run the same check on the good input and watch it pass, so a check that always fails, or one that matches nothing at all, is not mistaken for agreement.

A new script that validates something deserves the same pair before it is trusted: feed it a sample that must fail and a sample that must pass.

## Keep a gate as a script

Run a gate as one script that writes its output and its exit code to a log, so a failure can still be attributed afterwards. The gates here are `just dev` (fetch, format, codegen, lint, test), `just ci-rust` (format check, clippy with `-D warnings`, tests, codegen, `assert_unchanged`) and the nightly clippy line; `just ci-rust` fails when a codegen run leaves an uncommitted diff, because `assert_unchanged` compares the tree with `git status`.

## Local gates are not CI gates

CI runs a matrix nobody runs locally: two toolchains, the MSRV, macOS and Windows, a wasm build, both feature sets, and jobs that are filtered by path. Two consequences:

- A green check does not prove your code ran. `Audit`, `Fuzz` and the `e2e` jobs skip when the paths they watch are untouched, and a lane can be allowed to fail: the `rust` job marks its nightly lane `continue-on-error`, so a nightly-only lint does not block a merge. Reproduce the gate you care about locally.
- A local pass does not prove CI passes. Run what the job runs, with the same flags, before saying a failure is fixed.

## Measure the same way twice

An expectation only matches the measurement that produced it. Do not reuse a number that came from another tool: the coverage JSON, the LCOV file and the text report of one profile count lines differently. Recompute with the command the check uses, and name the source of every number.

## Flakes

A test that failed once is not understood until two cases are separated: run it alone N times, and run the whole suite N times. If only the parallel run fails, look for shared state — ports, timers, files, a global; if the single run fails as well, it is a bug. Record the first failure with its environment instead of retrying until green.

## Mutation as a last resort

When the question is "could this test ever fail", mutate one production line, run the test, then restore the file byte for byte. Point the build at a separate target directory: a mutated artifact left in a shared cache poisons later runs. Keep the mutation to the lines the test claims to cover.

## Ports and rewrites

A rewrite is verified against the thing it replaces, not against its own output: build a table of inputs, run both implementations on every row, and compare stdout, stderr, exit code and bytes where the bytes are the contract. The `xtask report` subcommands were accepted that way, by feeding the retired scripts and the new ones the same mint log and the same JUnit report.

## What to write down

State the claim, the command, the exit code and the artifact. When a claim cannot be settled with what is at hand, name the artifact that would settle it and leave the question open rather than softening it.
