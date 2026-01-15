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

#[doc(hidden)]
pub struct Options {
    pub json: Option<PathBuf>,
    pub filter: Vec<String>,
    pub list: bool,
}

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

fn status(passed: bool) -> ColoredString {
    if passed { "PASSED".green() } else { "FAILED".red() }
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
                let status = status(case.passed);
                let duration = case.duration_ms;
                println!("{status} {duration:>w$.3}ms [{suite_name}/{fixture_name}/{case_name}]");
                if !case.passed
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
            let status = status(fixture.case_count.all_passed());
            let duration = fixture.duration_ms;
            println!("{status} {duration:>w$.3}ms [{suite_name}/{fixture_name}]");
        }
        let status = status(suite.fixture_count.all_passed());
        let duration = suite.duration_ms;
        println!("{status} {duration:>w$.3}ms [{suite_name}]");
    }
    let status = status(report.suite_count.all_passed());
    let duration = report.duration_ms;
    println!("{status} {duration:>w$.3}ms");
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

    let report = crate::runner::run(&mut tcx).await;

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
                },
            )
        }
    };
}
