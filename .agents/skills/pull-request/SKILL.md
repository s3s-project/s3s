---

name: "pull-request"
description: Prepare a change for this repository as a pull request. Use when a branch is ready to publish, when writing the commit series or the pull request description, or when a queued pull request needs follow-up.
license: "Apache-2.0"
---

# Pull request

A pull request here is squash-merged into `main`, so its title becomes the commit subject on `main` and its body is the only narrative a later reader gets. Prepare both with the same care as the diff.

## Before publishing

- Run the gates on the tree that will be pushed: `just dev`, `just ci-rust`, and the nightly clippy line CI uses. `just ci-rust` ends with `assert_unchanged`, so a codegen run that is not committed fails the gate; keep `just codegen` idempotent. CI runs `just ci-rust` under both a stable and a nightly toolchain, so the nightly half has to be run locally as `cargo +nightly clippy --workspace --all-targets --all-features -- -D warnings`.
- Run `just ci-python` for Python changes, `just spdx-check` for new files, and `just link-license-check` when the set of published crates changes. A skill lives in `.agents/skills/<name>/SKILL.md`, is symlinked from `.github/skills/<name>`, and carries `license: "Apache-2.0"` in its frontmatter instead of an SPDX header.
- A change that touches a feature gate is also checked in `s3s-fs`: the default build must not pull `quinn` or `h3`, and `--features http3` must.
- Collect the evidence the body will cite: commands, exit codes, counts, and the source of every number (a gate, a measurement, a parity check).
- Keep one logical change per branch. A second concern rides in a second pull request, not in a second commit series.

## Branch and history

- Name the branch `<type>/<slug>` with the same type the commits use: `feat/`, `fix/`, `docs/`, `test/`, `refactor/`, `chore/`.
- Branch from `main`, or from the branch the change builds on, and keep the history linear: the `main` ruleset requires linear history and the merge method is squash, so merge commits and "fix review" commits only enlarge the reviewer's diff.
- Fold a fix into the commit that owns the file and push with `--force-with-lease`. Rewriting your own unmerged branch is normal here; rewriting a branch someone else reviewed warrants a comment that says what moved.

## Commits

- Follow [Conventional Commits](https://www.conventionalcommits.org/en/v1.0.0/); the `Git` section of `CONTRIBUTING.md` points there.
- Scope with the crate or the area the change belongs to (`s3s`, `s3s-fs`, `codegen`, `xtask`, `ci`, `deps`, `release`, ...).
- The subject states the change; the body states why it is needed and what it makes true. Do not narrate the iterations that led to it.
- Mark a change that alters existing behaviour with `!` after the type or scope (`feat(s3s)!:`) and describe the migration under `### Behavior`.
- Add `Assisted-by: <tool>:<model>` when a tool wrote a substantive part of the change, and never add `Signed-off-by` on someone else's behalf; the [AI Contribution Policy](https://github.com/s3s-project/.github/blob/main/AI_POLICY.md) governs both.

## Title and body

- **Title**: the subject of the squash commit, without a `(#N)` suffix — GitHub appends the number when it merges.
- **Body**: use these sections in this order and drop the ones that do not apply.
  - `### Summary` — one paragraph: the problem, the change, and the number that shows the effect.
  - `### Changes` — bullets, one per behavioural unit rather than one per file.
  - `### Behavior` — what changes for a user or a caller, including deliberate breakages.
  - `### Tests` — what ran, with exit codes or counts, plus any parity check or red control that shows the tests can fail.
  - `### References` — the issues, upstream discussions or earlier pull requests the change answers. Write `Part of #N` while the issue still tracks other work: `Fixes` and `Closes` close it on merge.
  - `### Dependencies` — what the change adds, removes or bumps, and anything the release order depends on.
  - `### AI` — the tool and model that took part, what they did, and that a human reviewed the diff and owns the change.
- Open the pull request as a **draft**: the `main` ruleset runs a Copilot code review on drafts too. Convert it to ready when the checks are green and the description is complete.

## What the checks will run

- `CI` runs on `pull_request` against `main` and `feat/**`, on `merge_group`, on a weekly schedule and on manual dispatch. `skip-check` is configured with `paths_ignore: '["*.md"]'`, and `status-check` aggregates the jobs; that aggregate is the only check the ruleset requires.
- A pull request runs `python`, `rust-msrv`, `rust` (stable and nightly; the nightly leg is `continue-on-error`), `cross-test` (macOS and Windows), `wasm-test` and `coverage`.
- The seven `e2e` jobs (`mint`, `s3-tests`, `boto3`, `rclone`, `s3s-e2e` against MinIO or `s3s-fs`) carry `if: github.event_name != 'pull_request'`: they run in the merge queue and on the schedule, never on the pull request. A pull request with every visible check green can therefore still fail the queue, so run the affected script under `scripts/` before queueing it.
- `Audit` (dependency and license policy) runs when a manifest, the lockfile or `.cargo/` changes; `Fuzz` when `fuzz/`, `crates/` or its own workflow changes — on a pull request that is `fuzz-check` alone, because `fuzz-run` carries the same `!= 'pull_request'` gate and fuzzes one rotating target per week on the schedule; `Semver` runs on every pull request and merge group.
- Code scanning uses GitHub's default setup. A very large diff can make it attribute pre-existing alerts to the pull request, and that check is not required: read `code-scanning/alerts` before treating it as a finding.

## Landing

- `main` takes pull requests through the merge queue with the squash method; the queue re-runs `CI`, `Audit` and `Semver` on `merge_group`, and that is where the `e2e` jobs run for the first time. A queued pull request that fails the queue is dequeued rather than merged, so read the `merge_group` run before queueing it again.
- Merging does not delete the head branch, so clean up after it: once the pull request is merged, delete the head branch (remote and local) and remove the worktree instead of leaving a stale checkout behind.

## Working with a review

- Treat a review comment as a question to settle, not a verdict to obey: answer with the artifact that settles it (a command, a file, a test) and state what changed.
- Reply on the line that was questioned, and say what changed.
- Either amend the commit that owns the change or append a commit that answers the review; both are accepted here. Pick the one that leaves the history easiest to read and mention which one you used.

## Watching a pull request

Hang `cargo run -p xtask -- watch-pr <PR#>` in the background while a pull request is open, read its log when it exits and hang it again; the `watch-pr` skill covers the state line, the exit conditions, the restart discipline, and how to wait for checks alone with `gh`.

## The public layer

Everything committed here, and everything pasted into a pull request, a review or a log, is public. Strip local paths, internal notes, tool- or machine-specific references, and personal data before they leave the machine, and keep a published tool self-contained: no dependency on a local checkout, no absolute path, no private context. A script that is brought into the repository is rewritten for that audience rather than copied.

## What needs an explicit instruction

Pushing a branch, opening, undrafting, queueing or merging a pull request, dismissing a scanning alert, changing repository settings, deleting branches, and force-pushing a branch somebody else owns are actions to take only when the person responsible asks for them.
