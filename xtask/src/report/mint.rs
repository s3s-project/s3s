// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

//! Check a `MinIO` `mint` log (<https://github.com/minio/mint#mint-log-format>)
//! against the expected-failure baselines.
//!
//! Port of `scripts/report-mint.py`: the tables, the output format and the
//! exit codes are kept as they were.

use std::collections::{BTreeSet, HashMap, HashSet};
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

    /// The suite did not run in this invocation, so none of its assertions
    /// apply. Repeat the flag or separate the names with commas. A full run
    /// needs no flag: by default every suite the pinned image runs must appear
    /// in the log. A name that is not one of those suites is rejected, so a typo
    /// cannot weaken the gate.
    #[arg(long, value_name = "GROUP", value_delimiter = ',')]
    allow_missing: Vec<String>,
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
/// Baseline recorded against the pinned mint image in `scripts/mint.env` and
/// the pinned backend image in `scripts/minio.env`. The counters and the
/// entries below are calibrated on that backend, so overriding
/// `MINIO_IMAGE_REF` to run the gate against another one does not satisfy
/// them. One entry is left: a `MinIO` extension s3s does not implement. What
/// the earlier entries covered - the s3s defects and the limits of the proxy
/// or the backend - is fixed, or the backend the suites run against answers
/// it, so no entry is left for them.
///
/// The run this list is scored against enables the proxy's authentication
/// passthrough: a request carrying a credential the proxy does not know is
/// forwarded verbatim and the backend decides, so the dynamically created user
/// of `mc test_admin_users` and the temporary credentials of the `minio-js`
/// assume-role case are no longer expected to fail.
const EXPECTED_FAILURES: &[(&str, &[(&str, usize)])] = &[(
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
    //
    // The suite reports 61 passes, one failure and nine `NA` here. Its own
    // conditions explain them: three bucket-encryption cases reach their
    // request and the backend answers `NotImplemented`, which is the only
    // error code the suite logs `NA` for, and three bucket-notification plus
    // three bucket-replication cases return before sending a request because
    // the environment variables they need (`MINIO_JAVA_TEST_SQS_ARN` and the
    // replication ones) are not set. A backend whose CORS handlers answer
    // `NotImplemented` reports three more `NA`, for the bucket-CORS cases,
    // which pass here instead.
    //
    // `aws-sdk-java-v2` has no entry here either: `scripts/mint.sh` sets
    // `ENABLE_HTTP_TESTS=1`, the switch the image's patch 0006 adds, patch 0007
    // gives the CRT case an object it can range over instead of a zero byte
    // object, and the suite reports seven passes, one per case. That count is its
    // floor in `check_counters`, and a failure there is a defect to report rather
    // than an entry to add here.
    &[("putObjectFanOut()", 1)],
)];

/// The suites the pinned mint image runs, in the order the image lists them.
///
/// The existence check is both ways: a suite here that the log does not carry
/// fails the gate, and a suite in the log that is not here fails it too. A suite
/// that is absent is a coverage hole, because the image reports a suite it could
/// not run on its console and the log does not carry that line; a suite that is
/// extra means the log and the image disagree.
///
/// The list is the image's `run/core` directory. An image upgrade that adds a
/// suite therefore fails this gate until the suite is listed here and given a
/// counter check in `check_counters`, which is the point: a new suite must not
/// arrive unnoticed.
///
/// `NON_PARTICIPATING_GROUPS` names the suites that cannot produce a line with the
/// configuration this repository runs mint with; it is empty today because the
/// suite that was in it now runs over plain HTTP. Every suite here must appear, and
/// a non-participating suite producing lines is a notice rather than an error,
/// because the change it announces is the fix.
const EXPECTED_GROUPS: &[&str] = &[
    "aws-sdk-go-v2",
    "aws-sdk-java-v2",
    "aws-sdk-php",
    "aws-sdk-ruby",
    "awscli",
    "healthcheck",
    "mc",
    "minio-go",
    "minio-java",
    "minio-js",
    "minio-py",
    "s3cmd",
    "s3select",
    "versioning",
];

/// The suites that produce no log line, and why. Empty today.
///
/// `aws-sdk-java-v2` was the only member: every one of its cases returned before
/// running it unless the endpoint was reached over TLS, and mint runs over plain
/// HTTP here. The image now runs those cases over plain HTTP behind
/// `ENABLE_HTTP_TESTS`, which `scripts/mint.sh` sets, so the suite logs its seven
/// lines and is held to a floor like every other suite.
///
/// The list stays as the escape hatch for a suite that cannot log at all. A line
/// from a member is reported on stdout rather than failing the gate: the reason
/// no longer holds, so the suite belongs in `EXPECTED_GROUPS` with a counter check,
/// and leaving that transition unnoticed is what this list is for.
const NON_PARTICIPATING_GROUPS: &[(&str, &str)] = &[];

/// Whether the invocation makes no claim about a suite, either because mint did
/// not start it (`--allow-missing`) or because it cannot log at all.
fn is_tolerated(name: &str, allow_missing: &[String]) -> bool {
    is_non_participating(name) || allow_missing.iter().any(|allowed| allowed == name)
}

fn is_non_participating(name: &str) -> bool {
    NON_PARTICIPATING_GROUPS.iter().any(|(group, _)| *group == name)
}

/// The awscli runner uses the full command line as the test function name,
/// including a random bucket name; normalize it so the name is stable across
/// runs.
static AWS_CLI_BUCKET: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"awscli-mint-test-bucket-\d+").expect("constant regex"));

impl Mint {
    pub(crate) fn run(self) -> Result<bool> {
        let (logs, unparsed) = parse_log(&self.log)?;
        let counts = counters(&logs);

        for name in &counts.order {
            print_counter(name, counts.by_name[name]);
        }
        println!();

        let totals = counts.total();
        print_counter("summary", totals);

        // A line that is not a JSON object is named with its position, and does
        // not decide the exit code on its own: a suite is free to print a banner,
        // and the assertions below are about the suites that print nothing.
        if !unparsed.is_empty() {
            println!();
            for line in &unparsed {
                println!("error parsing log line {}:{}: {}", self.log.display(), line.number, line.text);
            }
        }

        let observed: BTreeSet<&str> = counts.order.iter().map(String::as_str).collect();

        // Every check runs to completion so every violation is reported; the exit
        // code is decided afterwards.
        let mut notices = Vec::new();
        check_non_participating(&observed, NON_PARTICIPATING_GROUPS, &mut notices);

        let mut errors = Vec::new();
        check_allow_missing(&self.allow_missing, &observed, &mut errors, &mut notices);
        check_expected_groups(&observed, &self.allow_missing, &mut errors);
        check_unexpected_groups(&observed, &mut errors);
        check_counters(&counts.by_name, &self.allow_missing, &mut errors);
        check_gate(&logs, &self.allow_missing, &mut errors);

        if !notices.is_empty() {
            println!();
            for notice in &notices {
                println!("note: {notice}");
            }
        }

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

/// A log line that is not a JSON object, with its 1-based position in the file.
#[derive(Debug)]
struct UnparsedLine {
    number: usize,
    text: String,
}

/// Read the log, skipping the lines that are not JSON objects.
///
/// Unlike the script, a line that parses but lacks a field is reported here
/// instead of aborting the run. The position of every unparsable line is kept so
/// the caller can name it, which is what tells a suite that printed a banner
/// apart from a suite that printed nothing.
fn parse_log(path: &Path) -> Result<(Vec<MintLog>, Vec<UnparsedLine>)> {
    let text = fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?;
    let mut logs = Vec::new();
    let mut unparsed = Vec::new();

    for (index, raw) in text.lines().enumerate() {
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
            Err(_) => unparsed.push(UnparsedLine {
                number: index + 1,
                text: line.to_owned(),
            }),
        }
    }

    Ok((logs, unparsed))
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
/// Every suite in `EXPECTED_GROUPS` carries the same three-part contract, and this
/// function states all three parts for each of them:
///
/// 1. the suite appears in the log, which `check_expected_groups` asserts;
/// 2. it reports at least the passes a full run of the pinned image produces, so a
///    suite that stops early fails here;
/// 3. it reports no failure at all, unless the failure is registered by name in
///    `EXPECTED_FAILURES`, which `check_gate` holds to its count.
///
/// The floors are the passes a full run produces: both images are pinned by digest
/// (see `scripts/mint.env` and `scripts/minio.env`), so the numbers are stable. A
/// floor that has to be lowered, or a `check_fail_zero` that has to become a
/// count, needs the reason written next to it.
fn check_counters(counts: &HashMap<String, Counters>, allow_missing: &[String], errors: &mut Vec<String>) {
    fn check_pass_at_least(
        counts: &HashMap<String, Counters>,
        name: &str,
        minimum: usize,
        allow_missing: &[String],
        errors: &mut Vec<String>,
    ) {
        if is_tolerated(name, allow_missing) {
            return;
        }
        let pass_count = counts.get(name).map_or(0, |counter| counter.pass);
        if pass_count < minimum {
            errors.push(format!("group counter: \"{name}\" passed {pass_count}, expected at least {minimum}"));
        }
    }

    fn check_fail_zero(counts: &HashMap<String, Counters>, name: &str, allow_missing: &[String], errors: &mut Vec<String>) {
        if is_tolerated(name, allow_missing) {
            return;
        }
        let fail_count = counts.get(name).map_or(0, |counter| counter.fail);
        if fail_count != 0 {
            errors.push(format!("group counter: \"{name}\" failed {fail_count} test(s), expected 0"));
        }
    }

    check_pass_at_least(counts, "aws-sdk-go-v2", 24, allow_missing, errors);
    check_fail_zero(counts, "aws-sdk-go-v2", allow_missing, errors);
    // Seven cases, one line each. The switch that runs them over plain HTTP is in
    // `scripts/mint.sh`; no failure is expected, so there is no entry in
    // `EXPECTED_FAILURES`.
    check_pass_at_least(counts, "aws-sdk-java-v2", 7, allow_missing, errors);
    check_fail_zero(counts, "aws-sdk-java-v2", allow_missing, errors);
    check_pass_at_least(counts, "aws-sdk-php", 13, allow_missing, errors);
    check_fail_zero(counts, "aws-sdk-php", allow_missing, errors);
    // No known failure is left in this suite.
    check_pass_at_least(counts, "aws-sdk-ruby", 13, allow_missing, errors);
    check_fail_zero(counts, "aws-sdk-ruby", allow_missing, errors);
    check_pass_at_least(counts, "awscli", 18, allow_missing, errors);
    check_fail_zero(counts, "awscli", allow_missing, errors);
    check_pass_at_least(counts, "healthcheck", 6, allow_missing, errors);
    check_fail_zero(counts, "healthcheck", allow_missing, errors);
    // `test_admin_users` passes now that the authentication passthrough lets the
    // dynamically created user reach the backend.
    check_pass_at_least(counts, "mc", 29, allow_missing, errors);
    check_fail_zero(counts, "mc", allow_missing, errors);
    check_pass_at_least(counts, "minio-go", 2, allow_missing, errors);
    check_fail_zero(counts, "minio-go", allow_missing, errors);
    // The one known failure needs a MinIO extension; the nine bucket
    // configuration cases the suite cannot run here report `NA` and are
    // counted separately, so they are not part of this floor. This is the only
    // suite without a `check_fail_zero`: its failure is the entry in
    // `EXPECTED_FAILURES`, which allows exactly that one case to fail and fails
    // the gate for every other failure in the suite.
    check_pass_at_least(counts, "minio-java", 61, allow_missing, errors);
    // The assume-role case passes now that the STS protocol shape is forwarded
    // and the temporary credentials reach the backend.
    check_pass_at_least(counts, "minio-js", 249, allow_missing, errors);
    check_fail_zero(counts, "minio-js", allow_missing, errors);
    check_pass_at_least(counts, "minio-py", 22, allow_missing, errors);
    check_fail_zero(counts, "minio-py", allow_missing, errors);
    check_pass_at_least(counts, "s3cmd", 8, allow_missing, errors);
    check_fail_zero(counts, "s3cmd", allow_missing, errors);
    check_pass_at_least(counts, "s3select", 11, allow_missing, errors);
    check_fail_zero(counts, "s3select", allow_missing, errors);
    check_pass_at_least(counts, "versioning", 18, allow_missing, errors);
    check_fail_zero(counts, "versioning", allow_missing, errors);
}

/// Every suite the image runs must contribute at least one line, unless the
/// invocation does not claim it.
///
/// This is the assertion the counter checks cannot make. A suite whose lines all
/// failed to parse, and a suite that never started, are both absent from the log,
/// and only absence is observable here.
fn check_expected_groups(observed: &BTreeSet<&str>, allow_missing: &[String], errors: &mut Vec<String>) {
    let missing: Vec<&str> = EXPECTED_GROUPS
        .iter()
        .copied()
        .filter(|group| !observed.contains(group) && !is_tolerated(group, allow_missing))
        .collect();
    if missing.is_empty() {
        return;
    }

    let seen: Vec<&str> = observed.iter().copied().collect();
    errors.push(format!(
        "suite(s) missing from the mint log: {} (log has: {})",
        missing.join(", "),
        seen.join(", ")
    ));
}

/// A suite in the log that the image does not run is an error.
///
/// The expected set is the image's `run/core` directory, so a name outside it
/// means the log and the image disagree: either the image gained a suite that has
/// to be listed and given a counter check, or the log did not come from the
/// pinned image. Both are worth failing on before the numbers are read.
fn check_unexpected_groups(observed: &BTreeSet<&str>, errors: &mut Vec<String>) {
    let unexpected: Vec<&str> = observed
        .iter()
        .copied()
        .filter(|group| !EXPECTED_GROUPS.contains(group))
        .collect();
    if unexpected.is_empty() {
        return;
    }

    let seen: Vec<&str> = observed.iter().copied().collect();
    errors.push(format!(
        "suite(s) in the mint log that the image does not run: {}; add them to EXPECTED_GROUPS and give them a counter check in check_counters (log has: {})",
        unexpected.join(", "),
        seen.join(", ")
    ));
}

/// Report a non-participating suite that produced lines anyway.
///
/// The reason it is listed as non-participating no longer holds, so it belongs in
/// `EXPECTED_GROUPS` with a counter check. This is a notice rather than an error:
/// the change it announces is the fix, and the per-function gate already fails if
/// the new lines carry a failure.
fn check_non_participating(observed: &BTreeSet<&str>, groups: &[(&str, &str)], notices: &mut Vec<String>) {
    for (group, reason) in groups {
        if observed.contains(group) {
            notices.push(format!(
                "suite \"{group}\" is declared non-participating but produced log lines: {reason}"
            ));
        }
    }
}

/// Validate the `--allow-missing` names, and report the ones that had no effect.
///
/// A name the image does not run is an error, because a typo would silently
/// weaken the gate. A name whose suite did produce lines is a notice: the flag
/// was not needed for this log.
fn check_allow_missing(allow_missing: &[String], observed: &BTreeSet<&str>, errors: &mut Vec<String>, notices: &mut Vec<String>) {
    for name in allow_missing {
        if !EXPECTED_GROUPS.contains(&name.as_str()) {
            errors.push(format!("--allow-missing names a suite the image does not run: \"{name}\""));
            continue;
        }
        if observed.contains(name.as_str()) {
            notices.push(format!(
                "--allow-missing names \"{name}\", which produced log lines; the flag has no effect here"
            ));
        }
    }
}

/// Evaluate the per-function gate; an empty list of errors means it passed.
fn check_gate(logs: &[MintLog], allow_missing: &[String], errors: &mut Vec<String>) {
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
                // A suite this invocation does not claim cannot make its baseline
                // entry stale.
                if !is_tolerated(name, allow_missing) {
                    errors.push(format!("expected failure entry is stale: \"{name}\" \"{function}\" did not run"));
                }
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
    use super::{
        EXPECTED_FAILURES, EXPECTED_GROUPS, Mint, check_expected_groups, check_gate, check_non_participating,
        check_unexpected_groups, counters, is_non_participating, normalize_function, parse_log,
    };
    use std::collections::BTreeSet;
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

        let (logs, unparsed) = parse_log(&path).expect("parse");
        assert_eq!(logs.len(), 3, "the non-JSON line is skipped");
        assert_eq!(unparsed.len(), 1, "the non-JSON line is kept for the report");
        assert_eq!(unparsed[0].number, 2, "its position is 1-based");

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

        let (logs, _) = parse_log(&path).expect("parse");
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

        let (logs, _) = parse_log(&path).expect("parse");
        let mut errors = Vec::new();
        check_gate(&logs, &[], &mut errors);

        assert_eq!(errors.len(), 4, "three unexpected failures plus one stale entry: {errors:?}");
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
            allow_missing: Vec::new(),
        };
        assert!(!mint.run().expect("run"), "the missing suites must be reported");
    }

    /// A log that satisfies every assertion of a full run: each suite the image
    /// runs contributes lines, the pass floors are met, and every expected failure
    /// appears once inside its suite's block, which is how mint writes a suite.
    fn full_log() -> String {
        const PASSES: &[(&str, usize)] = &[
            ("aws-sdk-go-v2", 24),
            ("aws-sdk-java-v2", 7),
            ("aws-sdk-php", 13),
            ("aws-sdk-ruby", 13),
            ("awscli", 18),
            ("healthcheck", 6),
            ("mc", 29),
            ("minio-go", 2),
            ("minio-java", 61),
            ("minio-js", 249),
            ("minio-py", 22),
            ("s3cmd", 8),
            ("s3select", 11),
            ("versioning", 18),
        ];
        const FAILURES: &[(&str, &str)] = &[("minio-java", "putObjectFanOut()")];

        let mut lines = Vec::new();
        for (suite, passes) in PASSES {
            for index in 0..*passes {
                lines.push(format!(
                    "{{\"name\":\"{suite}\",\"function\":\"{suite}_case_{index}\",\"status\":\"PASS\"}}"
                ));
            }
            for (failure_suite, function) in FAILURES.iter().filter(|(name, _)| name == suite) {
                lines.push(format!("{{\"name\":\"{failure_suite}:{function}\",\"status\":\"FAIL\"}}"));
            }
        }
        lines.join("\n") + "\n"
    }

    /// The same log without every line of one suite, in both the plain and the
    /// prefixed name form mint uses for a failure entry.
    fn without_suite(log: &str, suite: &str) -> String {
        let plain = format!("\"name\":\"{suite}\"");
        let prefixed = format!("\"name\":\"{suite}:");
        let kept: Vec<&str> = log
            .lines()
            .filter(|line| !line.contains(&plain) && !line.contains(&prefixed))
            .collect();
        kept.join("\n") + "\n"
    }

    /// The same log with one pass of a suite removed: the suite is still there, one
    /// line short of a full run.
    fn without_one_pass(log: &str, suite: &str) -> String {
        let needle = format!("\"name\":\"{suite}\"");
        let mut removed = false;
        let mut kept = Vec::new();
        for line in log.lines() {
            if !removed && line.contains(&needle) && line.contains("\"status\":\"PASS\"") {
                removed = true;
                continue;
            }
            kept.push(line);
        }
        assert!(removed, "the fixture has a pass line for {suite}");
        kept.join("\n") + "\n"
    }

    /// The same log with an unregistered failure appended to the end of a suite's
    /// block, which is how mint writes a suite: the failure belongs to that run.
    fn with_failure_in_suite(log: &str, suite: &str, function: &str) -> String {
        let needle = format!("\"name\":\"{suite}\"");
        let lines: Vec<&str> = log.lines().collect();
        let mut out = String::new();
        let mut inserted = false;
        for (index, line) in lines.iter().enumerate() {
            out.push_str(line);
            out.push('\n');
            let ends_the_block = line.contains(&needle) && !lines.get(index + 1).is_some_and(|next| next.contains(&needle));
            if ends_the_block && !inserted {
                out.push_str(&entry(&format!("{suite}:{function}"), "FAIL"));
                out.push('\n');
                inserted = true;
            }
        }
        assert!(inserted, "the fixture has a block for {suite}");
        out
    }

    fn observed_suites(log: &str, name: &str) -> BTreeSet<String> {
        let (logs, _) = parse_log(&log_file(name, log)).expect("parse");
        counters(&logs).order.into_iter().collect()
    }

    /// Positive control: the log a full run produces passes, now that every suite
    /// in the image logs its lines.
    #[test]
    fn a_full_log_passes() {
        let mint = Mint {
            log: log_file("full", &full_log()),
            allow_missing: Vec::new(),
        };
        assert!(mint.run().expect("run"), "a full log must pass");
    }

    /// A suite the image runs that is absent from the log is named, even when no
    /// counter check covers it: `healthcheck` has none.
    #[test]
    fn a_missing_suite_is_named_by_the_expected_suite_check() {
        let log = without_suite(&full_log(), "healthcheck");
        let observed = observed_suites(&log, "missing-named");
        let observed: BTreeSet<&str> = observed.iter().map(String::as_str).collect();

        let mut errors = Vec::new();
        check_expected_groups(&observed, &[], &mut errors);
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(errors[0].contains("healthcheck"), "{errors:?}");
        assert!(errors[0].contains("log has:"), "the message lists what the log has: {errors:?}");

        let mint = Mint {
            log: log_file("missing-suite", &log),
            allow_missing: Vec::new(),
        };
        assert!(!mint.run().expect("run"), "a suite that vanished from the log must fail the gate");
    }

    /// A suite whose lines all fail to parse contributes nothing, so it counts as
    /// missing; its banner lines are still reported with their positions.
    #[test]
    fn a_suite_with_only_unparsable_lines_is_missing() {
        let full = full_log();
        let broken = full.matches("\"name\":\"healthcheck\"").count();
        assert!(broken > 0, "the fixture has healthcheck lines");
        let log = full.replace("\"name\":\"healthcheck\"", "\"name\" \"healthcheck\"");
        let path = log_file("unparsable-suite", &log);

        let (_, unparsed) = parse_log(&path).expect("parse");
        assert_eq!(unparsed.len(), broken, "every banner line is kept");
        assert!(unparsed.iter().all(|line| line.number > 0), "each line keeps its position");

        let mint = Mint {
            log: path,
            allow_missing: Vec::new(),
        };
        assert!(!mint.run().expect("run"), "a suite that contributes no parsed line is missing");
    }

    /// A banner line is reported with its file and position and does not decide
    /// the exit code: the mc suite prints one before its JSON lines.
    #[test]
    fn unparsable_lines_are_reported_without_failing_the_gate() {
        let log = format!("Dependency validation complete\n{}", full_log());
        let path = log_file("unparsable-line", &log);

        let (_, unparsed) = parse_log(&path).expect("parse");
        assert_eq!(unparsed.len(), 1);
        assert_eq!(unparsed[0].number, 1);
        assert_eq!(unparsed[0].text, "Dependency validation complete");

        let mint = Mint {
            log: path,
            allow_missing: Vec::new(),
        };
        assert!(mint.run().expect("run"), "a banner line alone must not fail the gate");
    }

    /// A suite in the log that the image does not run fails the gate and is named,
    /// so an image upgrade that adds one is not read as a clean run either.
    #[test]
    fn an_unknown_suite_in_the_log_fails_the_gate() {
        let log = full_log() + &format!("{}\n", entry("brand-new-suite", "PASS"));
        let observed = observed_suites(&log, "unknown-suite");
        let observed: BTreeSet<&str> = observed.iter().map(String::as_str).collect();

        let mut errors = Vec::new();
        check_unexpected_groups(&observed, &mut errors);
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(errors[0].contains("brand-new-suite"), "{errors:?}");
        assert!(
            errors[0].contains("EXPECTED_GROUPS") && errors[0].contains("check_counters"),
            "the message says what to update: {errors:?}"
        );

        let mint = Mint {
            log: log_file("unknown-suite-run", &log),
            allow_missing: Vec::new(),
        };
        assert!(!mint.run().expect("run"), "an unknown suite must fail the gate");
    }

    /// A suite name carrying its function after a colon is that same suite, so the
    /// existence check must not report it as unknown.
    #[test]
    fn a_colon_suffixed_name_is_the_same_suite() {
        // Two lines, not one: `counters` keeps the last consecutive run of a name,
        // so this appended run replaces the `minio-go` block of the fixture and has
        // to meet that suite's floor of two by itself. One line would fall below the
        // floor and fail the run assertion below for a reason that has nothing to do
        // with the name normalisation this test is about.
        let log = full_log()
            + &format!(
                "{}\n{}\n",
                entry("minio-go: testFunctional", "PASS"),
                entry("minio-go: testFunctional", "PASS")
            );
        let observed = observed_suites(&log, "colon-name");
        let observed: BTreeSet<&str> = observed.iter().map(String::as_str).collect();

        let mut errors = Vec::new();
        check_unexpected_groups(&observed, &mut errors);
        assert!(errors.is_empty(), "{errors:?}");

        let mint = Mint {
            log: log_file("colon-name-run", &log),
            allow_missing: Vec::new(),
        };
        assert!(mint.run().expect("run"), "a colon-suffixed name belongs to its suite");
    }

    /// A non-participating suite that logs anyway is reported: its reason no
    /// longer holds. The pinned image has no such suite today, so the check is
    /// exercised with a synthetic list; the notice stays a notice rather than an
    /// error, because the change it announces is the fix.
    #[test]
    fn a_non_participating_suite_that_logs_is_reported() {
        let groups = [("aws-sdk-java-v2", "every case returns before running")];
        let log = full_log();
        let observed = observed_suites(&log, "non-participating");
        let observed: BTreeSet<&str> = observed.iter().map(String::as_str).collect();

        let mut notices = Vec::new();
        check_non_participating(&observed, &groups, &mut notices);
        assert_eq!(notices.len(), 1, "{notices:?}");
        assert!(notices[0].contains("aws-sdk-java-v2"), "{notices:?}");
        assert!(
            notices[0].contains("every case returns before running"),
            "the reason travels with the notice: {notices:?}"
        );
    }

    /// The closed-out state: the java suite is expected, held to a floor, and no
    /// longer declared non-participating.
    #[test]
    fn the_java_suite_participates() {
        assert!(EXPECTED_GROUPS.contains(&"aws-sdk-java-v2"), "the suite is part of the image's set");
        assert!(
            !is_non_participating("aws-sdk-java-v2"),
            "the suite logs seven lines over plain HTTP now, so it cannot stay in the list"
        );
    }

    /// The floor is live for `healthcheck`, the suite that had no counter check at
    /// all before: one pass short of its six must fail the gate.
    #[test]
    fn the_healthcheck_floor_is_live() {
        let log = without_one_pass(&full_log(), "healthcheck");
        let observed = observed_suites(&log, "healthcheck-short");
        let observed: BTreeSet<&str> = observed.iter().map(String::as_str).collect();
        let counts = counters(&parse_log(&log_file("healthcheck-short-counts", &log)).expect("parse").0);
        assert_eq!(counts.by_name["healthcheck"].pass, 5, "the fixture is one line short");

        let mint = Mint {
            log: log_file("healthcheck-short", &log),
            allow_missing: Vec::new(),
        };
        assert!(!mint.run().expect("run"), "a short healthcheck suite must fail the gate");
        assert!(observed.contains("healthcheck"), "the suite is still in the log");
    }

    /// Part two of the contract, for every suite in the expected set: one pass short
    /// of a full run fails the gate.
    #[test]
    fn every_expected_suite_has_a_pass_floor() {
        for suite in EXPECTED_GROUPS {
            let log = without_one_pass(&full_log(), suite);
            let mint = Mint {
                log: log_file(&format!("floor-{suite}"), &log),
                allow_missing: Vec::new(),
            };
            assert!(!mint.run().expect("run"), "a suite one pass short must fail: {suite}");
        }
    }

    /// Part three of the contract, for every suite in the expected set: a failure
    /// that is not registered by name fails the gate. The registered suites are the
    /// ones `EXPECTED_FAILURES` holds to their own count.
    #[test]
    fn every_expected_suite_forbids_unregistered_failures() {
        let registered: Vec<&str> = EXPECTED_FAILURES.iter().map(|(name, _)| *name).collect();
        for suite in EXPECTED_GROUPS {
            if registered.contains(suite) {
                continue;
            }
            let log = with_failure_in_suite(&full_log(), suite, "unregistered_case");
            let mint = Mint {
                log: log_file(&format!("failzero-{suite}"), &log),
                allow_missing: Vec::new(),
            };
            assert!(!mint.run().expect("run"), "an unregistered failure must fail: {suite}");
        }
    }

    /// `--allow-missing` drops every assertion of the named suite, so a local
    /// run of a subset passes; without the flag the same log fails.
    #[test]
    fn allow_missing_tolerates_a_suite_that_did_not_run() {
        let log = without_suite(&full_log(), "versioning");

        let strict = Mint {
            log: log_file("allow-missing-off", &log),
            allow_missing: Vec::new(),
        };
        assert!(!strict.run().expect("run"), "without the flag the missing floor must fail");

        let tolerated = Mint {
            log: log_file("allow-missing-on", &log),
            allow_missing: vec!["versioning".to_owned()],
        };
        assert!(tolerated.run().expect("run"), "the flag tolerates the suite and its floor");
    }

    /// A name the image does not run is rejected, so a typo cannot weaken the
    /// gate.
    #[test]
    fn allow_missing_rejects_an_unknown_suite() {
        let mint = Mint {
            log: log_file("allow-missing-typo", &full_log()),
            allow_missing: vec!["minio-jss".to_owned()],
        };
        assert!(!mint.run().expect("run"), "the typo must be reported");
    }
}
