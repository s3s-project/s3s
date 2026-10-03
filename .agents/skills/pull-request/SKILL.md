---

name: "pull-request"
description: Prepare a change for this repository as a pull request. Use when a branch is ready to publish, when writing the commit series or the pull request description, or when a queued pull request needs follow-up.
license: "Apache-2.0"
---

# Pull request

A pull request here is squash-merged into `main`, so its title becomes the commit subject on `main` and its body is the only narrative a later reader gets. Prepare both with the same care as the diff.

## Before publishing

- Run the gates on the tree that will be pushed: `just dev`, `just ci-rust`, and the nightly clippy line CI uses. `just ci-rust` ends with `assert_unchanged`, so a codegen run that is not committed fails the gate; keep `just codegen` idempotent.
- Run `just ci-python` for Python changes and `cargo run -p xtask -- spdx check` for new files. A skill lives in `.agents/skills/<name>/SKILL.md`, is linked from `.github/skills/<name>`, and carries `license: "Apache-2.0"` in its frontmatter instead of an SPDX header.
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
- Add `Assisted-by: <tool>:<model>` when a tool wrote a substantive part of the change, and never add `Signed-off-by` on someone else's behalf; the [AI Contribution Policy](https://github.com/s3s-project/.github/blob/main/AI_POLICY.md) governs both.

## Title and body

- **Title**: the subject of the squash commit, without a `(#N)` suffix — GitHub appends the number when it merges.
- **Body**: use these sections in this order and drop the ones that do not apply.
  - `### Summary` — one paragraph: the problem, the change, and the number that shows the effect.
  - `### Changes` — bullets, one per behavioural unit rather than one per file.
  - `### Behavior` — what changes for a user or a caller, including deliberate breakages.
  - `### Tests` — what ran, with exit codes or counts, plus any parity check or red control that shows the tests can fail.
  - `### References` — the issues, upstream discussions or earlier pull requests the change answers.
  - `### AI Disclosure` — the tool and model that took part, what they did, and that a human reviewed the diff and owns the change.
- Open the pull request as a **draft**: the `main` ruleset runs a Copilot code review on drafts too. Convert it to ready when the checks are green and the description is complete.

## What the checks will run

- `CI` runs on `pull_request` and on `merge_group`: `rust` (stable and nightly), `rust-msrv`, `cross-test` (macOS and Windows), `wasm-test`, `coverage`, `python`, `skip-check`, and the `e2e` jobs, which are filtered by the paths the pull request touches. `status-check` aggregates them and is the only check the ruleset requires.
- `Audit` (dependency and license policy) and `Fuzz` (`fuzz-check`, plus `fuzz-run` when the fuzz paths change) are path-filtered; `Semver` runs on `pull_request` and `merge_group`.
- Code scanning uses GitHub's default setup. A very large diff can make it attribute pre-existing alerts to the pull request, and that check is not required: read `code-scanning/alerts` before treating it as a finding.

## Landing

- `main` takes pull requests through the merge queue with the squash method; the queue re-runs `CI`, `Audit` and `Semver` on `merge_group`. A queued pull request that fails the queue is dequeued rather than merged, so read the `merge_group` run before queueing it again.
- Merging does not delete the head branch, so clean up after it: once the pull request is merged, delete the head branch (remote and local) and remove the worktree instead of leaving a stale checkout behind.

## Working with a review

- Treat a review comment as a question to settle, not a verdict to obey: answer with the artifact that settles it (a command, a file, a test) and state what changed.
- Reply on the line that was questioned, and say what changed.
- Either amend the commit that owns the change or append a commit that answers the review; both are accepted here. Pick the one that leaves the history easiest to read and mention which one you used.

## Watching a pull request

`cargo run -p xtask -- watch-pr <PR#>` polls a pull request, prints one state line per tick, appends it to `target/pr-watch/pr-<n>.log`, and takes the last line of that log as its baseline, so a watcher that is restarted does not miss a change. It exits when something decision-relevant changes — a new failure, a settled check set, a review, a label, a review request, a merge-queue entry, or the merge or close transition. Check-count churn, a flap between `BLOCKED` and `UNSTABLE`, and comments written by a bot are deliberately ignored. It ticks every 60 seconds — a fixed interval, because a stretched one only delays the moment a change is noticed. `--hours` bounds how long it runs, `--keep-running` keeps it alive across changes, and `--log` moves the log.

## What to reuse

`gh` already watches two things well: `gh pr checks <PR> --watch --interval 60` waits for the checks of a pull request (exit code 0 when they pass, non-zero when one fails, 8 while the set is pending), and `gh run watch <run-id> --exit-status` follows a single workflow run. Reach for those when the checks, or one run, are the whole question; `gh pr checks --watch` refreshes every 10 seconds by default, so pass 60 to match the pace used here.

Watch the pull request itself with `xtask watch-pr`: it covers the state that decides whether a pull request can land — draft, merge state, reviews, labels, review requests and the merge-queue entry — and it keeps a baseline across restarts, which the built-in commands do not.

## The public layer

Everything committed here, and everything pasted into a pull request, a review or a log, is public. Strip local paths, internal notes, tool- or machine-specific references, and personal data before they leave the machine, and keep a published tool self-contained: no dependency on a local checkout, no absolute path, no private context. A script that is brought into the repository is rewritten for that audience rather than copied.

## What needs an explicit instruction

Pushing a branch, opening, undrafting, queueing or merging a pull request, dismissing a scanning alert, changing repository settings, deleting branches, and force-pushing a branch somebody else owns are actions to take only when the person responsible asks for them.
