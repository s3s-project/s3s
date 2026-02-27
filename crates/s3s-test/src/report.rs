use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize)]
pub struct Report {
    pub suite_count: CountSummary,
    pub duration_ns: u64,
    pub duration_ms: f64,

    pub suites: Vec<SuiteReport>,
}

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

#[derive(Serialize, Deserialize)]
pub struct CaseReport {
    pub name: String,

    pub passed: bool,
    pub ignored: bool,
    pub duration_ns: u64,
    pub duration_ms: f64,

    pub run: Option<FnSummary>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct FnSummary {
    pub result: FnResult,
    pub duration_ns: u64,
    pub duration_ms: f64,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct CountSummary {
    pub total: u64,
    pub passed: u64,
    pub failed: u64,
    pub ignored: u64,
}

impl CountSummary {
    #[must_use]
    pub fn all_passed(&self) -> bool {
        self.failed == 0
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub enum FnResult {
    Ok,
    Err(String),
    Panicked,
}

impl FnResult {
    #[must_use]
    pub fn is_ok(&self) -> bool {
        matches!(self, FnResult::Ok)
    }
}
