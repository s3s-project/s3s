// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

//! Plan and run the scheduled fuzzing.
//!
//! The scheduled run fuzzes one target per day. The target list is read from
//! `fuzz/Cargo.toml` instead of being written out again in the workflow, so a
//! target added to the fuzz workspace cannot be left out of the rotation. A
//! target that drives the `s3s` crate is fuzzed with the minio codegen variant
//! on alternating rotation cycles, because that variant changes generated
//! `s3s` code.

use std::fs;
use std::path::Path;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, ensure};
use clap::{Parser, Subcommand};

use crate::repo_root;

/// Manifest declaring the fuzz targets.
const MANIFEST: &str = "fuzz/Cargo.toml";

/// Manifest path prefix of a fuzz target, as opposed to a helper binary.
const TARGETS_PREFIX: &str = "fuzz_targets/";

/// Feature selection of the minio codegen variant.
const MINIO: &str = "--features minio";

/// Seconds per day, for the UTC day number.
const SECONDS_PER_DAY: u64 = 86_400;

/// Plan and run the scheduled fuzzing.
#[derive(Debug, Subcommand)]
pub(crate) enum Fuzz {
    /// Print the target and the features of the day, without fuzzing.
    Plan(PlanArgs),
    /// Fuzz the target of the day through the fuzz workspace recipes.
    Run(RunArgs),
}

#[derive(Debug, Parser)]
pub(crate) struct PlanArgs {
    /// UTC day number since 1970-01-01; defaults to today.
    #[arg(long, value_name = "DAYS")]
    day: Option<u64>,
}

#[derive(Debug, Parser)]
pub(crate) struct RunArgs {
    /// UTC day number since 1970-01-01; defaults to today.
    #[arg(long, value_name = "DAYS")]
    day: Option<u64>,
    /// Time budget in seconds; defaults to the budget of the `ci-run` recipe.
    #[arg(long, value_name = "SECONDS")]
    budget: Option<u64>,
}

/// One fuzz target of the fuzz workspace.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Target {
    name: String,
    /// Whether the target drives the `s3s` crate, and so has a codegen variant.
    s3s_backed: bool,
}

/// What the scheduled run does on one day.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Plan {
    name: String,
    features: Option<&'static str>,
}

impl Plan {
    /// One line naming the target and its features.
    fn render(&self) -> String {
        match self.features {
            Some(features) => format!("{} {features}", self.name),
            None => self.name.clone(),
        }
    }
}

impl Fuzz {
    pub(crate) fn run(self) -> Result<bool> {
        let root = repo_root();
        match self {
            Fuzz::Plan(args) => {
                let plan = scheduled(&root, day_number(args.day)?)?;
                println!("{}", plan.render());
                Ok(true)
            }
            Fuzz::Run(args) => {
                let plan = scheduled(&root, day_number(args.day)?)?;
                println!("fuzzing {}", plan.render());
                let mut arguments = vec![
                    "-f".to_owned(),
                    "fuzz/justfile".to_owned(),
                    "ci-run".to_owned(),
                    plan.name.clone(),
                ];
                if plan.features.is_some() || args.budget.is_some() {
                    arguments.push(plan.features.unwrap_or_default().to_owned());
                }
                if let Some(budget) = args.budget {
                    arguments.push(budget.to_string());
                }
                let status = Command::new("just")
                    .args(&arguments)
                    .current_dir(&root)
                    .status()
                    .context("failed to run just")?;
                Ok(status.success())
            }
        }
    }
}

/// Selected day, or today in UTC.
fn day_number(day: Option<u64>) -> Result<usize> {
    let seconds = match day {
        Some(day) => return usize::try_from(day).context("the day number does not fit this platform"),
        None => SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .context("the system clock is before the Unix epoch")?
            .as_secs(),
    };
    usize::try_from(seconds / SECONDS_PER_DAY).context("the day number does not fit this platform")
}

/// What the scheduled run does on the given day of the workspace.
fn scheduled(root: &Path, day: usize) -> Result<Plan> {
    scheduled_for(day, &targets(root)?)
}

/// What the scheduled run does on the given day of a target list: one target in
/// rotation, and the minio codegen variant on alternating rotation cycles.
fn scheduled_for(day: usize, targets: &[Target]) -> Result<Plan> {
    ensure!(!targets.is_empty(), "no fuzz target to schedule");
    let target = &targets[day % targets.len()];
    let minio = target.s3s_backed && (day / targets.len()).is_multiple_of(2);
    Ok(Plan {
        name: target.name.clone(),
        features: minio.then_some(MINIO),
    })
}

/// Every fuzz target of the workspace, in manifest order.
fn targets(root: &Path) -> Result<Vec<Target>> {
    let manifest_path = root.join(MANIFEST);
    let manifest = fs::read_to_string(&manifest_path).with_context(|| format!("failed to read {}", manifest_path.display()))?;
    let entries = parse_manifest(&manifest);
    ensure!(!entries.is_empty(), "{MANIFEST} declares no fuzz target");
    let mut targets = Vec::with_capacity(entries.len());
    for (name, path) in entries {
        let source = root.join("fuzz").join(&path);
        let text = fs::read_to_string(&source).with_context(|| format!("failed to read {}", source.display()))?;
        targets.push(Target {
            name,
            s3s_backed: uses_s3s(&text),
        });
    }
    Ok(targets)
}

/// `[[bin]]` entries of a manifest that live under `fuzz_targets/`, as
/// `(name, path)` pairs, in declaration order.
fn parse_manifest(manifest: &str) -> Vec<(String, String)> {
    let mut targets = Vec::new();
    let mut name: Option<String> = None;
    for line in manifest.lines() {
        let line = line.trim();
        if line == "[[bin]]" {
            name = None;
        } else if let Some(value) = line.strip_prefix("name = ") {
            name = Some(value.trim_matches('"').to_owned());
        } else if let Some(value) = line.strip_prefix("path = ") {
            let path = value.trim_matches('"');
            if path.starts_with(TARGETS_PREFIX)
                && let Some(name) = name.take()
            {
                targets.push((name, path.to_owned()));
            }
        }
    }
    targets
}

/// Whether a fuzz target source drives the `s3s` crate. The word boundary keeps
/// `s3s_chunked::`, `s3s_multipart::` and `s3s_sigv4::` out of it.
fn uses_s3s(source: &str) -> bool {
    source.lines().any(|line| {
        line.match_indices("s3s::")
            .any(|(index, _)| !line[..index].ends_with(|c: char| c.is_alphanumeric() || c == '_'))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
[package]
name = "s3s-fuzz"

[[bin]]
name = "alpha"
path = "fuzz_targets/alpha.rs"

[[bin]]
name = "beta"
path = "fuzz_targets/beta.rs"

[[bin]]
name = "gen_corpus"
path = "bin/gen_corpus.rs"
"#;

    fn sample(backed: &[bool]) -> Vec<Target> {
        parse_manifest(SAMPLE)
            .into_iter()
            .zip(backed)
            .map(|((name, _), s3s_backed)| Target {
                name,
                s3s_backed: *s3s_backed,
            })
            .collect()
    }

    fn names(plan: &Result<Plan>) -> String {
        plan.as_ref().expect("the day is schedulable").name.clone()
    }

    #[test]
    fn parses_only_fuzz_targets() {
        let parsed: Vec<String> = parse_manifest(SAMPLE).into_iter().map(|(name, _)| name).collect();
        assert_eq!(parsed, vec!["alpha".to_owned(), "beta".to_owned()]);
    }

    #[test]
    fn keeps_crates_that_merely_start_with_s3s_out_of_the_variant() {
        assert!(uses_s3s("use s3s::dto::Bucket;"));
        assert!(!uses_s3s("use s3s_chunked::ChunkedStream;"));
        assert!(!uses_s3s("use s3s_multipart::Multipart;"));
        assert!(uses_s3s("fn f() -> s3s::StdError { todo!() }"));
    }

    #[test]
    fn rotates_one_target_per_day() {
        let targets = sample(&[true, false]);
        let picked: Vec<String> = (0..5).map(|day| names(&scheduled_for(day, &targets))).collect();
        assert_eq!(picked, vec!["alpha", "beta", "alpha", "beta", "alpha"]);
    }

    #[test]
    fn alternates_the_minio_variant_per_rotation_cycle() {
        let targets = sample(&[true, true]);
        let features: Vec<Option<&'static str>> = (0..6)
            .map(|day| scheduled_for(day, &targets).expect("the day is schedulable").features)
            .collect();
        assert_eq!(features, vec![Some(MINIO), Some(MINIO), None, None, Some(MINIO), Some(MINIO)]);
    }

    #[test]
    fn leaves_the_variant_off_targets_without_s3s() {
        let targets = sample(&[false, false]);
        for day in 0..6 {
            let plan = scheduled_for(day, &targets).expect("the day is schedulable");
            assert_eq!(plan.features, None, "day {day} picked {plan:?}");
        }
    }

    #[test]
    fn schedules_every_fuzz_target_of_the_workspace() {
        let root = repo_root();
        let listed = targets(&root).expect("the fuzz workspace manifest parses");
        let mut listed: Vec<String> = listed.into_iter().map(|target| target.name).collect();
        let mut on_disk: Vec<String> = fs::read_dir(root.join("fuzz/fuzz_targets"))
            .expect("the fuzz target directory exists")
            .map(|entry| entry.expect("a readable directory entry").path())
            .filter(|path| path.extension().is_some_and(|extension| extension == "rs"))
            .map(|path| path.file_stem().expect("a file stem").to_string_lossy().into_owned())
            .collect();
        listed.sort();
        on_disk.sort();
        assert_eq!(listed, on_disk, "every fuzz target on disk is in the rotation");
    }
}
