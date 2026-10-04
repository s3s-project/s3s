// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

//! Repository automation for the s3s workspace.
//!
//! Run it as `cargo run -p xtask -- <command>`. The `justfile` is the
//! human-facing entry point; recipes that need more than a single command line
//! delegate their logic here.

mod coverage;
mod crawl;
mod fuzz;
mod link_license;
mod mutants;
mod report;
mod spdx;
mod watch_pr;

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(name = "xtask", about = "Repository automation for s3s")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Summarize a `cargo-llvm-cov` JSON export as a per-package table.
    Coverage(coverage::Coverage),
    /// Crawl the AWS Smithy models and the S3 error code documentation.
    #[command(subcommand)]
    Crawl(crawl::Crawl),
    /// Plan and run the scheduled fuzzing.
    #[command(subcommand)]
    Fuzz(fuzz::Fuzz),
    /// Link the LICENSE file into every workspace member.
    LinkLicense(link_license::LinkLicense),
    /// Plan and run mutation testing sweeps.
    #[command(subcommand)]
    Mutants(mutants::Mutants),
    /// Report gates for the end-to-end suites.
    #[command(subcommand)]
    Report(report::Report),
    /// Check or insert SPDX license headers.
    #[command(subcommand)]
    Spdx(spdx::Spdx),
    /// Watch a pull request until its state changes.
    WatchPr(watch_pr::WatchPr),
}

fn main() -> ExitCode {
    let result = match Cli::parse().command {
        Command::Coverage(cmd) => cmd.run(),
        Command::Crawl(cmd) => cmd.run(),
        Command::Fuzz(cmd) => cmd.run(),
        Command::LinkLicense(cmd) => cmd.run(),
        Command::Mutants(cmd) => cmd.run(),
        Command::Report(cmd) => cmd.run(),
        Command::Spdx(cmd) => cmd.run(),
        Command::WatchPr(cmd) => cmd.run(),
    };
    match result {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::FAILURE,
        Err(error) => {
            eprintln!("{error:?}");
            ExitCode::FAILURE
        }
    }
}

/// Root of the checkout that contains this crate.
pub(crate) fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask is a top-level workspace member")
        .to_path_buf()
}
