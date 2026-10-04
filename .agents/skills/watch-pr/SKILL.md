---

name: "watch-pr"
description: Follow an open pull request with xtask watch-pr. Use when a pull request has to be watched until it merges or closes, when a watcher exits and has to be restarted from its log, or when deciding between watching the pull request and waiting for its checks.
license: "Apache-2.0"
---

# Watching a pull request

`cargo run -p xtask -- watch-pr <PR#>` follows the state that decides whether a pull request can land. Hang it in the background when a pull request opens, read its log when it exits, and hang it again: a zero exit is the signal, and a non-zero exit means the watcher itself failed.

## What it watches

Each tick prints one state line; the line is appended to `target/pr-watch/pr-<n>.log` when it differs from the last logged one, once when the log is empty, and at a heartbeat every twenty ticks (`--log` moves the file). The line covers `state`, `draft`, `mergeStateStatus`, auto-merge, base and head, the review decision, review and comment counts, labels, review requests, the check summary with the names of failing checks, the title, and the merge-queue entry.

## When it exits

It exits when something decision-relevant changes, when the pull request merges or closes, or when its twelve-hour budget runs out. Restarting it after each exit keeps the coverage continuous: the baseline is restored from the last line of the log, so a restart does not re-report a change that was already read, and it does not miss one that happened while nothing was watching.

A zero exit means one of those three; a non-zero exit means the snapshot failed three times in a row, usually because `gh` is unavailable. The failure lines are printed to stdout rather than appended to the log, so read them before restarting.

It deliberately ignores the churn that would wake somebody for nothing: check counts moving while the set is still running, a flap between `BLOCKED` and `UNSTABLE`, and comments written by a bot. A new failure, a settled check set, a review, a label, a review request, a merge-queue entry and the merge or close transition all count.

Keep one watcher per pull request, and hang the next one after every exit; the twelve-hour budget means a pull request that stays open needs a new watcher every twelve hours, so a quiet one takes two hangs a day.

## Reading the log

Read the log at natural nodes rather than polling it: when a watcher exits, when a review arrives, when the checks settle. Report what changed, then hang the next watcher with the last line as its baseline. When the pull request merges or closes, keep the terminal line in the same log, stop watching, and continue with the landing steps in the `pull-request` skill — the merge does not delete the head branch and `main` may have moved.

## Waiting for checks instead

`watch-pr` covers the pull request as a whole. When the checks, or a single run, are the whole question, `gh` watches them directly:

- `gh pr checks <PR> --watch --interval 60` waits for the checks of a pull request (exit code 0 when they pass, non-zero when one fails, 8 while the set is pending).
- `gh run watch <run-id> --exit-status` follows one workflow run.

`gh pr checks --watch` refreshes every ten seconds by default, so pass 60 to match the pace used here. Reach for `watch-pr` when the draft state, the reviews, the labels, the review requests or the merge-queue entry matter: it keeps a baseline across restarts, which the built-in commands do not.
