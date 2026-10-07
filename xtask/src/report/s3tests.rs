// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

//! Summarize the `Ceph` `s3-tests` `JUnit` report and gate on the baselines.
//!
//! Port of `scripts/report-s3tests.py`: the categories, the output format and
//! the exit codes are kept as they were.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use anyhow::{Context, Result, bail};
use clap::Parser;
use quick_xml::events::{BytesStart, Event};
use quick_xml::{Reader, XmlVersion};
use regex::Regex;

/// Summarize the s3-tests report and gate on the baselines.
#[derive(Debug, Parser)]
pub(crate) struct S3Tests {
    /// Path to the report.
    report: PathBuf,
}

/// Baseline results as of 2026-10-07, measured against the pinned suite image
/// (`ghcr.io/s3s-project/s3-tests`, upstream 5522d1c3 plus the fixture patches);
/// reduce these as compatibility improves.
const ALLOWED_FAILURES: u64 = 525;
const ALLOWED_ERRORS: u64 = 2;

/// Mapping from the last component of the pytest classname to an S3 capability
/// category.
const MODULE_CATEGORIES: &[(&str, &str)] = &[
    ("test_headers", "headers"),
    ("test_iam", "iam"),
    ("test_s3select", "s3-select"),
    ("test_sns", "sns"),
    ("test_sts", "sts"),
    ("test_utils", "utils"),
];

/// Ordered list of (pattern, category) pairs used to classify tests from
/// `s3tests.functional.test_s3`. The first matching pattern wins.
static S3_PATTERNS: LazyLock<Vec<(Regex, &'static str)>> = LazyLock::new(|| {
    [
        (r"object_lock", "object-lock"),
        (r"presigned", "presigned"),
        (r"multipart", "multipart"),
        (r"sse_|_sse_|sse$|encryption|encrypted|_enc_|_kms_", "encryption"),
        (r"versioning|versioned|delete_marker", "versioning"),
        (r"lifecycle", "lifecycle"),
        (r"cors", "cors"),
        (r"post_object", "post-object"),
        (r"_tag", "tagging"),
        (r"acl", "acl"),
        (r"access_bucket|block_public|ignore_public|public_block", "acl"),
        (r"_policy|policy_", "policy"),
        (r"bucket_list|listv2", "list-objects"),
        (r"logging", "logging"),
        (r"delete", "delete"),
        (r"restore", "restore"),
        (r"ranged", "range-get"),
        (r"copy", "copy"),
        (r"bucket", "bucket"),
    ]
    .into_iter()
    .map(|(pattern, category)| (Regex::new(pattern).expect("constant regex"), category))
    .collect()
});

/// Counters of one capability category.
#[derive(Debug, Default, Clone, Copy)]
struct Counts {
    total: u64,
    passed: u64,
    failures: u64,
    errors: u64,
    skipped: u64,
}

/// Totals of every `testsuite` element.
#[derive(Debug, Default)]
struct Totals {
    tests: u64,
    failures: u64,
    errors: u64,
    skipped: u64,
}

/// A test case while its children are being read.
#[derive(Debug, Default)]
struct TestCase {
    classname: String,
    name: String,
    failure: bool,
    error: bool,
    skipped: bool,
}

impl S3Tests {
    pub(crate) fn run(self) -> Result<bool> {
        let report = read_report(&self.report)?;
        print_report(&report);
        check_baselines(&report)?;
        Ok(true)
    }
}

/// Return the S3 capability category for a test case.
fn classify_test(classname: &str, name: &str) -> &'static str {
    let module = classname.rsplit('.').next().unwrap_or_default();
    if module != "test_s3" {
        return MODULE_CATEGORIES
            .iter()
            .find(|(expected, _)| *expected == module)
            .map_or("other", |(_, category)| *category);
    }
    for (pattern, category) in S3_PATTERNS.iter() {
        if pattern.is_match(name) {
            return category;
        }
    }
    "object"
}

fn attribute(element: &BytesStart<'_>, name: &str) -> Result<Option<String>> {
    for attribute in element.attributes() {
        let attribute = attribute.context("malformed attribute")?;
        if attribute.key.local_name().as_ref() == name {
            // JUnit reports are XML 1.0 documents, which is the default version.
            let value = attribute
                .normalized_value(XmlVersion::default())
                .context("invalid attribute value")?;
            return Ok(Some(value.into_owned()));
        }
    }
    Ok(None)
}

fn integer_attribute(element: &BytesStart<'_>, name: &str) -> Result<u64> {
    let Some(value) = attribute(element, name)? else {
        return Ok(0);
    };
    value.parse().with_context(|| format!("invalid {name} value: {value}"))
}

fn record(categories: &mut BTreeMap<&'static str, Counts>, case: &TestCase) {
    let counts = categories.entry(classify_test(&case.classname, &case.name)).or_default();
    counts.total += 1;
    if case.failure {
        counts.failures += 1;
    } else if case.error {
        counts.errors += 1;
    } else if case.skipped {
        counts.skipped += 1;
    } else {
        counts.passed += 1;
    }
}

/// The parsed report: the totals of every counted suite plus the per-category
/// breakdown.
#[derive(Debug, Default)]
struct Report {
    totals: Totals,
    categories: BTreeMap<&'static str, Counts>,
}

fn read_report(report_path: &Path) -> Result<Report> {
    let xml = fs::read_to_string(report_path).with_context(|| format!("report not found: {}", report_path.display()))?;
    let mut reader = Reader::from_str(&xml);
    let mut totals = Totals::default();
    let mut categories: BTreeMap<&'static str, Counts> = BTreeMap::new();
    let mut current: Option<TestCase> = None;
    let mut depth = 0_usize;

    loop {
        match reader.read_event() {
            Ok(Event::Start(element)) => {
                match element.local_name().as_ref() {
                    // The script only counts the root and its direct children.
                    "testsuite" if depth <= 1 => {
                        totals.tests += integer_attribute(&element, "tests")?;
                        totals.failures += integer_attribute(&element, "failures")?;
                        totals.errors += integer_attribute(&element, "errors")?;
                        totals.skipped += integer_attribute(&element, "skipped")?;
                    }
                    "testcase" => {
                        current = Some(TestCase {
                            classname: attribute(&element, "classname")?.unwrap_or_default(),
                            name: attribute(&element, "name")?.unwrap_or_default(),
                            ..TestCase::default()
                        });
                    }
                    "failure" => set(&mut current, |case| case.failure = true),
                    "error" => set(&mut current, |case| case.error = true),
                    "skipped" => set(&mut current, |case| case.skipped = true),
                    _ => {}
                }
                depth += 1;
            }
            Ok(Event::Empty(element)) => match element.local_name().as_ref() {
                "testsuite" if depth <= 1 => {
                    totals.tests += integer_attribute(&element, "tests")?;
                    totals.failures += integer_attribute(&element, "failures")?;
                    totals.errors += integer_attribute(&element, "errors")?;
                    totals.skipped += integer_attribute(&element, "skipped")?;
                }
                "testcase" => {
                    let case = TestCase {
                        classname: attribute(&element, "classname")?.unwrap_or_default(),
                        name: attribute(&element, "name")?.unwrap_or_default(),
                        ..TestCase::default()
                    };
                    record(&mut categories, &case);
                }
                "failure" => set(&mut current, |case| case.failure = true),
                "error" => set(&mut current, |case| case.error = true),
                "skipped" => set(&mut current, |case| case.skipped = true),
                _ => {}
            },
            Ok(Event::End(element)) => {
                if element.local_name().as_ref() == "testcase"
                    && let Some(case) = current.take()
                {
                    record(&mut categories, &case);
                }
                depth = depth.saturating_sub(1);
            }
            Ok(Event::Eof) => break,
            Err(error) => {
                return Err(error).with_context(|| format!("error parsing {}", report_path.display()));
            }
            _ => {}
        }
    }

    Ok(Report { totals, categories })
}

fn print_report(report: &Report) {
    println!(
        "tests {}, failures {}, errors {}, skipped {}",
        report.totals.tests, report.totals.failures, report.totals.errors, report.totals.skipped
    );
    println!();
    println!(
        "{:<16} {:>6} {:>7} {:>9} {:>7} {:>8}",
        "capability", "total", "passed", "failures", "errors", "skipped"
    );
    for (category, counts) in &report.categories {
        println!(
            "{category:<16} {:>6} {:>7} {:>9} {:>7} {:>8}",
            counts.total, counts.passed, counts.failures, counts.errors, counts.skipped
        );
    }
}

fn check_baselines(report: &Report) -> Result<()> {
    if report.totals.failures > ALLOWED_FAILURES || report.totals.errors > ALLOWED_ERRORS {
        bail!(
            "s3-tests regressions: failures {} (allowed {ALLOWED_FAILURES}), errors {} (allowed {ALLOWED_ERRORS})",
            report.totals.failures,
            report.totals.errors
        );
    }
    Ok(())
}

fn set(current: &mut Option<TestCase>, apply: impl FnOnce(&mut TestCase)) {
    if let Some(case) = current.as_mut() {
        apply(case);
    }
}

#[cfg(test)]
mod tests {
    use super::{Report, Totals, check_baselines, classify_test, read_report};
    use std::io::Write as _;

    const JUNIT: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<testsuites name="pytest tests">
  <testsuite name="pytest" errors="1" failures="1" skipped="1" tests="4" time="1.0">
    <testcase classname="s3tests.functional.test_headers" name="test_ok" time="0.1" />
    <testcase classname="s3tests.functional.test_headers" name="test_bad" time="0.1"><failure message="x">trace</failure></testcase>
    <testcase classname="s3tests.functional.test_iam" name="test_err" time="0.1"><error message="y">trace</error></testcase>
    <testcase classname="s3tests.functional.test_s3select" name="test_skip" time="0.1"><skipped message="z" /></testcase>
    <testsuite name="nested" tests="99" failures="99">
      <testcase classname="s3tests.functional.test_s3" name="test_object_lock_put" time="0.1" />
    </testsuite>
  </testsuite>
</testsuites>
"#;

    fn report_file(name: &str, contents: &str) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!("xtask-report-{name}-{}.xml", std::process::id()));
        let mut file = std::fs::File::create(&path).expect("create");
        file.write_all(contents.as_bytes()).expect("write");
        path
    }

    #[test]
    fn classifies_modules_and_s3_tests() {
        assert_eq!(classify_test("s3tests.functional.test_headers", "test_bucket"), "headers");
        assert_eq!(classify_test("s3tests.functional.test_iam", "test_users"), "iam");
        assert_eq!(classify_test("s3tests.functional.test_s3", "test_object_lock"), "object-lock");
        assert_eq!(classify_test("s3tests.functional.test_s3", "test_versioning_obj"), "versioning");
        assert_eq!(classify_test("s3tests.functional.test_s3", "test_acl"), "acl");
        assert_eq!(classify_test("s3tests.functional.test_s3", "test_plain"), "object");
        assert_eq!(classify_test("something.unknown", "test"), "other");
    }

    #[test]
    fn the_earlier_pattern_wins() {
        // `presigned` comes before `_tag` and `bucket`.
        assert_eq!(classify_test("s3tests.functional.test_s3", "test_presigned_post_tag"), "presigned");
    }

    #[test]
    fn reads_totals_and_categories_from_a_report() {
        let report = read_report(&report_file("junit", JUNIT)).expect("parse");

        assert_eq!(report.totals.tests, 4);
        assert_eq!(report.totals.failures, 1);
        assert_eq!(report.totals.errors, 1);
        assert_eq!(report.totals.skipped, 1, "the nested suite is not counted in the totals");

        let categories: Vec<(&str, u64, u64, u64, u64, u64)> = report
            .categories
            .iter()
            .map(|(category, counts)| (*category, counts.total, counts.passed, counts.failures, counts.errors, counts.skipped))
            .collect();
        assert_eq!(
            categories,
            [
                ("headers", 2, 1, 1, 0, 0),
                ("iam", 1, 0, 0, 1, 0),
                ("object-lock", 1, 1, 0, 0, 0),
                ("s3-select", 1, 0, 0, 0, 1),
            ],
            "a self-closing test case passes, and the nested suite's case is counted"
        );
    }

    #[test]
    fn the_baseline_gate_reports_the_totals() {
        let over = Report {
            totals: Totals {
                tests: 1055,
                failures: 526,
                errors: 3,
                skipped: 163,
            },
            ..Report::default()
        };
        let error = check_baselines(&over).expect_err("over the baseline");
        assert_eq!(
            error.to_string(),
            "s3-tests regressions: failures 526 (allowed 525), errors 3 (allowed 2)"
        );

        let at_the_limit = Report {
            totals: Totals {
                tests: 1053,
                failures: 525,
                errors: 2,
                skipped: 163,
            },
            ..Report::default()
        };
        assert!(check_baselines(&at_the_limit).is_ok(), "the baseline itself is allowed");
    }
}
