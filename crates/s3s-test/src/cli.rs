// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

use std::ops::Not;
use std::path::Path;
use std::path::PathBuf;
use std::process::ExitCode;

use crate::report::FnResult;
use crate::report::Report;
use crate::tcx::TestContext;

use colored::ColoredString;
use colored::Colorize;
use regex::RegexSet;

type StdError = Box<dyn std::error::Error + Send + Sync + 'static>;

#[doc(hidden)]
pub use clap;

#[doc(hidden)]
pub use const_str;

/// The CLI options understood by [`main`] and the [`main!`](crate::main) macro.
#[doc(hidden)]
pub struct Options {
    /// Path to write the JSON report to.
    pub json: Option<PathBuf>,
    /// Regex patterns matching `suite/fixture/case` paths.
    pub filter: Vec<String>,
    /// Print all registered cases without running them.
    pub list: bool,
    /// Run cases tagged [`CaseTag::Ignored`](crate::CaseTag::Ignored).
    pub run_ignored: bool,
    /// Run cases within a fixture concurrently.
    pub concurrent: bool,
}

/// Initializes the environment: loads `.env`, then sets up the tracing
/// subscriber with an `EnvFilter` from `RUST_LOG`.
#[doc(hidden)]
pub fn setup() {
    use std::io::IsTerminal;
    use tracing_subscriber::EnvFilter;

    dotenvy::dotenv().ok();

    let env_filter = EnvFilter::from_default_env();
    let enable_color = std::io::stdout().is_terminal();

    tracing_subscriber::fmt()
        .pretty()
        .with_env_filter(env_filter)
        .with_ansi(enable_color)
        .init();
}

fn status(passed: bool, ignored: bool) -> ColoredString {
    if ignored {
        "IGNORED".yellow()
    } else if passed {
        "PASSED".green()
    } else {
        "FAILED".red()
    }
}

fn write_report(json_path: &Path, report: &Report) -> Result<(), StdError> {
    let report_json = serde_json::to_string_pretty(&report)?;
    std::fs::write(json_path, report_json)?;
    Ok(())
}

fn print_summary(report: &Report) {
    let w = format!("{:.3}", report.duration_ms).len();

    for suite in &report.suites {
        let suite_name = suite.name.as_str().magenta();
        for fixture in &suite.fixtures {
            let fixture_name = fixture.name.as_str().blue();
            for case in &fixture.cases {
                let case_name = case.name.as_str().cyan();
                let st = status(case.passed, case.ignored);
                let duration = case.duration_ms;
                println!("{st} {duration:>w$.3}ms [{suite_name}/{fixture_name}/{case_name}]");
                if !case.passed
                    && !case.ignored
                    && let Some(ref run) = case.run
                {
                    let hint = match run.result {
                        FnResult::Ok => "".normal(),
                        FnResult::Err(_) => "ERROR".red(),
                        FnResult::Panicked => "PANICKED".red().bold(),
                    };
                    let msg = if let FnResult::Err(ref e) = run.result {
                        e.as_str()
                    } else {
                        ""
                    };
                    println!("  {hint} {msg}");
                }
            }
            let st = status(fixture.case_count.all_passed(), false);
            let duration = fixture.duration_ms;
            println!("{st} {duration:>w$.3}ms [{suite_name}/{fixture_name}]");
        }
        let st = status(suite.fixture_count.all_passed(), false);
        let duration = suite.duration_ms;
        println!("{st} {duration:>w$.3}ms [{suite_name}]");
    }
    let st = status(report.suite_count.all_passed(), false);
    let duration = report.duration_ms;
    println!("{st} {duration:>w$.3}ms");
}

#[tokio::main]
async fn async_main(reg: impl FnOnce(&mut TestContext), opt: &Options) -> ExitCode {
    let mut tcx = TestContext::new();
    reg(&mut tcx);

    if opt.filter.is_empty().not() {
        let filter_set = match RegexSet::new(&opt.filter) {
            Ok(x) => x,
            Err(err) => {
                eprintln!("Failed to build filter set: {err}");
                return ExitCode::from(2);
            }
        };
        tcx.filter(&filter_set);
    }

    if opt.run_ignored {
        tcx.include_ignored();
    }

    if opt.list {
        for suite in tcx.suites.values() {
            let suite_name = suite.name.magenta();
            println!("{suite_name}");
            for fixture in suite.fixtures.values() {
                let fixture_name = fixture.name.blue();
                println!("{suite_name}/{fixture_name}");
                for case in fixture.cases.values() {
                    let case_name = case.name.cyan();
                    println!("{suite_name}/{fixture_name}/{case_name}");
                }
            }
        }
        return ExitCode::from(0);
    }

    let report = crate::runner::run(&mut tcx, opt.concurrent).await;

    if let Some(ref json_path) = opt.json
        && let Err(err) = write_report(json_path, &report)
    {
        eprintln!("Failed to write report: {err}");
        return ExitCode::from(2);
    }

    print_summary(&report);

    if report.suite_count.all_passed() {
        ExitCode::from(0)
    } else {
        ExitCode::from(1)
    }
}

/// Runs the harness and returns the process exit code.
///
/// The exit code is nonzero when any suite fails. Use this function directly
/// for a custom entry point, or [`main!`](crate::main) for the standard CLI.
#[doc(hidden)]
#[must_use]
pub fn main(reg: impl FnOnce(&mut TestContext), opt: &Options) -> ExitCode {
    setup();
    async_main(reg, opt)
}

#[doc(hidden)]
#[must_use]
pub const fn unwrap<'a>(s: Option<&'a str>, default: &'a str) -> &'a str {
    match s {
        Some(s) => s,
        None => default,
    }
}

/// Generates the binary entry point for a test harness.
///
/// The macro expands to a `main` function that parses the standard CLI
/// (`--filter`, `--list`, `--json`, `--run-ignored`, `--concurrent`),
/// registers the suites via the given function, and runs them.
///
/// ```no_run
/// use s3s_test::tcx::TestContext;
///
/// fn register(_tcx: &mut TestContext) {}
///
/// s3s_test::main!(register);
/// ```
#[macro_export]
macro_rules! main {
    ($register:expr) => {
        use s3s_test::cli::clap;

        const LONG_VERSION: &str = {
            use s3s_test::cli::const_str;
            use s3s_test::cli::unwrap;
            const PKG_VERSION: &str = env!("CARGO_PKG_VERSION");
            const GIT_COMMIT: &str = unwrap(option_env!("S3S_GIT_COMMIT"), "-");
            const GIT_BRANCH: &str = unwrap(option_env!("S3S_GIT_BRANCH"), "-");
            const GIT_TAG: &str = unwrap(option_env!("S3S_GIT_TAG"), "-");
            const PROFILE: &str = unwrap(option_env!("S3S_PROFILE"), "-");
            const_str::format!(
                "{}\nbranch: {}\ncommit: {}\ntag: {}\nprofile: {}",
                PKG_VERSION,
                GIT_BRANCH,
                GIT_COMMIT,
                GIT_TAG,
                PROFILE
            )
        };

        #[derive(Debug, clap::Parser)]
        #[clap(version, long_version = LONG_VERSION)]
        struct Opt {
            #[clap(long)]
            json: Option<::std::path::PathBuf>,

            #[clap(long)]
            filter: Vec<::std::string::String>,

            #[clap(long)]
            list: bool,

            #[clap(long)]
            run_ignored: bool,

            #[clap(long)]
            concurrent: bool,
        }

        fn main() -> impl ::std::process::Termination {
            use clap::Parser as _;
            let opt = Opt::parse();
            s3s_test::cli::main(
                $register,
                &s3s_test::cli::Options {
                    json: opt.json,
                    filter: opt.filter,
                    list: opt.list,
                    run_ignored: opt.run_ignored,
                    concurrent: opt.concurrent,
                },
            )
        }
    };
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::*;

    use std::sync::Arc;

    async fn ok_case(_: Arc<MockFixture>) -> crate::Result {
        Ok(())
    }

    async fn fail_case(_: Arc<MockFixture>) -> crate::Result {
        Err(crate::Failed::from_string("fail"))
    }

    async fn panic_case(_: Arc<MockFixture>) -> crate::Result {
        panic!("boom");
    }

    fn options(json: Option<PathBuf>, filter: Vec<String>, list: bool) -> Options {
        Options {
            json,
            filter,
            list,
            run_ignored: false,
            concurrent: false,
        }
    }

    fn register_ok(tcx: &mut TestContext) {
        let mut suite = tcx.suite::<MockSuite>("suite");
        let mut fixture = suite.fixture::<MockFixture>("fixture");
        fixture.case("ok", ok_case);
    }

    fn register_ok_and_fail(tcx: &mut TestContext) {
        let mut suite = tcx.suite::<MockSuite>("suite");
        let mut fixture = suite.fixture::<MockFixture>("fixture");
        fixture.case("ok", ok_case);
        fixture.case("fail", fail_case);
    }

    fn register_panic(tcx: &mut TestContext) {
        let mut suite = tcx.suite::<MockSuite>("suite");
        let mut fixture = suite.fixture::<MockFixture>("fixture");
        fixture.case("panics", panic_case);
    }

    fn register_ignored_fail(tcx: &mut TestContext) {
        use crate::tcx::CaseTag;

        let mut suite = tcx.suite::<MockSuite>("suite");
        let mut fixture = suite.fixture::<MockFixture>("fixture");
        fixture.case("ignored", fail_case).tag(CaseTag::Ignored);
    }

    #[test]
    fn run_success_returns_zero_and_writes_json() {
        let json_path = std::env::temp_dir().join(format!("s3s-test-report-{}.json", std::process::id()));
        let opt = options(Some(json_path.clone()), Vec::new(), false);

        let code = async_main(register_ok, &opt);
        assert_eq!(code, ExitCode::from(0));

        let text = std::fs::read_to_string(&json_path).unwrap();
        let report: crate::report::Report = serde_json::from_str(&text).unwrap();
        assert!(report.suite_count.all_passed());
        std::fs::remove_file(&json_path).ok();
    }

    #[test]
    fn failing_case_returns_nonzero() {
        let opt = options(None, Vec::new(), false);
        let code = async_main(register_ok_and_fail, &opt);
        assert_eq!(code, ExitCode::from(1));
    }

    #[test]
    fn panicking_case_returns_nonzero() {
        let opt = options(None, Vec::new(), false);
        let code = async_main(register_panic, &opt);
        assert_eq!(code, ExitCode::from(1));
    }

    #[test]
    fn ignored_case_is_skipped_by_default() {
        let opt = options(None, Vec::new(), false);
        let code = async_main(register_ignored_fail, &opt);
        assert_eq!(code, ExitCode::from(0));
    }

    #[test]
    fn run_ignored_runs_ignored_cases() {
        let opt = Options {
            json: None,
            filter: Vec::new(),
            list: false,
            run_ignored: true,
            concurrent: false,
        };
        let code = async_main(register_ignored_fail, &opt);
        assert_eq!(code, ExitCode::from(1));
    }

    #[test]
    fn list_mode_returns_zero_without_running() {
        let opt = options(None, Vec::new(), true);
        let code = async_main(register_ok_and_fail, &opt);
        assert_eq!(code, ExitCode::from(0));
    }

    #[test]
    fn filter_runs_only_matching_cases() {
        let opt = options(None, vec![String::from("ok")], false);
        let code = async_main(register_ok_and_fail, &opt);
        assert_eq!(code, ExitCode::from(0));
    }

    #[test]
    fn invalid_filter_returns_error_code() {
        let opt = options(None, vec![String::from("[")], false);
        let code = async_main(register_ok, &opt);
        assert_eq!(code, ExitCode::from(2));
    }

    #[test]
    fn setup_initializes_tracing() {
        setup();
    }
}
