// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

//! Check a `MinIO` `mint` log (<https://github.com/minio/mint#mint-log-format>)
//! against the expected-failure baselines.
//!
//! Port of `scripts/report-mint.py`: the tables, the output format and the
//! exit codes are kept as they were.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use anyhow::{Context, Result};
use clap::Parser;
use regex::Regex;
use serde::Deserialize;

/// Check a mint log against the expected-failure baselines.
#[derive(Debug, Parser)]
pub(crate) struct Mint {
    /// Path to the mint log, one JSON object per line.
    log: PathBuf,
}

/// One line of the mint log (<https://github.com/minio/mint#mint-log-format>).
#[derive(Debug, Deserialize)]
struct MintLog {
    name: String,
    function: Option<String>,
    status: String,
}

/// Per-suite counters.
#[derive(Debug, Default, Clone, Copy)]
struct Counters {
    pass: usize,
    fail: usize,
    na: usize,
}

/// Per-function gate: only the test functions listed here are allowed to fail,
/// and each entry caps how many times it may fail. Any failure outside the list
/// fails the gate, and every entry must actually run in this mint log (a stale
/// entry after a mint image upgrade is reported instead of being silently
/// ignored).
///
/// Baseline re-recorded on 2026-10-04 against the pinned image in
/// `scripts/mint.env` and the pinned `MinIO` image. What is left is the s3s
/// defects s3s owns, the `MinIO` extensions it does not implement, and the two
/// limits of the proxy or the backend, each mapped to a tracked known issue.
///
/// The run this list is scored against enables the proxy's authentication
/// passthrough: a request carrying a credential the proxy does not know is
/// forwarded verbatim and the backend decides, so the dynamically created user
/// of `mc test_admin_users` and the temporary credentials of the `minio-js`
/// assume-role case are no longer expected to fail.
const EXPECTED_FAILURES: &[(&str, &[(&str, usize)])] = &[
    (
        "aws-sdk-go-v2",
        // Backend: the pinned MinIO ignores `If-Match` on `DeleteObject`, so a
        // delete with a wrong ETag succeeds instead of being rejected.
        // FIXME: https://github.com/minio/mint/blob/master/run/core/aws-sdk-go-v2/main.go#L294
        &[("ConditionalDeleteWithIncorrectETag", 1)],
    ),
    (
        "minio-java",
        // MinIO extension s3s does not implement: `putObjectFanOut()` sends the
        // `x-minio-fanout-list` form field, which the POST policy validation
        // rejects because it is not a policy condition. The case only runs at
        // all because the image stopped sending an unsigned `x-amz-acl` on its
        // presigned PUT.
        //
        // `getObjectAcl()` used to be listed here. Under the `minio` feature the
        // grantee repeats its type as the `<Type>` child element MinIO writes,
        // so the case passes.
        &[("putObjectFanOut()", 1)],
    ),
];

/// The awscli runner uses the full command line as the test function name,
/// including a random bucket name; normalize it so the name is stable across
/// runs.
static AWS_CLI_BUCKET: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"awscli-mint-test-bucket-\d+").expect("constant regex"));

impl Mint {
    pub(crate) fn run(self) -> Result<bool> {
        let logs = parse_log(&self.log)?;
        let counts = counters(&logs);

        for name in &counts.order {
            print_counter(name, counts.by_name[name]);
        }
        println!();

        let totals = counts.total();
        print_counter("summary", totals);

        // Both gates run to completion so every violation is reported; the exit
        // code is decided afterwards.
        let mut errors = Vec::new();
        check_counters(&counts.by_name, &mut errors);
        check_gate(&logs, &mut errors);

        if errors.is_empty() {
            return Ok(true);
        }

        println!();
        println!("mint gate check failed:");
        for error in &errors {
            println!("  - {error}");
        }
        Ok(false)
    }
}

/// Read the log, skipping the lines that are not JSON objects.
///
/// Unlike the script, a line that parses but lacks a field is reported here
/// instead of aborting the run.
fn parse_log(path: &Path) -> Result<Vec<MintLog>> {
    let text = fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?;
    let mut logs = Vec::new();

    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() {
            continue;
        }

        let json = line.find('{').map_or(line, |index| &line[index..]);
        match serde_json::from_str::<MintLog>(json) {
            Ok(mut entry) => {
                if let Some((name, function)) = entry.name.split_once(':') {
                    let (name, function) = (name.trim().to_owned(), function.trim().to_owned());
                    entry.name = name;
                    entry.function = Some(function);
                }
                logs.push(entry);
            }
            Err(_) => println!("error parsing log line: {line}"),
        }
    }

    Ok(logs)
}

/// Counters per suite, in first-appearance order.
#[derive(Debug, Default)]
struct CountersByName {
    order: Vec<String>,
    by_name: HashMap<String, Counters>,
}

impl CountersByName {
    fn total(&self) -> Counters {
        self.by_name.values().fold(Counters::default(), |total, counter| Counters {
            pass: total.pass + counter.pass,
            fail: total.fail + counter.fail,
            na: total.na + counter.na,
        })
    }
}

/// Count the entries of every suite.
///
/// The script grouped *consecutive* entries (`itertools.groupby`) into a dict
/// keyed by name, so when a name appears in two separate runs only the last run
/// is counted. Mint writes each suite in one block, so this only shows up in
/// hand-edited logs; the port keeps the rule.
fn counters(logs: &[MintLog]) -> CountersByName {
    let mut counts = CountersByName::default();
    let mut run: Option<(String, Counters)> = None;

    for entry in logs {
        match run.as_mut() {
            Some((name, counter)) if *name == entry.name => count_status(counter, &entry.status),
            _ => {
                if let Some((name, counter)) = run.take() {
                    commit_run(&mut counts, name, counter);
                }
                let mut counter = Counters::default();
                count_status(&mut counter, &entry.status);
                run = Some((entry.name.clone(), counter));
            }
        }
    }

    if let Some((name, counter)) = run {
        commit_run(&mut counts, name, counter);
    }

    counts
}

fn count_status(counter: &mut Counters, status: &str) {
    match status {
        "PASS" => counter.pass += 1,
        "FAIL" => counter.fail += 1,
        "NA" => counter.na += 1,
        _ => {}
    }
}

fn commit_run(counts: &mut CountersByName, name: String, counter: Counters) {
    if !counts.by_name.contains_key(&name) {
        counts.order.push(name.clone());
    }
    counts.by_name.insert(name, counter);
}

fn print_counter(name: &str, counter: Counters) {
    println!("{name:<20} passed {:>3}, failed {:>3}, na {:>3}", counter.pass, counter.fail, counter.na);
}

fn normalize_function(name: &str, function: &str) -> String {
    if name == "awscli" {
        return AWS_CLI_BUCKET.replace_all(function, "awscli-mint-test-bucket-N").into_owned();
    }
    function.to_owned()
}

/// Evaluate the group-level counter assertions.
///
/// Both images are pinned by digest (see `scripts/mint.env` and
/// `scripts/minio.env`), so a full run produces the same counts every time and
/// the pass floors below are the counts a full run produces. Their job is to
/// catch a suite that stops early: that is what hid three real failures - the
/// `minio-java` and `aws-sdk-ruby` suites stopped at their first failure until
/// the mint image stopped sending an unsigned `x-amz-acl` on a presigned PUT.
/// A floor that has to be lowered, or a `check_fail_zero` that has to become a
/// count, needs the reason written next to it.
fn check_counters(counts: &HashMap<String, Counters>, errors: &mut Vec<String>) {
    fn check_pass_at_least(counts: &HashMap<String, Counters>, name: &str, minimum: usize, errors: &mut Vec<String>) {
        let pass_count = counts.get(name).map_or(0, |counter| counter.pass);
        if pass_count < minimum {
            errors.push(format!("group counter: \"{name}\" passed {pass_count}, expected at least {minimum}"));
        }
    }

    fn check_fail_zero(counts: &HashMap<String, Counters>, name: &str, errors: &mut Vec<String>) {
        let fail_count = counts.get(name).map_or(0, |counter| counter.fail);
        if fail_count != 0 {
            errors.push(format!("group counter: \"{name}\" failed {fail_count} test(s), expected 0"));
        }
    }

    check_pass_at_least(counts, "aws-sdk-go-v2", 5, errors);
    check_fail_zero(counts, "aws-sdk-php", errors);
    // No known failure is left in this suite.
    check_pass_at_least(counts, "aws-sdk-ruby", 13, errors);
    check_fail_zero(counts, "awscli", errors);
    // `test_admin_users` passes now that the authentication passthrough lets the
    // dynamically created user reach the backend.
    check_pass_at_least(counts, "mc", 29, errors);
    check_fail_zero(counts, "minio-go", errors);
    // The one known failure needs a MinIO extension; the twelve
    // bucket-configuration cases report NA against the pinned backend and are
    // counted separately, so they are not part of this floor.
    check_pass_at_least(counts, "minio-java", 58, errors);
    // The assume-role case passes now that the STS protocol shape is forwarded
    // and the temporary credentials reach the backend.
    check_pass_at_least(counts, "minio-js", 248, errors);
    check_pass_at_least(counts, "minio-py", 22, errors);
    check_fail_zero(counts, "s3cmd", errors);
    check_fail_zero(counts, "s3select", errors);
    check_pass_at_least(counts, "versioning", 18, errors);
}

/// Evaluate the per-function gate; an empty list of errors means it passed.
fn check_gate(logs: &[MintLog], errors: &mut Vec<String>) {
    let mut order: Vec<(String, String)> = Vec::new();
    let mut appearances: HashSet<(String, String)> = HashSet::new();
    let mut fail_counts: HashMap<(String, String), usize> = HashMap::new();

    for entry in logs {
        let function = entry.function.as_deref().unwrap_or_default();
        let key = (entry.name.clone(), normalize_function(&entry.name, function));
        if appearances.insert(key.clone()) {
            order.push(key.clone());
        }
        if entry.status == "FAIL" {
            *fail_counts.entry(key).or_default() += 1;
        }
    }

    for (name, functions) in EXPECTED_FAILURES {
        for (function, max_fail) in *functions {
            let key = ((*name).to_owned(), (*function).to_owned());
            if !appearances.contains(&key) {
                errors.push(format!("expected failure entry is stale: \"{name}\" \"{function}\" did not run"));
                continue;
            }
            let fail_count = fail_counts.get(&key).copied().unwrap_or(0);
            if fail_count > *max_fail {
                errors.push(format!(
                    "\"{name}\" \"{function}\" failed {fail_count} time(s), expected at most {max_fail}"
                ));
            }
        }
    }

    for key in &order {
        let Some(fail_count) = fail_counts.get(key) else {
            continue;
        };
        let (name, function) = key;
        let expected = EXPECTED_FAILURES
            .iter()
            .find(|(expected_name, _)| expected_name == name)
            .is_some_and(|(_, functions)| functions.iter().any(|(expected, _)| expected == function));
        if !expected {
            errors.push(format!("unexpected failure: \"{name}\" \"{function}\" failed {fail_count} time(s)"));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Mint, check_gate, counters, normalize_function, parse_log};
    use std::io::Write as _;

    fn log_file(name: &str, contents: &str) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!("xtask-report-{name}-{}.json", std::process::id()));
        let mut file = std::fs::File::create(&path).expect("create");
        file.write_all(contents.as_bytes()).expect("write");
        path
    }

    fn entry(name: &str, status: &str) -> String {
        format!("{{\"name\":\"{name}\",\"status\":\"{status}\",\"duration\":1}}")
    }

    #[test]
    fn counts_groups_in_first_appearance_order() {
        let path = log_file(
            "counts",
            &format!("{}\nNot json\n{}\n{}\n", entry("b", "PASS"), entry("a", "FAIL"), entry("b", "NA")),
        );

        let logs = parse_log(&path).expect("parse");
        assert_eq!(logs.len(), 3, "the non-JSON line is skipped");

        let counts = counters(&logs);
        assert_eq!(counts.order, ["b", "a"], "the name keeps its first position");
        assert_eq!(counts.by_name["b"].pass, 0, "only the last run of \"b\" is counted");
        assert_eq!(counts.by_name["b"].na, 1);
        assert_eq!(counts.by_name["a"].fail, 1);
        assert_eq!(counts.total().pass, 0);
    }

    #[test]
    fn splits_the_function_out_of_the_name() {
        let path = log_file("split", &format!("{}\n", entry("minio-js:listObjects(bucket)", "FAIL")));

        let logs = parse_log(&path).expect("parse");
        assert_eq!(logs[0].name, "minio-js");
        assert_eq!(logs[0].function.as_deref(), Some("listObjects(bucket)"));
    }

    #[test]
    fn normalizes_the_awscli_bucket_name() {
        assert_eq!(
            normalize_function("awscli", "awscli-mint-test-bucket-12345 cp"),
            "awscli-mint-test-bucket-N cp"
        );
        assert_eq!(normalize_function("mc", "test_admin_users"), "test_admin_users");
    }

    #[test]
    fn reports_unexpected_and_stale_entries() {
        let path = log_file(
            "gate",
            &format!(
                "{}\n{}\n{}\n",
                entry("minio-java:getPresignedObjectUrl()", "FAIL"),
                entry("minio-js:notExpected", "FAIL"),
                entry("mc:test_admin_users", "FAIL")
            ),
        );

        let logs = parse_log(&path).expect("parse");
        let mut errors = Vec::new();
        check_gate(&logs, &mut errors);

        assert_eq!(errors.len(), 5, "three unexpected failures plus two stale entries: {errors:?}");
        assert!(
            errors
                .iter()
                .any(|error| error.contains("unexpected failure: \"minio-java\" \"getPresignedObjectUrl()\"")),
            "the case the patched image fixes must have no expected-failure entry left: {errors:?}"
        );
        assert!(
            errors
                .iter()
                .any(|error| error.contains("unexpected failure: \"minio-js\" \"notExpected\""))
        );
        assert!(
            errors
                .iter()
                .any(|error| error.contains("unexpected failure: \"mc\" \"test_admin_users\"")),
            "the passthrough fixes this case, so it has no expected-failure entry left: {errors:?}"
        );
        assert!(errors.iter().any(|error| error.contains("expected failure entry is stale")));
    }

    #[test]
    fn a_partial_log_fails_the_counter_gate() {
        let mint = Mint {
            log: log_file("table", &format!("{}\n{}\n", entry("awscli", "PASS"), entry("mc", "FAIL"))),
        };
        assert!(!mint.run().expect("run"), "the missing suites must be reported");
    }
}
