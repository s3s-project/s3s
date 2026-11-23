// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

//! Check the Docker build context against the workspace layout.
//!
//! `docker/Dockerfile` copies the workspace into the image path by path, so a
//! workspace member that lives outside those paths makes `cargo chef prepare`
//! fail before anything is built. The only run that would notice is the weekly
//! Docker workflow, which does not report to pull requests, so the breakage can
//! sit unseen for weeks. This check moves the same failure into the normal gate.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, ensure};
use clap::Subcommand;
use serde_json::Value;

use crate::repo_root;

const DOCKERFILE: &str = "docker/Dockerfile";

/// Check the Docker build context.
#[derive(Debug, PartialEq, Eq, Subcommand)]
pub(crate) enum Docker {
    /// Verify that every workspace member is inside the build context.
    Check,
}

impl Docker {
    pub(crate) fn run(self) -> Result<bool> {
        match self {
            Self::Check => check(),
        }
    }
}

fn check() -> Result<bool> {
    let root = repo_root();
    let path = root.join(DOCKERFILE);
    let text = std::fs::read_to_string(&path).with_context(|| format!("failed to read {}", path.display()))?;
    let sources = copy_sources(&text);
    ensure!(!sources.is_empty(), "{DOCKERFILE} copies nothing");

    let members = workspace_members(&root)?;
    let missing: Vec<String> = members
        .iter()
        .filter(|member| !is_covered(member, &sources))
        .cloned()
        .collect();

    if missing.is_empty() {
        println!("{DOCKERFILE} covers all {} workspace members", members.len());
        return Ok(true);
    }

    for member in &missing {
        eprintln!("workspace member missing from the Docker build context: {member}");
    }
    eprintln!("add a COPY line for each missing member to {DOCKERFILE}");
    Ok(false)
}

/// Local source paths of the `COPY` instructions, without the `./` prefix.
///
/// The options come first and never name a path in the build context, so they
/// are skipped. The last operand is the destination and every operand before it
/// is a source, which is how `COPY ./crates ./codegen ./sources/` copies two
/// trees. An instruction with `--from` reads out of another stage, so it
/// contributes nothing to the build context.
fn copy_sources(text: &str) -> Vec<String> {
    let mut sources: Vec<String> = Vec::new();
    for line in text.lines() {
        let mut tokens = line.split_whitespace();
        let Some(instruction) = tokens.next() else {
            continue;
        };
        if !instruction.eq_ignore_ascii_case("COPY") {
            continue;
        }
        let tokens: Vec<&str> = tokens.collect();
        let options = tokens.iter().take_while(|token| token.starts_with("--")).count();
        if tokens[..options].iter().any(|option| option.starts_with("--from=")) {
            continue;
        }
        let Some((_destination, operands)) = tokens[options..].split_last() else {
            continue;
        };
        for &source in operands {
            let source = source.strip_prefix("./").unwrap_or(source);
            if source.is_empty() || sources.iter().any(|existing| existing == source) {
                continue;
            }
            sources.push(source.to_owned());
        }
    }
    sources
}

/// Directory of every workspace member, relative to the repository root.
fn workspace_members(root: &Path) -> Result<Vec<String>> {
    let output = Command::new("cargo")
        .args(["metadata", "--no-deps", "--format-version", "1"])
        .current_dir(root)
        .output()
        .context("failed to run cargo metadata")?;
    ensure!(output.status.success(), "cargo metadata exited with {}", output.status);
    let metadata: Value = serde_json::from_slice(&output.stdout).context("failed to parse cargo metadata")?;
    let packages = metadata["packages"].as_array().context("cargo metadata lists no packages")?;

    let mut members: Vec<String> = Vec::with_capacity(packages.len());
    for package in packages {
        let manifest = package["manifest_path"].as_str().context("package without manifest_path")?;
        let dir: PathBuf = Path::new(manifest)
            .parent()
            .context("manifest path without a parent directory")?
            .to_path_buf();
        let relative = dir.strip_prefix(root).context("workspace member outside the repository")?;
        members.push(relative.to_string_lossy().replace('\\', "/"));
    }
    members.sort();
    Ok(members)
}

/// A member is covered when its directory is (or is inside) a copied path.
///
/// `COPY . .` covers the whole context, so a `.` source matches every member.
fn is_covered(member: &str, sources: &[String]) -> bool {
    let member = Path::new(member);
    sources
        .iter()
        .any(|source| source == "." || member.starts_with(Path::new(source)))
}

#[cfg(test)]
mod tests {
    use super::{copy_sources, is_covered};

    #[test]
    fn copy_sources_keeps_the_local_sources_in_order() {
        let text = "\
FROM scratch AS planner
COPY ./Cargo.toml ./Cargo.toml
COPY ./crates ./crates
COPY --from=planner /app/recipe.json recipe.json
copy ./xtask ./xtask
COPY ./crates ./crates
    ";
        assert_eq!(copy_sources(text), ["Cargo.toml", "crates", "xtask"]);
    }

    #[test]
    fn copy_sources_ignores_lines_that_are_not_copy_instructions() {
        let text = "# COPY ./commented ./commented\nRUN echo COPY ./run ./run\n";
        assert_eq!(copy_sources(text), Vec::<String>::new());
    }

    #[test]
    fn copy_sources_keeps_the_sources_of_a_copy_with_options() {
        let text = "COPY --chown=root:root ./xtask ./xtask\nCOPY --chmod=755 ./crates ./crates\n";
        assert_eq!(copy_sources(text), ["xtask", "crates"]);
    }

    #[test]
    fn copy_sources_records_every_operand_except_the_destination() {
        let text = "COPY ./crates ./codegen ./sources/\n";
        assert_eq!(copy_sources(text), ["crates", "codegen"]);
    }

    #[test]
    fn copy_sources_ignores_a_copy_from_another_stage() {
        let text = "COPY --from=planner /app/recipe.json recipe.json\n";
        assert_eq!(copy_sources(text), Vec::<String>::new());
    }

    #[test]
    fn a_copied_directory_covers_the_members_inside_it() {
        let sources = ["Cargo.toml".to_owned(), "crates".to_owned(), "codegen".to_owned()];
        assert!(is_covered("crates/s3s", &sources));
        assert!(is_covered("codegen", &sources));
        assert!(!is_covered("xtask", &sources));
    }

    #[test]
    fn a_copy_of_the_whole_context_covers_every_member() {
        let sources = [".".to_owned()];
        assert!(is_covered("crates/s3s", &sources));
        assert!(is_covered("xtask", &sources));
    }

    #[test]
    fn a_sibling_sharing_a_prefix_is_not_covered() {
        let sources = ["xtask".to_owned()];
        assert!(is_covered("xtask", &sources));
        assert!(!is_covered("xtask-extra", &sources));
    }
}
