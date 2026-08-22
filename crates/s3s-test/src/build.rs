// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

use std::env;
use std::process::Command;

pub fn collect_info() {
    if let Some(val) = git_commit() {
        println!("cargo:rustc-env=S3S_GIT_COMMIT={val}");
    }
    if let Some(branch) = git_branch() {
        println!("cargo:rustc-env=S3S_GIT_BRANCH={branch}");
    }
    if let Some(tag) = git_tag() {
        println!("cargo:rustc-env=S3S_GIT_TAG={tag}");
    }
    if let Ok(val) = env::var("PROFILE") {
        println!("cargo:rustc-env=S3S_PROFILE={val}");
    }
}

#[must_use]
fn git(args: &[&str]) -> Option<String> {
    let output = Command::new("git").args(args).output().ok()?;
    if output.status.success() {
        Some(String::from_utf8_lossy(&output.stdout).trim().to_string())
    } else {
        None
    }
}

#[must_use]
pub fn git_commit() -> Option<String> {
    git(&["rev-parse", "HEAD"])
}

#[must_use]
pub fn git_branch() -> Option<String> {
    git(&["rev-parse", "--abbrev-ref", "HEAD"])
}

#[must_use]
pub fn git_tag() -> Option<String> {
    git(&["describe", "--tags", "--exact-match"])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_collect_info() {
        collect_info();
    }
}
