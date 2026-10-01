// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

//! Link the repository LICENSE file into every workspace member, including the
//! `publish = false` tool crates, so the checkout stays uniform.
//!
//! `cargo package` resolves the link, so each published package carries the
//! Apache-2.0 text.

use std::fs;
use std::io::ErrorKind;
use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, ensure};
use clap::Parser;
use serde_json::Value;

use crate::repo_root;

const LICENSE: &str = "LICENSE";

/// Link the LICENSE file into every workspace member.
#[derive(Debug, Parser)]
pub(crate) struct LinkLicense {
    /// Verify the links instead of creating them.
    #[arg(long)]
    check: bool,
}

/// State of one member's LICENSE entry.
enum State {
    /// The entry exists and resolves to the repository LICENSE file.
    Linked,
    /// The entry does not exist yet.
    Missing,
    /// The entry exists but does not resolve to the repository LICENSE file.
    Conflict(String),
}

impl LinkLicense {
    pub(crate) fn run(self) -> Result<bool> {
        let root = repo_root();
        let metadata = cargo_metadata(&root)?;
        let packages = metadata["packages"].as_array().context("cargo metadata lists no packages")?;

        let mut created = 0_usize;
        let mut linked = 0_usize;
        let mut problems: Vec<String> = Vec::new();

        for package in packages {
            let manifest = package["manifest_path"].as_str().context("package without manifest_path")?;
            let dir = Path::new(manifest)
                .parent()
                .context("manifest path without a parent directory")?;
            let target = relative_target(&root, dir)?;
            let entry = dir.join(LICENSE);
            match entry_state(&entry, &root, &target)? {
                State::Linked => linked += 1,
                State::Missing if self.check => {
                    problems.push(format!("{}: missing (expected a symlink to {target})", relative(&root, &entry)));
                }
                State::Missing => {
                    create_link(&entry, &target, &root.join(LICENSE))?;
                    println!("created {} -> {target}", relative(&root, &entry));
                    created += 1;
                }
                State::Conflict(reason) => {
                    problems.push(format!("{}: {reason}", relative(&root, &entry)));
                }
            }
        }

        if self.check {
            if problems.is_empty() {
                println!("all workspace members link the LICENSE file");
                return Ok(true);
            }
            println!("LICENSE link problems in {} crate(s):", problems.len());
            for problem in &problems {
                println!("{problem}");
            }
            return Ok(false);
        }

        println!("created {created}, already linked {linked}");
        if problems.is_empty() {
            return Ok(true);
        }
        println!("LICENSE link problems in {} crate(s):", problems.len());
        for problem in &problems {
            println!("{problem}");
        }
        Ok(false)
    }
}

/// Ask cargo for the workspace members.
fn cargo_metadata(root: &Path) -> Result<Value> {
    let output = Command::new("cargo")
        .args(["metadata", "--no-deps", "--format-version", "1"])
        .current_dir(root)
        .output()
        .context("failed to run cargo metadata")?;
    ensure!(output.status.success(), "cargo metadata exited with {}", output.status);
    serde_json::from_slice(&output.stdout).context("failed to parse cargo metadata")
}

/// Relative path from a member directory back to the repository LICENSE file.
fn relative_target(root: &Path, dir: &Path) -> Result<String> {
    let relative = dir.strip_prefix(root).context("workspace member outside the repository")?;
    Ok(format!("{}LICENSE", "../".repeat(relative.components().count())))
}

fn entry_state(entry: &Path, root: &Path, target: &str) -> Result<State> {
    let metadata = match fs::symlink_metadata(entry) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(State::Missing),
        Err(error) => return Err(error).with_context(|| format!("failed to stat {}", entry.display())),
    };

    if metadata.file_type().is_symlink() {
        let link = fs::read_link(entry).with_context(|| format!("failed to read {}", entry.display()))?;
        return Ok(if link == Path::new(target) {
            State::Linked
        } else {
            State::Conflict(format!("symlink points to {}, expected {target}", link.display()))
        });
    }

    let actual = fs::read(entry).with_context(|| format!("failed to read {}", entry.display()))?;
    let expected = fs::read(root.join(LICENSE)).context("failed to read the repository LICENSE")?;
    Ok(if actual == expected {
        State::Linked
    } else {
        State::Conflict("regular file differs from the repository LICENSE".to_owned())
    })
}

/// Create the link; a plain copy is the fallback where symlinks are unavailable.
#[cfg(unix)]
fn create_link(entry: &Path, target: &str, _source: &Path) -> Result<()> {
    std::os::unix::fs::symlink(target, entry).with_context(|| format!("failed to create {}", entry.display()))
}

#[cfg(not(unix))]
fn create_link(entry: &Path, _target: &str, source: &Path) -> Result<()> {
    fs::copy(source, entry)
        .map(|_| ())
        .with_context(|| format!("failed to copy {} to {}", source.display(), entry.display()))
}

/// Path shown in messages: relative to the repository root when possible.
fn relative(root: &Path, path: &Path) -> String {
    path.strip_prefix(root).unwrap_or(path).display().to_string()
}
