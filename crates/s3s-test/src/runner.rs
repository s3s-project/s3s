// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

#![allow(clippy::wildcard_imports)]
#![allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]

use crate::report::*;
use crate::tcx::*;

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Instant;

use tokio::spawn;
use tokio::task::JoinHandle;
use tracing::Instrument;
use tracing::info;
use tracing::info_span;
use tracing::instrument;

type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

macro_rules! run_fn {
    ($call:expr) => {{
        let t0 = std::time::Instant::now();
        let result = spawn($call.in_current_span()).await;
        let duration_ns = t0.elapsed().as_nanos() as u64;
        let duration_ms = duration_ns as f64 / 1e6;
        let summary = match result {
            Ok(Ok(_)) => FnSummary {
                result: FnResult::Ok,
                duration_ns,
                duration_ms,
            },
            Ok(Err(ref e)) => FnSummary {
                result: FnResult::Err(e.to_string()),
                duration_ns,
                duration_ms,
            },
            Err(ref e) if e.is_panic() => FnSummary {
                result: FnResult::Panicked,
                duration_ns,
                duration_ms,
            },
            Err(ref e) => FnSummary {
                result: FnResult::Err(e.to_string()),
                duration_ns,
                duration_ms,
            },
        };
        (result, summary)
    }};
}

fn count(total: u64, iter: impl IntoIterator<Item = bool>) -> CountSummary {
    let mut passed = 0;
    let mut failed = 0;
    for p in iter {
        if p {
            passed += 1;
        } else {
            failed += 1;
        }
    }
    CountSummary {
        total,
        passed,
        failed,
        ignored: 0,
    }
}

fn count_cases(total: u64, iter: impl IntoIterator<Item = (bool, bool)>) -> CountSummary {
    let mut passed = 0;
    let mut failed = 0;
    let mut ignored = 0;
    for (p, i) in iter {
        if i {
            ignored += 1;
        } else if p {
            passed += 1;
        } else {
            failed += 1;
        }
    }
    CountSummary {
        total,
        passed,
        failed,
        ignored,
    }
}

pub async fn run(tcx: &mut TestContext, concurrent: bool) -> Report {
    let total_suites = tcx.suites.len();
    info!(total_suites, "Test start");

    let mut suites: Vec<SuiteReport> = Vec::with_capacity(tcx.suites.len());

    let t0 = Instant::now();

    for suite in tcx.suites.values() {
        let report = run_suite(suite, concurrent).await;
        suites.push(report);
    }

    let duration_ns = t0.elapsed().as_nanos() as u64;
    let suite_count = count(total_suites as u64, suites.iter().map(|r| r.fixture_count.all_passed()));

    info!(duration_ns, ?suite_count, "Test end");

    Report {
        suite_count,
        duration_ns,
        duration_ms: duration_ns as f64 / 1e6,
        suites,
    }
}

#[instrument(skip(suite), fields(name = suite.name))]
async fn run_suite(suite: &SuiteInfo, concurrent: bool) -> SuiteReport {
    let total_fixtures = suite.fixtures.len();
    info!(total_fixtures, "Test suite start");

    let setup_summary;
    let mut teardown_summary = None;
    let mut fixtures: Vec<FixtureReport> = Vec::with_capacity(suite.fixtures.len());

    let t0 = Instant::now();

    'run: {
        let (result, summary) = run_fn!((suite.setup)());
        setup_summary = Some(summary);
        let Ok(Ok(suite_data)) = result else { break 'run };

        for fixture in suite.fixtures.values() {
            let report = run_fixture(fixture, &suite_data, concurrent).await;
            fixtures.push(report);
        }

        let (_, summary) = run_fn!((suite.teardown)(suite_data));
        teardown_summary = Some(summary);
    }

    let duration_ns = t0.elapsed().as_nanos() as u64;
    let fixture_count = count(total_fixtures as u64, fixtures.iter().map(|r| r.case_count.all_passed()));

    info!(duration_ns, ?fixture_count, "Test suite end");

    SuiteReport {
        name: suite.name.clone(),
        setup: setup_summary,
        teardown: teardown_summary,
        fixture_count,
        duration_ns,
        duration_ms: duration_ns as f64 / 1e6,
        fixtures,
    }
}

enum CaseHandle {
    Ignored(CaseReport),
    Running(JoinHandle<CaseReport>),
}

#[allow(clippy::similar_names)]
async fn run_case(case_name: String, case_future: BoxFuture<'static, crate::Result>, should_panic: bool) -> CaseReport {
    info!("Test case start");
    let t0 = Instant::now();

    let result = spawn(case_future.in_current_span()).await;
    let elapsed_ns = t0.elapsed().as_nanos() as u64;
    let elapsed_ms = elapsed_ns as f64 / 1e6;

    let (passed, result) = match result {
        Ok(Ok(())) => (!should_panic, FnResult::Ok),
        Ok(Err(ref e)) => (!should_panic, FnResult::Err(e.to_string())),
        Err(ref e) if e.is_panic() => (should_panic, FnResult::Panicked),
        Err(ref e) => (false, FnResult::Err(e.to_string())),
    };
    let summary = FnSummary {
        result,
        duration_ns: elapsed_ns,
        duration_ms: elapsed_ms,
    };

    info!(?summary, "Test case end");

    CaseReport {
        name: case_name,
        passed,
        ignored: false,
        duration_ns: elapsed_ns,
        duration_ms: elapsed_ms,
        run: Some(summary),
    }
}

fn ignored_report(case: &CaseInfo) -> CaseReport {
    CaseReport {
        name: case.name.clone(),
        passed: true,
        ignored: true,
        duration_ns: 0,
        duration_ms: 0.0,
        run: None,
    }
}

#[instrument(skip(fixture, suite_data), fields(name = fixture.name))]
async fn run_fixture(fixture: &FixtureInfo, suite_data: &ArcAny, concurrent: bool) -> FixtureReport {
    let total_cases = fixture.cases.len();
    info!(total_cases, "Test fixture start");

    let setup_summary;
    let mut teardown_summary = None;
    let mut cases: Vec<CaseReport> = Vec::with_capacity(fixture.cases.len());

    let t0 = Instant::now();

    'run: {
        info!("Test fixture setup");
        let (result, summary) = run_fn!((fixture.setup)(Arc::clone(suite_data)));
        setup_summary = Some(summary);
        let Ok(Ok(fixture_data)) = result else { break 'run };

        if concurrent {
            let mut handles: Vec<CaseHandle> = Vec::with_capacity(fixture.cases.len());

            for case in fixture.cases.values() {
                if case.tags.contains(&CaseTag::Ignored) {
                    info!(name = case.name, "Test case ignored");
                    handles.push(CaseHandle::Ignored(ignored_report(case)));
                    continue;
                }

                let should_panic = case.tags.contains(&CaseTag::ShouldPanic);
                let case_name = case.name.clone();
                let case_future = (case.run)(Arc::clone(&fixture_data));
                let span = info_span!("case", name = case_name.as_str());

                let handle = spawn(run_case(case_name, case_future, should_panic).instrument(span));
                handles.push(CaseHandle::Running(handle));
            }

            for handle in handles {
                cases.push(match handle {
                    CaseHandle::Ignored(report) => report,
                    CaseHandle::Running(h) => h.await.unwrap_or_else(|_| CaseReport {
                        name: String::from("<unknown>"),
                        passed: false,
                        ignored: false,
                        duration_ns: 0,
                        duration_ms: 0.0,
                        run: Some(FnSummary {
                            result: FnResult::Panicked,
                            duration_ns: 0,
                            duration_ms: 0.0,
                        }),
                    }),
                });
            }
        } else {
            for case in fixture.cases.values() {
                if case.tags.contains(&CaseTag::Ignored) {
                    info!(name = case.name, "Test case ignored");
                    cases.push(ignored_report(case));
                    continue;
                }

                let should_panic = case.tags.contains(&CaseTag::ShouldPanic);
                let case_name = case.name.clone();
                let case_future = (case.run)(Arc::clone(&fixture_data));
                let span = info_span!("case", name = case_name.as_str());

                let report = run_case(case_name, case_future, should_panic).instrument(span).await;
                cases.push(report);
            }
        }

        info!("Test fixture teardown");
        let (_, summary) = run_fn!((fixture.teardown)(fixture_data));
        teardown_summary = Some(summary);
    }

    let duration_ns = t0.elapsed().as_nanos() as u64;
    let case_count = count_cases(total_cases as u64, cases.iter().map(|r| (r.passed, r.ignored)));

    info!(duration_ns, ?case_count, "Test fixture end");

    FixtureReport {
        name: fixture.name.clone(),
        setup: setup_summary,
        teardown: teardown_summary,
        case_count,
        duration_ns,
        duration_ms: duration_ns as f64 / 1e6,
        cases,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tcx::TestContext;

    use std::sync::Arc;

    struct TestSuiteType;

    impl crate::traits::TestSuite for TestSuiteType {
        fn setup() -> impl Future<Output = crate::Result<Self>> + Send + 'static {
            std::future::ready(Ok(Self))
        }
    }

    struct TestFixtureType;

    impl crate::traits::TestFixture<TestSuiteType> for TestFixtureType {
        fn setup(_: Arc<TestSuiteType>) -> impl Future<Output = crate::Result<Self>> + Send + 'static {
            std::future::ready(Ok(Self))
        }
    }

    async fn ok_case(_: Arc<TestFixtureType>) -> crate::Result {
        Ok(())
    }

    async fn panic_case(_: Arc<TestFixtureType>) -> crate::Result {
        panic!("boom");
    }

    fn register_case(
        name: &str,
        case: impl crate::traits::TestCase<TestFixtureType, TestSuiteType> + 'static,
        should_panic: bool,
    ) -> TestContext {
        let mut tcx = TestContext::new();
        let mut suite = tcx.suite::<TestSuiteType>("suite");
        let mut fixture = suite.fixture::<TestFixtureType>("fixture");
        let mut builder = fixture.case(name, case);
        if should_panic {
            builder.tag(CaseTag::ShouldPanic);
        }
        tcx
    }

    async fn run_case_passed(tcx: &mut TestContext) -> bool {
        let report = run(tcx, false).await;
        let suite = &report.suites[0];
        let fixture = &suite.fixtures[0];
        fixture.cases[0].passed
    }

    #[tokio::test]
    async fn should_panic_tag_with_panic_passes() {
        let mut tcx = register_case("panics", panic_case, true);
        assert!(run_case_passed(&mut tcx).await, "should_panic case that panics should pass");
    }

    #[tokio::test]
    async fn should_panic_tag_without_panic_fails() {
        let mut tcx = register_case("no-panic", ok_case, true);
        assert!(!run_case_passed(&mut tcx).await, "should_panic case that does not panic should fail");
    }

    #[tokio::test]
    async fn untagged_case_with_panic_fails() {
        let mut tcx = register_case("panics", panic_case, false);
        assert!(!run_case_passed(&mut tcx).await, "untagged case that panics should fail");
    }

    #[tokio::test]
    async fn untagged_case_without_panic_passes() {
        let mut tcx = register_case("ok", ok_case, false);
        assert!(run_case_passed(&mut tcx).await, "untagged case that returns Ok should pass");
    }
}
