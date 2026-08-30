// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

use serde::{Deserialize, Serialize};

/// The top-level report produced by the runner.
///
/// Serialized to JSON with `--json`. The exit code is nonzero when
/// `suite_count.all_passed()` is false.
#[derive(Serialize, Deserialize)]
pub struct Report {
    pub suite_count: CountSummary,
    pub duration_ns: u64,
    pub duration_ms: f64,

    pub suites: Vec<SuiteReport>,
}

/// The report of one suite, including setup/teardown summaries and fixture
/// reports.
#[derive(Serialize, Deserialize)]
pub struct SuiteReport {
    pub name: String,

    pub fixture_count: CountSummary,
    pub duration_ns: u64,
    pub duration_ms: f64,

    pub setup: Option<FnSummary>,
    pub teardown: Option<FnSummary>,
    pub fixtures: Vec<FixtureReport>,
}

/// The report of one fixture, including setup/teardown summaries and case
/// reports.
#[derive(Serialize, Deserialize)]
pub struct FixtureReport {
    pub name: String,

    pub case_count: CountSummary,
    pub duration_ns: u64,
    pub duration_ms: f64,

    pub setup: Option<FnSummary>,
    pub teardown: Option<FnSummary>,
    pub cases: Vec<CaseReport>,
}

/// The report of one case.
#[derive(Serialize, Deserialize)]
pub struct CaseReport {
    pub name: String,

    pub passed: bool,
    pub ignored: bool,
    pub duration_ns: u64,
    pub duration_ms: f64,

    pub run: Option<FnSummary>,
}

/// The outcome of one function invocation (setup, teardown, or case).
///
/// `None` for the run summary of an ignored case.
#[derive(Debug, Serialize, Deserialize)]
pub struct FnSummary {
    pub result: FnResult,
    pub duration_ns: u64,
    pub duration_ms: f64,
}

/// A summary of pass/fail/ignored counts.
#[derive(Debug, Serialize, Deserialize)]
pub struct CountSummary {
    pub total: u64,
    pub passed: u64,
    pub failed: u64,
    pub ignored: u64,
}

impl CountSummary {
    /// Returns true when no case failed (ignored cases do not count).
    #[must_use]
    pub fn all_passed(&self) -> bool {
        self.failed == 0
    }
}

/// The result of a function invocation.
///
/// A panicked invocation is reported as `Panicked` even when it satisfies a
/// `ShouldPanic` tag (the case then passes).
#[derive(Debug, Serialize, Deserialize)]
pub enum FnResult {
    Ok,
    Err(String),
    Panicked,
}

impl FnResult {
    /// Returns true when the invocation completed successfully.
    #[must_use]
    pub fn is_ok(&self) -> bool {
        matches!(self, FnResult::Ok)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fn_result_is_ok() {
        assert!(FnResult::Ok.is_ok());
        assert!(!FnResult::Err(String::new()).is_ok());
        assert!(!FnResult::Panicked.is_ok());
    }

    #[test]
    fn count_summary_all_passed_ignores_ignored() {
        let passed = CountSummary {
            total: 3,
            passed: 3,
            failed: 0,
            ignored: 0,
        };
        assert!(passed.all_passed());

        let failed = CountSummary {
            total: 3,
            passed: 2,
            failed: 1,
            ignored: 0,
        };
        assert!(!failed.all_passed());

        let ignored = CountSummary {
            total: 3,
            passed: 2,
            failed: 0,
            ignored: 1,
        };
        assert!(ignored.all_passed());
    }
}
