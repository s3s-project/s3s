// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

//! Report gates for the end-to-end suites.

mod mint;
mod s3tests;

use anyhow::Result;
use clap::Subcommand;

/// Report gates for the end-to-end suites.
#[derive(Debug, Subcommand)]
pub(crate) enum Report {
    /// Check a mint log against the expected-failure baselines.
    Mint(mint::Mint),
    /// Summarize the s3-tests report and gate on the baselines.
    S3Tests(s3tests::S3Tests),
}

impl Report {
    pub(crate) fn run(self) -> Result<bool> {
        match self {
            Self::Mint(command) => command.run(),
            Self::S3Tests(command) => command.run(),
        }
    }
}
