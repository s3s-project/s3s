// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

//! Repository automation for the s3s workspace.
//!
//! Run it as `cargo run -p xtask -- <command>`. The `justfile` is the
//! human-facing entry point; recipes that need more than a single command line
//! delegate their logic here.

mod link_license;
mod spdx;

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
    /// Link the LICENSE file into every workspace member.
    LinkLicense(link_license::LinkLicense),
    /// Check or insert SPDX license headers.
    Spdx(spdx::Spdx),
}

fn main() -> ExitCode {
    let result = match Cli::parse().command {
        Command::LinkLicense(cmd) => cmd.run(),
        Command::Spdx(cmd) => cmd.run(),
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
