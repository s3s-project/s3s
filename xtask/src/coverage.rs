// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

//! Summarize a `cargo-llvm-cov` JSON export as a per-package table.
//!
//! Port of `scripts/coverage_summary.py`: the aggregation, the table layout and
//! the exit codes are kept as they were. The line metric printed here is the one
//! `--fail-under-lines` enforces, so the table can track a coverage goal locally
//! without turning it into a gate.

use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result, bail, ensure};
use clap::Parser;
use serde::Deserialize;

/// Print a `cargo-llvm-cov` JSON summary as a per-package coverage table.
#[derive(Debug, Parser)]
pub(crate) struct Coverage {
    /// JSON export written by `cargo llvm-cov --json --summary-only`.
    #[arg(value_name = "JSON")]
    json: PathBuf,
    /// Fail when the total line coverage is below this percentage.
    #[arg(long, value_name = "PERCENT")]
    fail_under_lines: Option<f64>,
}

impl Coverage {
    pub(crate) fn run(self) -> Result<bool> {
        let text = fs::read_to_string(&self.json).with_context(|| format!("failed to read {}", self.json.display()))?;
        let export: Export = serde_json::from_str(&text).context("failed to parse the llvm-cov export")?;
        let report = Report::new(&export)?;
        println!("{}", report.render());
        if !report.unmapped.is_empty() {
            println!("warning: {} files outside the workspace were skipped:", report.unmapped.len());
            for filename in &report.unmapped {
                println!("  - {filename}");
            }
        }
        if let Some(threshold) = self.fail_under_lines {
            let percent = report.totals.lines.percent();
            if percent < threshold {
                println!("error: line coverage {percent:.2}% is below {threshold}%");
                return Ok(false);
            }
        }
        Ok(true)
    }
}

/// The parts of the export this command reads.
#[derive(Debug, Deserialize)]
struct Export {
    data: Vec<Data>,
}

/// One entry of `data`; only the first one is used.
#[derive(Debug, Deserialize)]
struct Data {
    files: Option<Vec<FileEntry>>,
}

/// One measured source file.
#[derive(Debug, Deserialize)]
struct FileEntry {
    filename: String,
    summary: Summary,
}

/// Per-metric counters of one file or package.
#[derive(Debug, Default, Clone, Copy, Deserialize)]
struct Summary {
    #[serde(default)]
    lines: Counters,
    #[serde(default)]
    regions: Counters,
    #[serde(default)]
    functions: Counters,
    #[serde(default)]
    branches: Counters,
}

impl Summary {
    fn add(&mut self, other: Self) {
        self.lines.add(other.lines);
        self.regions.add(other.regions);
        self.functions.add(other.functions);
        self.branches.add(other.branches);
    }
}

/// Covered and total counts of one metric.
#[derive(Debug, Default, Clone, Copy, Deserialize)]
struct Counters {
    #[serde(default)]
    count: u64,
    #[serde(default)]
    covered: u64,
}

impl Counters {
    fn add(&mut self, other: Self) {
        self.count += other.count;
        self.covered += other.covered;
    }

    #[expect(clippy::cast_precision_loss, reason = "line counts stay far below 2^53")]
    fn percent(self) -> f64 {
        if self.count == 0 {
            return 0.0;
        }
        100.0 * self.covered as f64 / self.count as f64
    }
}

/// The aggregated table plus the paths that were skipped.
#[derive(Debug)]
struct Report {
    packages: Vec<(String, Summary)>,
    totals: Summary,
    unmapped: Vec<String>,
}

impl Report {
    fn new(export: &Export) -> Result<Self> {
        let Some(files) = export.data.first().and_then(|data| data.files.as_deref()) else {
            bail!("report has no data[0].files (use --json --summary-only)");
        };
        ensure!(!files.is_empty(), "report contains zero files");

        let mut packages: BTreeMap<String, Summary> = BTreeMap::new();
        let mut unmapped = Vec::new();
        for entry in files {
            let Some(package) = package_of(&entry.filename) else {
                unmapped.push(entry.filename.clone());
                continue;
            };
            packages.entry(package).or_default().add(entry.summary);
        }

        let mut totals = Summary::default();
        let mut ordered: Vec<(String, Summary)> = packages.into_iter().collect();
        for (_, summary) in &ordered {
            totals.add(*summary);
        }
        ordered.sort_by(|left, right| right.1.lines.percent().total_cmp(&left.1.lines.percent()));
        Ok(Self {
            packages: ordered,
            totals,
            unmapped,
        })
    }

    fn render(&self) -> String {
        let mut rows = vec![format!(
            "{:<16}{:>9}{:>9}{:>9}{:>11}{:>13}{:>12}",
            "package", "lines", "covered", "lines %", "regions %", "functions %", "branches %"
        )];
        for (name, summary) in &self.packages {
            rows.push(table_row(name, summary));
        }
        rows.push(table_row("TOTAL", &self.totals));
        rows.join("\n")
    }
}

/// Map a source path to the package it belongs to.
fn package_of(filename: &str) -> Option<String> {
    let parts: Vec<&str> = filename.split('/').collect();
    for (index, part) in parts.iter().enumerate() {
        if *part == "crates" && index + 1 < parts.len() {
            return Some(parts[index + 1].to_owned());
        }
        if *part == "codegen" {
            return Some("s3s-codegen".to_owned());
        }
        if *part == "xtask" {
            return Some("xtask".to_owned());
        }
    }
    None
}

/// Render one table row.
fn table_row(name: &str, summary: &Summary) -> String {
    let lines = summary.lines.percent();
    let regions = summary.regions.percent();
    let functions = summary.functions.percent();
    let branches = summary.branches.percent();
    format!(
        "{name:<16}{count:>9}{covered:>9}{lines:>8.2}%{regions:>10.2}%{functions:>12.2}%{branches:>11.2}%",
        count = summary.lines.count,
        covered = summary.lines.covered,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"{"data":[{"files":[
        {"filename":"/repo/crates/alpha/src/a.rs","summary":{"lines":{"count":10,"covered":5}}},
        {"filename":"/repo/crates/alpha/src/b.rs","summary":{"lines":{"count":10,"covered":10}}},
        {"filename":"/repo/crates/beta/src/lib.rs","summary":{"lines":{"count":4,"covered":4}}},
        {"filename":"/elsewhere/lib.rs","summary":{"lines":{"count":1,"covered":1}}}
    ]}]}"#;

    fn report(json: &str) -> Result<Report> {
        let export: Export = serde_json::from_str(json)?;
        Report::new(&export)
    }

    fn package(report: &Report, name: &str) -> Summary {
        report
            .packages
            .iter()
            .find(|(package, _)| package == name)
            .map(|(_, summary)| *summary)
            .expect("the sample report contains this package")
    }

    #[test]
    fn aggregates_files_per_package() {
        let report = report(SAMPLE).expect("the sample report parses");
        let alpha = package(&report, "alpha");
        assert_eq!(alpha.lines.count, 20);
        assert_eq!(alpha.lines.covered, 15);
        assert!((alpha.lines.percent() - 75.0).abs() < f64::EPSILON);
        assert!((package(&report, "beta").lines.percent() - 100.0).abs() < f64::EPSILON);
    }

    #[test]
    fn surfaces_paths_outside_the_workspace() {
        let report = report(SAMPLE).expect("the sample report parses");
        assert_eq!(report.unmapped, vec!["/elsewhere/lib.rs".to_owned()]);
    }

    #[test]
    fn table_lists_the_packages_and_a_total_row() {
        let table = report(SAMPLE).expect("the sample report parses").render();
        assert!(table.contains("TOTAL"));
        assert!(table.contains("alpha"));
        assert_eq!(table.lines().count(), 4);
    }

    #[test]
    fn rejects_a_report_without_files() {
        assert!(report(r#"{"data":[]}"#).is_err());
        assert!(report(r#"{"data":[{}]}"#).is_err());
    }

    #[test]
    fn rejects_a_report_with_zero_files() {
        assert!(report(r#"{"data":[{"files":[]}]}"#).is_err());
    }

    #[test]
    fn zero_lines_is_zero_percent() {
        assert!(Counters::default().percent().abs() < f64::EPSILON);
    }
}
