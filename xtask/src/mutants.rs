// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

//! Plan and run mutation testing sweeps.
//!
//! The sweep is budgeted and rotating, like the scheduled fuzz run: one run takes
//! the files that are due, spends a wall-clock budget on them and records what it
//! swept in a cursor that travels as a CI artifact. Candidates come from
//! `cargo mutants --list`, so the rotation follows the workspace.
//!
//! Two choices are easy to get wrong: the cursor stores a content digest rather
//! than an mtime, because a checkout rewrites mtimes; and survivors are a result,
//! not a failure, because cargo-mutants exits 2 or 3 on a useful run.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::LazyLock;
use std::thread::sleep;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, ensure};
use clap::{Parser, Subcommand};
use regex::Regex;
use serde::{Deserialize, Serialize};

use crate::repo_root;

/// Default configuration file, relative to the workspace root.
const CONFIG: &str = ".cargo/mutants.toml";

/// Default cursor, relative to the workspace root.
const CURSOR: &str = ".ci-state/mutants-cursor.json";

/// Default outcome directory of a sweep.
const OUT: &str = "mutants.out";

/// Assumed seconds per mutant on a CI runner; retune after the first runs.
const SECONDS_PER_MUTANT: u64 = 25;

/// Assumed fixed cost of one `cargo mutants` call: clean build plus baseline.
const FIXED_SECONDS: u64 = 60;

/// Address-space cap for a sweep. A mutant can make a test allocate without limit,
/// so the sweep limits itself and everything it spawns.
#[cfg(unix)]
const ADDRESS_SPACE_LIMIT: u64 = 8 * 1024 * 1024 * 1024;

/// Poll interval while a sweep runs.
const POLL: Duration = Duration::from_millis(500);

/// Crates the sweep examines.
///
/// This cannot be `examine_globs` in `.cargo/mutants.toml`: that option makes
/// cargo-mutants ignore `--file`, which the per-file rotation needs.
const SCOPE: [&str; 5] = [
    "crates/s3s-multipart/",
    "crates/s3s-chunked/",
    "crates/s3s-sigv2/",
    "crates/s3s-sigv4/",
    "crates/s3s-rfc2047/",
];

/// `<file>:<line>:<column>: <description>` of `cargo mutants --list`.
static MUTANT_LINE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^(?P<file>.+?):\d+:\d+: ").expect("constant regex"));

/// Plan and run mutation testing sweeps.
#[derive(Debug, Subcommand)]
pub(crate) enum Mutants {
    /// Print the units a sweep would run, without mutating anything.
    Plan(PlanArgs),
    /// Sweep the units that are due within a wall-clock budget.
    Run(RunArgs),
    /// Render the outcome of a sweep and compare it with the allowlist.
    Report(ReportArgs),
    /// Check that the configuration and the allowlist are usable (no mutation).
    Check(CheckArgs),
}

#[derive(Debug, Parser)]
pub(crate) struct PlanArgs {
    /// Wall-clock budget in minutes for the whole sweep.
    #[arg(long, value_name = "MINUTES", default_value_t = 15)]
    pub(crate) budget_minutes: u64,
    /// Sweep these files instead of the rotation.
    #[arg(long, value_name = "FILES", num_args = 1.., value_delimiter = ' ')]
    files: Vec<String>,
    /// Cursor holding the rotation state.
    #[arg(long, value_name = "PATH", default_value = CURSOR)]
    cursor: PathBuf,
    /// Mutation configuration.
    #[arg(long, value_name = "PATH", default_value = CONFIG)]
    config: PathBuf,
    /// Assumed seconds per mutant.
    #[arg(long, value_name = "SECONDS", default_value_t = SECONDS_PER_MUTANT)]
    seconds_per_mutant: u64,
    /// Assumed fixed cost of one `cargo mutants` call.
    #[arg(long, value_name = "SECONDS", default_value_t = FIXED_SECONDS)]
    fixed_seconds: u64,
}

#[derive(Debug, Parser)]
pub(crate) struct RunArgs {
    #[command(flatten)]
    plan: PlanArgs,
    /// Directory the per-unit outcome directories are written to.
    #[arg(long, value_name = "PATH", default_value = OUT)]
    out: PathBuf,
}

#[derive(Debug, Parser)]
pub(crate) struct ReportArgs {
    /// Outcome directory of one sweep, or a directory holding several.
    #[arg(long, value_name = "PATH", default_value = OUT)]
    out: PathBuf,
    /// Label shown in the rendered report.
    #[arg(long, value_name = "LABEL", default_value = "sweep")]
    label: String,
    /// Fail when a survivor outside the allowlist appears.
    #[arg(long)]
    check: bool,
}

#[derive(Debug, Parser)]
pub(crate) struct CheckArgs {
    /// Mutation configuration.
    #[arg(long, value_name = "PATH", default_value = CONFIG)]
    config: PathBuf,
}

/// One file that has mutants, with how many.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Candidate {
    path: String,
    mutants: usize,
}

/// One unit of work: a file, or one slice of it.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Unit {
    path: String,
    mutants: usize,
}

impl Unit {
    /// Directory name for this unit's outcome, filesystem safe.
    fn slug(&self) -> String {
        self.path.replace(['/', '.'], "_")
    }

    /// The `cargo mutants` arguments that select this unit.
    fn arguments(&self) -> Vec<String> {
        vec!["--file".to_owned(), self.path.clone()]
    }
}

/// Rotation state, carried between runs as an artifact.
#[derive(Debug, Default, Deserialize, Serialize)]
struct Cursor {
    version: u32,
    #[serde(default)]
    files: BTreeMap<String, FileState>,
    #[serde(default)]
    history: Vec<History>,
}

#[derive(Debug, Deserialize, Serialize)]
struct FileState {
    swept_at: u64,
    digest: String,
    mutants: usize,
    /// Mutants that produced an outcome in the last sweep of this file.
    #[serde(default)]
    tested: usize,
    /// Whether that sweep covered every mutant of the file.
    #[serde(default)]
    complete: bool,
}

#[derive(Debug, Deserialize, Serialize)]
struct History {
    swept_at: u64,
    mutants: usize,
    caught: usize,
    missed: usize,
    timeout: usize,
    unviable: usize,
    seconds: u64,
}

/// Survivors accepted as equivalent or as a tracked gap, with the reason.
///
/// The list is code, the way the s3-tests baseline in `report::s3tests` is: it is
/// reviewed in the same diff as the sweep that produced it, it cannot go missing
/// from the repository silently, and the compiler keeps its shape honest. A survivor
/// that is not named here fails `mutants report --check`.
const ACCEPTED: &[(&str, &str, &str)] = &[];

/// The recorded status of an accepted survivor, if it is recorded at all.
fn acceptance(name: &str) -> Option<&'static str> {
    ACCEPTED
        .iter()
        .find(|(recorded, _, _)| *recorded == name)
        .map(|(_, status, _)| *status)
}

/// Counts of one or more sweeps.
#[derive(Debug, Default, PartialEq, Eq)]
struct Totals {
    mutants: usize,
    caught: usize,
    missed: usize,
    timeout: usize,
    unviable: usize,
}

impl Totals {
    /// Mutants that produced an outcome, as opposed to the ones never reached.
    fn tested(&self) -> usize {
        self.caught + self.missed + self.timeout + self.unviable
    }
}

impl Mutants {
    pub(crate) fn run(self) -> Result<bool> {
        let root = repo_root();
        match self {
            Mutants::Plan(args) => {
                let units = plan(&root, &args)?;
                for unit in &units {
                    println!("{}\t{}\t{} mutants", unit.path, unit.slug(), unit.mutants);
                }
                println!("{} unit(s) planned", units.len());
                Ok(true)
            }
            Mutants::Run(args) => sweep(&root, &args),
            Mutants::Report(args) => report(&root, &args),
            Mutants::Check(args) => check(&root, &args),
        }
    }
}

/// The units a sweep would run, in order.
fn plan(root: &Path, args: &PlanArgs) -> Result<Vec<Unit>> {
    let mut candidates = candidates(root, &args.config)?;
    let files: Vec<&String> = args.files.iter().filter(|file| !file.is_empty()).collect();
    if !files.is_empty() {
        candidates.retain(|candidate| files.contains(&&candidate.path));
        ensure!(!candidates.is_empty(), "none of the requested files has mutants");
    }
    let cursor = read_cursor(root, &args.cursor)?;
    let mut entries = Vec::with_capacity(candidates.len());
    for candidate in candidates {
        let state = cursor.files.get(&candidate.path);
        let priority = match state {
            None => 0_u8,
            Some(state) => {
                let changed = digest(&root.join(&candidate.path)).map_or(true, |current| current != state.digest);
                if changed { 1 } else { 2 }
            }
        };
        let swept_at = state.map_or(0, |state| state.swept_at);
        entries.push((priority, swept_at, candidate.path, candidate.mutants));
    }
    entries.sort_by(|left, right| (&left.0, &left.1, &left.2).cmp(&(&right.0, &right.1, &right.2)));
    let entries: Vec<(String, usize)> = entries.into_iter().map(|(_, _, path, mutants)| (path, mutants)).collect();
    Ok(select(&entries, mutant_budget(args)))
}

/// Cap the address space of this process and its children, unless something below
/// the cap is already in place.
#[cfg(unix)]
fn cap_address_space() -> Result<()> {
    let resource = rlimit::Resource::AS;
    let (soft, hard) = rlimit::getrlimit(resource)?;
    let cap = ADDRESS_SPACE_LIMIT.min(hard);
    if soft == rlimit::INFINITY || soft > cap {
        rlimit::setrlimit(resource, cap, hard)?;
        println!("address space capped at {} GiB", cap >> 30);
    }
    Ok(())
}

/// Address-space limits are a Unix idea; elsewhere a runaway mutant is left to the
/// mutation timeout and to cargo-mutants' own parallelism.
#[cfg(not(unix))]
fn cap_address_space() -> Result<()> {
    Ok(())
}

/// Mutants the budget can afford in total.
fn mutant_budget(args: &PlanArgs) -> usize {
    let usable = args.budget_minutes.saturating_mul(60).saturating_sub(args.fixed_seconds);
    let seconds = args.seconds_per_mutant.max(1);
    usize::try_from(usable / seconds).unwrap_or(usize::MAX)
}

/// Pick the files to sweep: priority order, whole files, stopping when the budget
/// would be exceeded. When the most due file does not fit at all, it is started
/// anyway and the deadline turns it into a partial sweep — otherwise a file larger
/// than one run would never be reached.
fn select(entries: &[(String, usize)], budget: usize) -> Vec<Unit> {
    let mut units: Vec<Unit> = Vec::new();
    let mut remaining = budget;
    for (path, mutants) in entries {
        if *mutants <= remaining {
            remaining -= mutants;
            units.push(Unit {
                path: path.clone(),
                mutants: *mutants,
            });
            continue;
        }
        if units.is_empty() {
            // The most due file does not fit the budget: start it anyway, so a file
            // larger than one run still makes progress, and let the deadline stop it.
            // The cursor records how many mutants it got through as a partial sweep.
            units.push(Unit {
                path: path.clone(),
                mutants: *mutants,
            });
            return units;
        }
    }
    units
}

/// Files with mutants, straight from the tool.
fn candidates(root: &Path, config: &Path) -> Result<Vec<Candidate>> {
    let output = Command::new("cargo")
        .arg("mutants")
        .arg("--config")
        .arg(config)
        .arg("--list")
        .current_dir(root)
        .output()
        .context("failed to run cargo mutants --list")?;
    ensure!(
        output.status.success(),
        "cargo mutants --list exited with {}; is {} usable?",
        output.status,
        config.display()
    );
    let listing = String::from_utf8(output.stdout).context("cargo mutants --list wrote invalid UTF-8")?;
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    for line in listing.lines() {
        if let Some(captures) = MUTANT_LINE.captures(line) {
            *counts.entry(captures["file"].to_owned()).or_default() += 1;
        }
    }
    ensure!(
        !counts.is_empty(),
        "cargo mutants --list found no mutant; check the exclude rules in {}",
        config.display()
    );
    counts.retain(|path, _| in_scope(path));
    ensure!(
        !counts.is_empty(),
        "no mutant is inside the scope {SCOPE:?}; extend SCOPE when the sweep should cover more"
    );
    Ok(counts
        .into_iter()
        .map(|(path, mutants)| Candidate { path, mutants })
        .collect())
}

/// Sweep the planned units within the budget and roll the cursor forward.
fn sweep(root: &Path, args: &RunArgs) -> Result<bool> {
    cap_address_space()?;
    let units = plan(root, &args.plan)?;
    if units.is_empty() {
        println!("nothing due: every candidate file was swept and is unchanged");
        return Ok(true);
    }
    let out = root.join(&args.out);
    fs::create_dir_all(&out).with_context(|| format!("failed to create {}", out.display()))?;
    let budget = Duration::from_secs(args.plan.budget_minutes.saturating_mul(60));
    // One `cargo test` per mutant already uses every core; the tool's own advice is
    // to start at two workers, and an unbounded fan-out is what exhausted this
    // machine's memory once already.
    let jobs = std::env::var("CARGO_MUTANTS_JOBS").unwrap_or_else(|_| "2".to_owned());
    let started = Instant::now();
    let mut totals = Totals::default();
    let mut swept: Vec<(&Unit, usize)> = Vec::new();
    for unit in &units {
        if started.elapsed() >= budget && !swept.is_empty() {
            println!("budget reached before {}", unit.slug());
            break;
        }
        println!("sweeping {} ({} mutants) with CARGO_MUTANTS_JOBS={jobs}", unit.slug(), unit.mutants);
        let mut arguments = vec![
            "mutants".to_owned(),
            "--config".to_owned(),
            args.plan.config.display().to_string(),
        ];
        arguments.extend(unit.arguments());
        arguments.extend(
            ["--minimum-test-timeout", "5", "--timeout", "120", "--no-times", "-o"]
                .iter()
                .map(|value| (*value).to_owned()),
        );
        arguments.push(out.join(unit.slug()).display().to_string());
        // Print the exact command: it is the only way to tell what the tool was
        // actually asked to do when a filter or a flag does not behave.
        println!("running: cargo {}", arguments.join(" "));
        let mut command = Command::new("cargo");
        command.env("CARGO_MUTANTS_JOBS", &jobs).args(&arguments).current_dir(root);
        match run_with_deadline(&mut command, started + budget)? {
            // 0 all caught, 2 survivors, 3 timeouts: all three are useful results,
            // and only a broken run (4 baseline red, 1/5/6/70 tool or diff) is an
            // error. See the exit-code table in the cargo-mutants documentation.
            Some(status) if usable(status) => {
                let unit_totals = read_totals(&out.join(unit.slug()))?;
                merge(&mut totals, &unit_totals);
                swept.push((unit, unit_totals.tested()));
            }
            Some(status) => eprintln!("cargo mutants exited with {status} for {}", unit.slug()),
            None => {
                eprintln!("budget reached during {}", unit.slug());
                break;
            }
        }
    }
    let elapsed = started.elapsed().as_secs();
    roll_cursor(root, &args.plan, &swept, &totals, elapsed)?;
    println!(
        "swept {} unit(s): {} mutants, {} caught, {} missed, {} timeout, {} unviable in {}s",
        swept.len(),
        totals.mutants,
        totals.caught,
        totals.missed,
        totals.timeout,
        totals.unviable,
        elapsed
    );
    Ok(true)
}

/// Whether a `cargo mutants` exit code describes a usable run: mutants that
/// survived or timed out are results, not failures.
fn usable(status: ExitStatus) -> bool {
    matches!(status.code(), Some(0 | 2 | 3))
}

/// Run a command, killing it once the deadline passes.
fn run_with_deadline(command: &mut Command, deadline: Instant) -> Result<Option<ExitStatus>> {
    let mut child = command
        .stdin(Stdio::null())
        .spawn()
        .context("failed to start cargo mutants")?;
    loop {
        if let Some(status) = child.try_wait().context("failed to poll cargo mutants")? {
            return Ok(Some(status));
        }
        if Instant::now() >= deadline {
            child.kill().context("failed to stop cargo mutants")?;
            let _ = child.wait();
            return Ok(None);
        }
        sleep(POLL);
    }
}

/// Record the swept units and this run's totals in the cursor.
fn roll_cursor(root: &Path, args: &PlanArgs, swept: &[(&Unit, usize)], totals: &Totals, seconds: u64) -> Result<()> {
    let mut cursor = read_cursor(root, &args.cursor)?;
    let now = unix_seconds()?;
    for (unit, tested) in swept {
        let current = digest(&root.join(&unit.path))?;
        let state = cursor.files.entry(unit.path.clone()).or_insert_with(|| FileState {
            swept_at: now,
            digest: current.clone(),
            mutants: unit.mutants,
            tested: 0,
            complete: false,
        });
        state.swept_at = now;
        state.digest = current;
        state.mutants = unit.mutants;
        state.tested = *tested;
        // A file larger than one budget is started anyway; the cursor remembers how
        // far it got so the next run can tell a partial sweep from a complete one.
        state.complete = *tested >= unit.mutants && unit.mutants > 0;
    }
    cursor.version = 1;
    cursor.history.push(History {
        swept_at: now,
        mutants: totals.mutants,
        caught: totals.caught,
        missed: totals.missed,
        timeout: totals.timeout,
        unviable: totals.unviable,
        seconds,
    });
    let path = root.join(&args.cursor);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("failed to create {}", parent.display()))?;
    }
    fs::write(&path, serde_json::to_vec_pretty(&cursor)?).with_context(|| format!("failed to write {}", path.display()))?;
    Ok(())
}

/// Render the outcome of one or more sweeps and compare the survivors with the allowlist.
fn report(root: &Path, args: &ReportArgs) -> Result<bool> {
    let directories = collect_outcomes(root, &args.out);
    ensure!(!directories.is_empty(), "no outcomes.json under {}", args.out.display());
    let mut totals = Totals::default();
    let mut survivors: BTreeSet<String> = BTreeSet::new();
    for directory in &directories {
        merge(&mut totals, &read_totals(directory)?);
        survivors.extend(read_survivors(directory)?);
    }
    let known: Vec<&String> = survivors.iter().filter(|name| acceptance(name).is_some()).collect();
    let new: Vec<&String> = survivors.iter().filter(|name| acceptance(name).is_none()).collect();
    println!("### mutation {} ", args.label);
    println!();
    println!(
        "{} mutant(s): {} caught, {} missed, {} timeout, {} unviable",
        totals.mutants, totals.caught, totals.missed, totals.timeout, totals.unviable
    );
    if !survivors.is_empty() {
        println!();
        println!("Survivors:");
        for name in &survivors {
            let mark = if acceptance(name).is_some() { "accepted" } else { "NEW" };
            println!("- [{mark}] {name}");
        }
    }
    println!();
    println!(
        "{} survivor(s): {} accepted by the allowlist, {} outside it",
        survivors.len(),
        known.len(),
        new.len()
    );
    if args.check && !new.is_empty() {
        println!();
        println!("Failing: {} survivor(s) are not recorded in ACCEPTED", new.len());
        return Ok(false);
    }
    Ok(true)
}

/// Cheap gate: the configuration yields mutants and the accepted list is well formed.
fn check(root: &Path, args: &CheckArgs) -> Result<bool> {
    let candidates = candidates(root, &args.config)?;
    let mutants: usize = candidates.iter().map(|candidate| candidate.mutants).sum();
    for (name, status, reason) in ACCEPTED {
        ensure!(
            !status.is_empty() && !reason.is_empty(),
            "accepted entry {name:?} needs a status and a reason"
        );
    }
    println!("{}: {} file(s), {mutants} mutant(s)", args.config.display(), candidates.len());
    println!("accepted survivors: {}", ACCEPTED.len());
    Ok(true)
}

/// Totals and survivors of one outcome directory.
fn read_totals(directory: &Path) -> Result<Totals> {
    let outcomes = read_outcomes(directory)?;
    Ok(Totals {
        mutants: outcomes["total_mutants"].as_u64().unwrap_or(0).try_into()?,
        caught: outcomes["caught"].as_u64().unwrap_or(0).try_into()?,
        missed: outcomes["missed"].as_u64().unwrap_or(0).try_into()?,
        timeout: outcomes["timeout"].as_u64().unwrap_or(0).try_into()?,
        unviable: outcomes["unviable"].as_u64().unwrap_or(0).try_into()?,
    })
}

/// Names of the mutants that survived, as cargo-mutants reports them.
fn read_survivors(directory: &Path) -> Result<Vec<String>> {
    let outcomes = read_outcomes(directory)?;
    let mut survivors = Vec::new();
    for outcome in outcomes["outcomes"].as_array().into_iter().flatten() {
        if outcome["summary"] != "MissedMutant" {
            continue;
        }
        if let Some(name) = outcome["scenario"]["Mutant"]["name"].as_str() {
            survivors.push(name.to_owned());
        }
    }
    Ok(survivors)
}

/// Parse one `outcomes.json`.
fn read_outcomes(directory: &Path) -> Result<serde_json::Value> {
    let path = directory.join("mutants.out/outcomes.json");
    let text = fs::read_to_string(&path).with_context(|| format!("failed to read {}", path.display()))?;
    serde_json::from_str(&text).with_context(|| format!("failed to parse {}", path.display()))
}

/// Every `outcomes.json` under a directory, one per unit.
fn collect_outcomes(root: &Path, out: &Path) -> Vec<PathBuf> {
    let base = root.join(out);
    let mut directories = Vec::new();
    if base.join("mutants.out/outcomes.json").is_file() {
        directories.push(base.clone());
    }
    for entry in fs::read_dir(&base).into_iter().flatten().flatten() {
        let path = entry.path();
        if path.join("mutants.out/outcomes.json").is_file() {
            directories.push(path);
        }
    }
    directories.sort();
    directories
}

/// Add one sweep's counts into the running totals.
fn merge(totals: &mut Totals, other: &Totals) {
    totals.mutants += other.mutants;
    totals.caught += other.caught;
    totals.missed += other.missed;
    totals.timeout += other.timeout;
    totals.unviable += other.unviable;
}

/// Read the cursor, treating a missing or unreadable file as an empty rotation.
fn read_cursor(root: &Path, cursor: &Path) -> Result<Cursor> {
    let path = root.join(cursor);
    match fs::read_to_string(&path) {
        Ok(text) => serde_json::from_str(&text).with_context(|| format!("failed to parse {}", path.display())),
        Err(_) => Ok(Cursor::default()),
    }
}

/// Whether a path belongs to one of the crates the sweep examines.
fn in_scope(path: &str) -> bool {
    SCOPE.iter().any(|prefix| path.starts_with(*prefix))
}

/// Change detector for a file: FNV-1a over its bytes, cheap and dependency free.
fn digest(path: &Path) -> Result<String> {
    let bytes = fs::read(path).with_context(|| format!("failed to read {}", path.display()))?;
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in bytes {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    Ok(format!("fnv1a64:{hash:016x}"))
}

/// Seconds since the Unix epoch.
fn unix_seconds() -> Result<u64> {
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("the system clock is before the Unix epoch")?
        .as_secs())
}

#[cfg(test)]
mod tests {
    use super::*;

    const LISTING: &str = "\
crates/s3s/src/lib.rs:12:5: replace foo with bar
crates/s3s/src/lib.rs:20:9: replace baz with qux
crates/s3s/src/http/de.rs:3:1: replace a with b
";

    #[test]
    fn counts_mutants_per_file() {
        let mut counts: BTreeMap<String, usize> = BTreeMap::new();
        for line in LISTING.lines() {
            if let Some(captures) = MUTANT_LINE.captures(line) {
                *counts.entry(captures["file"].to_owned()).or_default() += 1;
            }
        }
        assert_eq!(counts["crates/s3s/src/lib.rs"], 2);
        assert_eq!(counts["crates/s3s/src/http/de.rs"], 1);
    }

    #[test]
    fn nothing_is_accepted_without_a_recorded_reason() {
        assert!(ACCEPTED.is_empty(), "record an accepted survivor here only with a reason");
        assert_eq!(acceptance("crates/s3s/src/lib.rs:1:1: replace x with y"), None);
    }

    #[test]
    fn scope_selects_whole_crates_only() {
        assert!(in_scope("crates/s3s-sigv2/src/lib.rs"));
        assert!(in_scope("crates/s3s-rfc2047/src/lib.rs"));
        assert!(!in_scope("crates/s3s/src/lib.rs"));
        assert!(!in_scope("xtask/src/main.rs"));
    }

    #[test]
    fn plans_the_whole_file_when_it_fits() {
        let entries = vec![("a.rs".to_owned(), 4)];
        let units = select(&entries, 100);
        assert_eq!(units.len(), 1);
        assert_eq!(units[0].path, "a.rs");
        assert_eq!(units[0].mutants, 4);
    }

    #[test]
    fn stops_before_the_budget_is_exceeded() {
        let entries = vec![("a.rs".to_owned(), 8), ("b.rs".to_owned(), 8), ("c.rs".to_owned(), 8)];
        let units = select(&entries, 10);
        assert_eq!(units.len(), 1);
        assert_eq!(units[0].path, "a.rs");
    }

    #[test]
    fn starts_the_most_due_file_when_it_does_not_fit() {
        let entries = vec![("big.rs".to_owned(), 600), ("small.rs".to_owned(), 20)];
        let units = select(&entries, 10);
        assert_eq!(units.len(), 1);
        assert_eq!(units[0].path, "big.rs", "the most due file is started even beyond the budget");
        assert_eq!(units[0].mutants, 600);
    }
    #[test]
    fn unit_slug_is_a_safe_directory_name() {
        let unit = Unit {
            path: "crates/s3s/src/lib.rs".to_owned(),
            mutants: 5,
        };
        assert_eq!(unit.slug(), "crates_s3s_src_lib_rs");
        assert_eq!(unit.arguments(), vec!["--file", "crates/s3s/src/lib.rs"]);
    }

    #[test]
    fn cursor_round_trips() {
        let mut cursor = Cursor {
            version: 1,
            ..Cursor::default()
        };
        cursor.files.insert(
            "a.rs".to_owned(),
            FileState {
                swept_at: 1,
                digest: "fnv1a64:00".to_owned(),
                mutants: 3,
                tested: 2,
                complete: false,
            },
        );
        let text = serde_json::to_string(&cursor).expect("serialises");
        let back: Cursor = serde_json::from_str(&text).expect("parses");
        assert_eq!(back.files["a.rs"].tested, 2);
        assert!(!back.files["a.rs"].complete);
    }

    #[test]
    fn budget_keeps_room_for_the_fixed_cost() {
        let args = PlanArgs {
            budget_minutes: 15,
            files: Vec::new(),
            cursor: PathBuf::from(CURSOR),
            config: PathBuf::from(CONFIG),
            seconds_per_mutant: 25,
            fixed_seconds: 60,
        };
        assert_eq!(mutant_budget(&args), (900 - 60) / 25);
    }
}
