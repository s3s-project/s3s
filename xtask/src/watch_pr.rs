// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

//! Watch a pull request until something decision-relevant changes.
//!
//! One tick prints a state line, appends it to a log when it differs from the
//! last logged line, and restores its baseline from that log, so a restart does
//! not miss a change. The watcher exits on a
//! material change, on the merge or close transition, or when its time budget
//! runs out; `--keep-running` turns the first case into "keep going".
//!
//! Check churn is deliberately quiet: a pull request that only changes the
//! number of pending checks, or that flaps between `BLOCKED` and `UNSTABLE`,
//! is not worth waking anybody for. A new failure, a settled check set, a
//! review, a label, a review request, a queue entry and the merge or close
//! transition are.

use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::process::Command;
use std::thread::sleep;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use clap::Parser;
use jiff::{Timestamp, Unit};
use serde::Deserialize;

/// Keys of a state line, in the order they are rendered.
const FIELDS: [&str; 15] = [
    "state", "draft", "merge", "auto", "base", "head", "review", "reviews", "comments", "labels", "reqs", "checks", "fail",
    "title", "queue",
];

/// Ticks between two heartbeat lines in the log.
const HEARTBEAT_TICKS: u64 = 20;

/// Seconds between two ticks. Fixed on purpose: a stretched interval only delays
/// the moment a change is noticed.
const TICK: Duration = Duration::from_secs(60);

/// Consecutive snapshot failures tolerated before the watcher gives up.
const MAX_CONSECUTIVE_FAILURES: u32 = 3;

/// Merge states that only mean "still waiting"; flapping between them is noise.
const QUIET_MERGE: [&str; 2] = ["BLOCKED", "UNSTABLE"];

/// Conclusions or states that mark a check as failed.
const FAILED: [&str; 6] = [
    "FAILURE",
    "ERROR",
    "TIMED_OUT",
    "CANCELLED",
    "ACTION_REQUIRED",
    "STARTUP_FAILURE",
];

/// The fields the snapshot asks `gh` for.
const VIEW_FIELDS: &str = "state,isDraft,mergeStateStatus,autoMergeRequest,baseRefName,headRefOid,reviewDecision,reviews,comments,labels,reviewRequests,statusCheckRollup,title";

/// Watch a pull request until its state changes.
#[derive(Debug, Parser)]
pub(crate) struct WatchPr {
    /// Pull request number.
    #[arg(value_name = "PR")]
    pr: u64,
    /// Stop after this many hours.
    #[arg(long, value_name = "HOURS", default_value_t = 24)]
    hours: u64,
    /// Keep watching after a change instead of exiting.
    #[arg(long)]
    keep_running: bool,
    /// Where to append the state lines; defaults to `target/pr-watch/pr-<n>.log`.
    #[arg(long, value_name = "PATH")]
    log: Option<PathBuf>,
}

impl WatchPr {
    pub(crate) fn run(self) -> Result<bool> {
        let log = self.log_path();
        if let Some(parent) = log.parent().filter(|parent| !parent.as_os_str().is_empty()) {
            fs::create_dir_all(parent).with_context(|| format!("failed to create {}", parent.display()))?;
        }
        let queue = Repository::resolve();
        let mut baseline = last_logged_line(&log);
        let deadline = Instant::now() + Duration::from_secs(self.hours.saturating_mul(3600));
        let mut failures = 0_u32;
        let mut ticks = 0_u64;
        loop {
            ticks += 1;
            match snapshot(self.pr, queue.as_ref()) {
                Ok(snapshot) => {
                    failures = 0;
                    let line = snapshot.render();
                    emit(&line);
                    let changed = material_change(baseline.as_deref(), &line);
                    if changed || ticks.is_multiple_of(HEARTBEAT_TICKS) {
                        append(&log, &line)?;
                    }
                    if snapshot.terminal() {
                        return Ok(true);
                    }
                    if changed && !self.keep_running {
                        return Ok(true);
                    }
                    // A change always moves the baseline, so a watcher that keeps
                    // running after one change does not report it again on every tick.
                    baseline = next_baseline(baseline, line, changed);
                }
                Err(error) => {
                    failures += 1;
                    emit(&format!("snapshot failed ({failures}/{MAX_CONSECUTIVE_FAILURES}): {error:#}"));
                    if failures >= MAX_CONSECUTIVE_FAILURES {
                        return Ok(false);
                    }
                }
            }
            if Instant::now() >= deadline {
                emit("time budget exhausted");
                return Ok(true);
            }
            sleep(TICK);
        }
    }

    /// Log file of this watcher.
    fn log_path(&self) -> PathBuf {
        self.log
            .clone()
            .unwrap_or_else(|| PathBuf::from(format!("target/pr-watch/pr-{}.log", self.pr)))
    }
}

/// The repository the pull request lives in, for the merge-queue lookup.
#[derive(Debug, Clone)]
struct Repository {
    owner: String,
    name: String,
}

impl Repository {
    /// Resolve the current repository; the queue lookup is skipped when it fails.
    fn resolve() -> Option<Self> {
        let name_with_owner = run_gh(&["repo", "view", "--json", "nameWithOwner", "--jq", ".nameWithOwner"]).ok()?;
        let (owner, name) = name_with_owner.trim().split_once('/')?;
        Some(Self {
            owner: owner.to_owned(),
            name: name.to_owned(),
        })
    }
}

/// One state line, keyed by [`FIELDS`].
#[derive(Debug, Clone, Default)]
struct Snapshot {
    fields: Vec<(&'static str, String)>,
}

impl Snapshot {
    fn render(&self) -> String {
        self.fields
            .iter()
            .map(|(key, value)| format!("{key}={value}"))
            .collect::<Vec<_>>()
            .join(" | ")
    }

    fn get(&self, key: &str) -> Option<&str> {
        self.fields
            .iter()
            .find(|(name, _)| *name == key)
            .map(|(_, value)| value.as_str())
    }

    /// Whether the pull request reached a terminal state.
    fn terminal(&self) -> bool {
        matches!(self.get("state"), Some("MERGED" | "CLOSED"))
    }
}

/// What `gh pr view` returns, trimmed to the fields the watcher reads.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PrView {
    state: String,
    is_draft: bool,
    #[serde(default)]
    merge_state_status: Option<String>,
    #[serde(default)]
    auto_merge_request: Option<serde_json::Value>,
    base_ref_name: String,
    head_ref_oid: String,
    #[serde(default)]
    review_decision: String,
    #[serde(default)]
    reviews: Vec<Review>,
    #[serde(default)]
    comments: Vec<Comment>,
    #[serde(default)]
    labels: Vec<Label>,
    #[serde(default)]
    review_requests: Vec<serde_json::Value>,
    #[serde(default)]
    status_check_rollup: Vec<Check>,
    title: String,
}

#[derive(Debug, Deserialize)]
struct Review {
    state: String,
    author: Author,
}

#[derive(Debug, Deserialize)]
struct Comment {
    author: Author,
}

#[derive(Debug, Deserialize)]
struct Label {
    name: String,
}

#[derive(Debug, Deserialize)]
struct Author {
    #[serde(default)]
    login: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Check {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    context: Option<String>,
    #[serde(default)]
    conclusion: Option<String>,
    #[serde(default)]
    state: Option<String>,
}

impl PrView {
    /// Render the view as a state line, with the merge-queue entry appended.
    fn snapshot(&self, queue: &str) -> Snapshot {
        let (checks, fail) = checks_summary(&self.status_check_rollup);
        let fields = vec![
            ("state", self.state.clone()),
            ("draft", self.is_draft.to_string()),
            ("merge", self.merge_state_status.clone().unwrap_or_else(|| "-".to_owned())),
            ("auto", if self.auto_merge_request.is_some() { "on" } else { "off" }.to_owned()),
            ("base", self.base_ref_name.clone()),
            ("head", short_sha(&self.head_ref_oid)),
            ("review", non_empty(&self.review_decision)),
            ("reviews", summarize_reviews(&self.reviews)),
            ("comments", summarize_comments(&self.comments)),
            ("labels", summarize_labels(&self.labels)),
            ("reqs", self.review_requests.len().to_string()),
            ("checks", checks),
            ("fail", fail),
            ("title", self.title.clone()),
            ("queue", queue.to_owned()),
        ];
        debug_assert_eq!(fields.len(), FIELDS.len());
        Snapshot { fields }
    }
}

/// Read one snapshot of the pull request.
fn snapshot(pr: u64, queue: Option<&Repository>) -> Result<Snapshot> {
    let raw = run_gh(&["pr", "view", &pr.to_string(), "--json", VIEW_FIELDS])?;
    let view: PrView = serde_json::from_str(&raw).context("`gh pr view` returned an unexpected payload")?;
    let entry = queue
        .and_then(|repository| merge_queue_entry(repository, pr))
        .unwrap_or_else(|| "-".to_owned());
    Ok(view.snapshot(&entry))
}

/// Best-effort merge-queue entry: `QUEUED/1` while it waits, `-` otherwise.
fn merge_queue_entry(repository: &Repository, pr: u64) -> Option<String> {
    let query = "query($o:String!,$r:String!,$n:Int!){repository(owner:$o,name:$r){pullRequest(number:$n){mergeQueueEntry{state position}}}}";
    let raw = run_gh(&[
        "api",
        "graphql",
        "-f",
        &format!("query={query}"),
        "-F",
        &format!("o={}", repository.owner),
        "-F",
        &format!("r={}", repository.name),
        "-F",
        &format!("n={pr}"),
    ])
    .ok()?;
    let value: serde_json::Value = serde_json::from_str(&raw).ok()?;
    let entry = value
        .get("data")?
        .get("repository")?
        .get("pullRequest")?
        .get("mergeQueueEntry")?;
    if entry.is_null() {
        return None;
    }
    let state = entry.get("state").and_then(serde_json::Value::as_str).unwrap_or("-");
    let position = entry.get("position").map_or_else(|| "-".to_owned(), ToString::to_string);
    Some(format!("{state}/{position}"))
}

/// Count the check conclusions and name the first failing check.
fn checks_summary(checks: &[Check]) -> (String, String) {
    let mut counts: BTreeMap<String, u32> = BTreeMap::new();
    let mut failing: Option<String> = None;
    for check in checks {
        // A pending check reports an empty conclusion, so fall through to the
        // state and, when that is empty as well, to an unknown marker.
        let key = non_empty_owned(check.conclusion.as_deref())
            .or_else(|| non_empty_owned(check.state.as_deref()))
            .unwrap_or_else(|| "?".to_owned());
        *counts.entry(key.clone()).or_default() += 1;
        if failing.is_none() && FAILED.contains(&key.as_str()) {
            failing = Some(
                check
                    .name
                    .clone()
                    .or_else(|| check.context.clone())
                    .unwrap_or_else(|| "(unnamed)".to_owned()),
            );
        }
    }
    if counts.is_empty() {
        return ("-".to_owned(), "-".to_owned());
    }
    let summary = counts
        .iter()
        .map(|(key, count)| format!("{key}:{count}"))
        .collect::<Vec<_>>()
        .join(",");
    (summary, failing.unwrap_or_else(|| "-".to_owned()))
}

/// `count(state/author)` of the reviews, or `0(-/-)`.
fn summarize_reviews(reviews: &[Review]) -> String {
    match reviews.last() {
        Some(review) => format!("{}({}/{})", reviews.len(), review.state, review.author.login.as_deref().unwrap_or("-")),
        None => "0(-/-)".to_owned(),
    }
}

/// `count(last author)` of the comments, or `0(-)`.
fn summarize_comments(comments: &[Comment]) -> String {
    match comments.last() {
        Some(comment) => format!("{}({})", comments.len(), comment.author.login.as_deref().unwrap_or("-")),
        None => "0(-)".to_owned(),
    }
}

/// Comma-separated labels, or `-`.
fn summarize_labels(labels: &[Label]) -> String {
    if labels.is_empty() {
        return "-".to_owned();
    }
    labels.iter().map(|label| label.name.clone()).collect::<Vec<_>>().join(",")
}

/// The first seven characters of a commit id.
fn short_sha(sha: &str) -> String {
    sha.chars().take(7).collect()
}

/// `value`, or `-` when it is empty.
fn non_empty(value: &str) -> String {
    if value.is_empty() { "-".to_owned() } else { value.to_owned() }
}

/// The baseline for the next tick: the current line once it differs from the
/// baseline, and the baseline itself otherwise.
fn next_baseline(baseline: Option<String>, line: String, changed: bool) -> Option<String> {
    if changed || baseline.is_none() { Some(line) } else { baseline }
}

/// `Some(value)` when the field carries text, `None` when it is absent or empty.
fn non_empty_owned(value: Option<&str>) -> Option<String> {
    value.filter(|value| !value.is_empty()).map(ToOwned::to_owned)
}

/// Whether a change between two state lines deserves attention.
fn material_change(previous: Option<&str>, current: &str) -> bool {
    let Some(previous) = previous else {
        return true;
    };
    let old = parse_line(previous);
    let new = parse_line(current);
    for key in FIELDS {
        if old.get(key) == new.get(key) {
            continue;
        }
        match key {
            // Check counts churn on every tick; only a failure or a settled set counts.
            "checks" => {
                if new.get("fail").is_some_and(|fail| fail.as_str() != "-") {
                    return true;
                }
                if !new.get("checks").is_some_and(|checks| checks.contains('?')) {
                    return true;
                }
            }
            // Waiting states flap between each other while the queue is busy.
            "merge" => {
                let new_merge = new.get("merge").map(String::as_str);
                let old_merge = old.get("merge").map(String::as_str);
                if is_quiet_merge(new_merge) && is_quiet_merge(old_merge) {
                    continue;
                }
                return true;
            }
            // A comment from a bot is reporting, not asking.
            "comments" => {
                if new.get("comments").is_some_and(|value| is_bot_author(value)) {
                    continue;
                }
                return true;
            }
            _ => return true,
        }
    }
    false
}

/// Parse a rendered state line back into its fields.
fn parse_line(line: &str) -> BTreeMap<String, String> {
    line.split(" | ")
        .filter_map(|part| part.split_once('='))
        .map(|(key, value)| (key.to_owned(), value.to_owned()))
        .collect()
}

fn is_quiet_merge(value: Option<&str>) -> bool {
    value.is_some_and(|value| QUIET_MERGE.contains(&value))
}

/// Whether a rendered comment field was written by a bot.
fn is_bot_author(value: &str) -> bool {
    let inner = match (value.find('('), value.rfind(')')) {
        (Some(start), Some(end)) if start < end => &value[start + 1..end],
        _ => value,
    };
    let inner = inner.to_lowercase();
    inner.contains("[bot]") || inner.contains("codecov") || inner.contains("copilot")
}

/// Timestamp of a log line, in UTC, to the second.
fn stamp() -> String {
    let now = Timestamp::now();
    // The log is read by people: a whole second is enough, and it keeps the
    // lines comparable with the ones already written.
    let second = now.round(Unit::Second).unwrap_or(now);
    second.to_string()
}

fn emit(text: &str) {
    println!("[{}] {text}", stamp());
}

fn append(path: &PathBuf, line: &str) -> Result<()> {
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("failed to open {}", path.display()))?;
    writeln!(file, "[{}] {line}", stamp()).with_context(|| format!("failed to write {}", path.display()))
}

/// The last state line of the log, used as the baseline of a restarted watcher.
fn last_logged_line(path: &PathBuf) -> Option<String> {
    let text = fs::read_to_string(path).ok()?;
    text.lines()
        .rev()
        .find_map(|line| line.split_once("] ").map(|(_, snapshot)| snapshot.to_owned()))
}

/// Run `gh` and return its stdout.
fn run_gh(args: &[&str]) -> Result<String> {
    let output = Command::new("gh")
        .args(args)
        .output()
        .context("failed to run `gh`; is the GitHub CLI installed?")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("`gh {}` failed: {}", args.join(" "), stderr.trim());
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    const LINE: &str = "state=OPEN | draft=True | merge=BLOCKED | auto=off | base=main | head=ec0a1b1 | review=- | reviews=1(COMMENTED/copilot-pull-request-reviewer) | comments=1(codecov) | labels=- | reqs=0 | checks=?:12,SKIPPED:8,SUCCESS:1 | fail=- | title=a title | queue=-/-";

    fn line(changes: &[(&str, &str)]) -> String {
        let mut fields = parse_line(LINE);
        for (key, value) in changes {
            fields.insert((*key).to_owned(), (*value).to_owned());
        }
        FIELDS
            .iter()
            .map(|key| format!("{key}={}", fields.get(*key).map_or("-", String::as_str)))
            .collect::<Vec<_>>()
            .join(" | ")
    }

    #[test]
    fn a_first_snapshot_is_always_a_change() {
        assert!(material_change(None, LINE));
    }

    #[test]
    fn pending_check_churn_is_not_a_change() {
        assert!(!material_change(Some(LINE), &line(&[("checks", "?:3,SKIPPED:8,SUCCESS:10")])));
    }

    #[test]
    fn a_settled_check_set_is_a_change() {
        assert!(material_change(Some(LINE), &line(&[("checks", "SKIPPED:8,SUCCESS:15")])));
    }

    #[test]
    fn a_failing_check_is_a_change() {
        assert!(material_change(
            Some(LINE),
            &line(&[("checks", "FAILURE:1,SKIPPED:8,SUCCESS:15"), ("fail", "CodeQL")])
        ));
    }

    #[test]
    fn waiting_states_do_not_wake_anybody() {
        assert!(!material_change(Some(LINE), &line(&[("merge", "UNSTABLE")])));
        assert!(material_change(Some(LINE), &line(&[("merge", "CLEAN")])));
    }

    #[test]
    fn a_bot_comment_is_not_a_change() {
        assert!(!material_change(Some(LINE), &line(&[("comments", "2(codecov)")])));
        assert!(!material_change(
            Some(LINE),
            &line(&[("comments", "3(copilot-pull-request-reviewer[bot])")])
        ));
        assert!(material_change(Some(LINE), &line(&[("comments", "2(alice)")])));
    }

    #[test]
    fn reviews_labels_and_state_changes_wake_anybody() {
        assert!(material_change(Some(LINE), &line(&[("reviews", "2(APPROVED/alice)")])));
        assert!(material_change(Some(LINE), &line(&[("labels", "bug")])));
        assert!(material_change(Some(LINE), &line(&[("state", "MERGED")])));
        assert!(material_change(Some(LINE), &line(&[("queue", "QUEUED/1")])));
    }

    #[test]
    fn a_rendered_line_round_trips() {
        let fields = parse_line(LINE);
        assert_eq!(fields.get("state").map(String::as_str), Some("OPEN"));
        assert_eq!(fields.get("queue").map(String::as_str), Some("-/-"));
        assert_eq!(fields.len(), FIELDS.len());
        assert_eq!(line(&[]), LINE);
    }

    #[test]
    fn the_check_summary_counts_and_names_the_failure() {
        let checks = vec![
            Check {
                name: Some("rust (stable)".to_owned()),
                context: None,
                conclusion: Some("SUCCESS".to_owned()),
                state: None,
            },
            Check {
                name: Some("CodeQL".to_owned()),
                context: None,
                conclusion: Some("FAILURE".to_owned()),
                state: None,
            },
            Check {
                name: None,
                context: Some("status-check".to_owned()),
                conclusion: None,
                state: Some("PENDING".to_owned()),
            },
        ];
        assert_eq!(checks_summary(&checks), ("FAILURE:1,PENDING:1,SUCCESS:1".to_owned(), "CodeQL".to_owned()));
        assert_eq!(checks_summary(&[]), ("-".to_owned(), "-".to_owned()));
    }

    #[test]
    fn a_pending_check_does_not_render_an_empty_key() {
        // `gh` reports an empty conclusion while a check is pending, and an
        // empty state for a check run that has not started.
        let checks = vec![
            Check {
                name: Some("rust (stable)".to_owned()),
                context: None,
                conclusion: Some(String::new()),
                state: Some("PENDING".to_owned()),
            },
            Check {
                name: Some("skip-check".to_owned()),
                context: None,
                conclusion: Some(String::new()),
                state: Some(String::new()),
            },
        ];
        assert_eq!(checks_summary(&checks), ("?:1,PENDING:1".to_owned(), "-".to_owned()));
    }

    #[test]
    fn a_view_renders_the_expected_line() {
        let raw = r#"{"state":"OPEN","isDraft":false,"mergeStateStatus":"UNSTABLE","autoMergeRequest":null,
            "baseRefName":"main","headRefOid":"ec0a1b1ebe25ac5ef64f1d2202868a9c66ce71d5","reviewDecision":"",
            "reviews":[{"state":"COMMENTED","author":{"login":"copilot-pull-request-reviewer"}}],
            "comments":[{"author":{"login":"codecov"}}],"labels":[{"name":"tests"}],
            "reviewRequests":[{"login":"alice"}],
            "statusCheckRollup":[{"name":"coverage","conclusion":"SUCCESS","state":"COMPLETED"},
            {"name":"rust","conclusion":null,"state":"IN_PROGRESS"}],"title":"a title"}"#;
        let view: PrView = serde_json::from_str(raw).expect("the fixture parses");
        let rendered = view.snapshot("QUEUED/1").render();
        assert_eq!(
            rendered,
            "state=OPEN | draft=false | merge=UNSTABLE | auto=off | base=main | head=ec0a1b1 | review=- | reviews=1(COMMENTED/copilot-pull-request-reviewer) | comments=1(codecov) | labels=tests | reqs=1 | checks=IN_PROGRESS:1,SUCCESS:1 | fail=- | title=a title | queue=QUEUED/1"
        );
    }

    #[test]
    fn a_change_moves_the_baseline_in_keep_running_mode() {
        // Without this, a watcher that keeps running reports the same change on
        // every tick and keeps appending the line to its log.
        assert_eq!(next_baseline(None, "first".to_owned(), true), Some("first".to_owned()));
        assert_eq!(
            next_baseline(Some("first".to_owned()), "second".to_owned(), true),
            Some("second".to_owned())
        );
        assert_eq!(
            next_baseline(Some("second".to_owned()), "second".to_owned(), false),
            Some("second".to_owned())
        );
    }

    #[test]
    fn terminal_states_are_recognized() {
        let view = PrView {
            state: "MERGED".to_owned(),
            is_draft: false,
            merge_state_status: None,
            auto_merge_request: None,
            base_ref_name: "main".to_owned(),
            head_ref_oid: "abc".to_owned(),
            review_decision: String::new(),
            reviews: Vec::new(),
            comments: Vec::new(),
            labels: Vec::new(),
            review_requests: Vec::new(),
            status_check_rollup: Vec::new(),
            title: "t".to_owned(),
        };
        assert!(view.snapshot("-").terminal());
    }
}
