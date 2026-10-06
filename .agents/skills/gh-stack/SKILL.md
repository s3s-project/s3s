---

name: "gh-stack"
description: Work with stacked pull requests in this repository using gh stack. Use when a change is split across branches that build on each other, when a stack must be rebased, synced, pushed or merged, or when a force-push to a stack branch has to be safe.
license: "Apache-2.0"
---

# Stacked pull requests

A stack is a chain of branches where each layer is based on the one below it, `main ← layer-1 ← layer-2`, and each layer is its own pull request. This repository enables GitHub's stacked pull requests, and the `gh stack` command manages them: it keeps the local branches, the remote branches, the pull requests and the GitHub stack object in step.

`gh stack` is an extension, not a core `gh` command, so install it once and confirm it is there:

```bash
gh extension install github/gh-stack
gh extension list
```

Reach for a stack when a change is genuinely layered: one logical change per layer, each one reviewable on its own, with the lower layers merging first. Read the `pull-request` skill for the commit series, title and body of each layer; this skill covers the stack around them.

## The local map

```bash
gh stack init layer-1 layer-2      # adopt existing branches bottom to top, or create missing ones
gh stack view                      # stack number, layers, PR links, current branch
gh stack switch                    # move between layers
gh stack checkout <branch|PR|URL>  # jump to a stack by number, PR, URL or branch
gh stack add layer-3               # branch on top of the current top, then commit
gh stack modify                    # restructure interactively
```

`init` adopts existing branches without rewriting them: the branch names and commits stay, only the relationship is recorded. When a layer changes, the layers above it need a rebase (below).

## Publishing

```bash
gh stack submit --auto --remote <name>   # push every layer, open the missing PRs
gh stack push --remote <name>            # push the branches only
gh stack rebase                          # fetch and cascade-rebase locally, no push
gh stack sync                            # fetch, reconcile, rebase, push atomically, re-link the stack
```

- `submit` without `--auto` opens an editor; with `--auto` it skips it. **`--auto` creates new pull requests as drafts** and `--open` marks them ready. The `main` ruleset runs Copilot review on drafts too (`review_draft_pull_requests: true`, `review_on_push: false`), so a draft gets a review but an amend does not trigger a second one.
- An `--auto` title is **not** always the commit subject: a layer whose branch name is not a Conventional Commit subject gets a title built from the branch name. Check every layer (`gh pr view <n> --json title,isDraft,baseRefName`) and correct it with `gh pr edit` before asking for review.
- `sync` is the safe way to reconcile: it pulls down branches that were added to the stack on GitHub, fast-forwards the trunk, cascade-rebases and pushes atomically. It prompts when local and remote diverge, which is the moment to stop and look rather than push.

## When the remote cannot be chosen

A checkout can carry more than one remote, and `gh stack` then refuses to pick one: `multiple remotes configured; set remote.pushDefault or use an interactive terminal`. Name the remote where the command takes one — `gh stack submit --remote <name>`, `gh stack push --remote <name>` — and for `rebase` and `sync`, which have no such flag, inject the setting for that single command rather than changing the repository configuration:

```bash
GIT_CONFIG_COUNT=1 GIT_CONFIG_KEY_0=remote.pushDefault GIT_CONFIG_VALUE_0=<name> gh stack rebase
```

## Never overwrite a rebase you did not make

`gh stack push` force-pushes each layer with `--force-with-lease` computed from the stack's own view, so it can replace work that moved the branch elsewhere — including the GitHub **Rebase stack** button, which rebases on the server and force-pushes both layers. The lease does not save you, because the stack refreshes what it considers the expected remote state.

Before any push, compare the three heads:

```bash
git fetch <remote>
git rev-parse --short HEAD <remote>/<branch>
gh pr view <n> --json headRefOid --jq .headRefOid
```

When the remote head is not what you last pushed, stop and reconcile (`gh stack sync`, or a plain `git reset --hard <remote>/<branch>` when you simply want to adopt the remote) instead of pushing over it. Keep a backup ref before any rewrite: `git branch backup/<what> <sha>`. And run `gh stack` from one worktree at a time: two sessions inside one stack each see a different "local" state.

## Landing

`main` takes pull requests through a merge queue with the squash method (minimum one entry, a ten-minute wait, all green). The ruleset requires only the `status-check` context, does **not** require branches to be up to date (`strict_required_status_checks_policy: false`), and requires linear history, so a stack does not have to be rebased onto the newest `main` to be mergeable.

- Merge from the bottom. When layer 1 lands, GitHub retargets layer 2's base to `main`; merge it next. `gh stack merge` merges the layers in order.
- The GitHub UI offers **Rebase stack** and **Enqueue stack**; the CLI equivalents are `gh stack rebase` followed by `gh stack push`, and enqueueing the bottom pull request.
- CI runs on `pull_request` and again on `merge_group`: the queue re-runs the required checks, so read the `merge_group` run before treating a queue failure as a code failure.
- After the last layer merges, delete the head branches (remote and local) and remove the worktree. A fully merged stack needs no `gh stack unstack`; that command is for abandoning or restructuring a stack whose layers will not merge in order.

## Watching a stack

```bash
gh stack view                                  # the map and the PR links
gh pr checks <n>                               # one layer at a time; the stack has no single check set
cargo run -p xtask -- watch-pr <n>             # the state that decides landing, with a restorable baseline
gh pr view <n> --json state,mergeStateStatus,mergedAt
```

## Where to look

- `gh stack --help` and `gh stack <command> --help` describe the installed version, which is the version that counts.
- The upstream command lives in github/gh-stack: <https://github.com/github/gh-stack>.
- GitHub's own documentation for stacked pull requests: <https://docs.github.com/en/pull-requests/how-tos/stacked-pull-requests>.
