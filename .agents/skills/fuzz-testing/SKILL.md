---

name: "fuzz-testing"
description: Add, run or schedule a fuzz target in this repository. Use when a target is added or renamed, when a scheduled run fails, when a crash has to be reproduced from an artifact, or when the fuzz budget and rotation are discussed.
license: "Apache-2.0"
---

# Fuzz testing

## The workspace

`fuzz/` is its own workspace: the root `just test` and `just lint` never see it, so run its recipes from that directory. `just ci-check` is the gate — the formatting check, clippy with `--cfg fuzzing` on nightly, and the `multipart_parser` seed check — and it fails before anything fuzzes. A committed seed under `fuzz/seeds/<target>/` is a promise: it encodes the outcome it was added for, so a seed whose result changes is a corpus bug and travels in the same commit as the fix it covers. How much of that a recipe can check depends on the target: `just check-seeds` covers `multipart_parser`, `just gen-corpus` validates the `xml_bodies` seeds while regenerating them, and the `aws_chunked` and `syntax_parsers` seeds have no checker at all, so those rest on review.

## Run one target locally

From `fuzz/`, `just fuzz <target>` copies the committed seeds into `fuzz/corpus/<target>/` and starts cargo-fuzz on nightly with ASan. Arguments after the target reach cargo-fuzz verbatim, so libFuzzer flags follow `--`; pass `-timeout=25` so a hanging input becomes an artifact instead of a stuck run. To reproduce one artifact: `just fuzz <target> <artifact>`.

`just ci-run <target> [FEATURES] [BUDGET]` is what the scheduled run does: it merges the accumulated corpus down to the inputs that increase coverage, re-seeds, then fuzzes for `BUDGET` seconds (900 by default) with `-timeout=25 -print_final_stats=1`. Run it locally before blaming a scheduled failure on the target.

## Where the inputs come from

Hand-written seeds are committed under `fuzz/seeds/<target>/`; the runtime corpus grows in `fuzz/corpus/<target>/`, which is local state and gitignored. `just gen-corpus [--features minio]` regenerates the `xml_bodies` seeds and validates each one, so a generator change belongs in the same commit as the seeds it rewrites. The second codegen variant needs the feature flag, and a target that exists in two variants is alternated by the scheduled rotation rather than fuzzed twice in one day.

## Adding a target

A target is a file under `fuzz/fuzz_targets/` with a `fuzz_target!` entry point, driving the public API the way a test would. The roster comes from the fuzz manifest, so a new target joins the daily rotation without anyone editing a schedule; check `cargo run -p xtask -- fuzz plan` to see the day it lands on.

## Reading the scheduled run

The daily workflow runs `cargo run -p xtask -- fuzz run` inside a fifteen-minute budget, one target per day, derived from the manifest. A failure is only interesting with its artifact: copy it out of the corpus, reproduce it locally, fix the cause, then keep the smallest failing input as a seed, and let `just check-seeds` guard it where that target has a checker.

## What CI cannot show

A pull request runs `fuzz-check`, which builds the targets and validates the seeds but never fuzzes; only the daily run explores. A change that only a target covers stays unverified until that target has had its turn, and saying so is better than implying coverage.
