// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

//! Checks the committed seed corpus of the `timestamp_parsers` fuzz target.
//!
//! A seed is only worth committing while it still exercises what it was added
//! for, and a corpus nobody re-reads rots silently when the parser changes. So
//! every seed has an entry in [`SEEDS`] stating the selector byte it carries and
//! the outcome it must produce; this binary fails when
//!
//! - a seed in the directory has no entry (an undocumented input),
//! - an entry has no seed (a dangling expectation),
//! - a seed's outcome differs from its expectation.
//!
//! Each file is `[selector_byte][payload]`; `selector_byte % 3` picks the
//! format and the payload is the input text. The check calls the same
//! `Timestamp::parse` and `Timestamp::format` the fuzzer calls, and it asserts
//! the same roundtrip the target's oracle C asserts, for the seeds the format
//! represents exactly.
//!
//! Usage (from the repository root):
//! ```text
//! cargo run --manifest-path fuzz/Cargo.toml --bin check_timestamp_seeds -- fuzz/seeds/timestamp_parsers
//! ```

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use s3s_time::FormatTimestampError;
use s3s_time::ParseTimestampError;
use s3s_time::Timestamp;
use s3s_time::TimestampFormat;

const NANOS_PER_SEC: i128 = 1_000_000_000;
const NANOS_PER_MILLI: i128 = 1_000_000;

/// What a seed must produce.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Expect {
    /// The parse succeeds; when the format represents the value exactly, the
    /// roundtrip `parse(format(x)) == x` must hold as well.
    Parses,
    /// The parse fails with this error.
    Fails(ParseTimestampError),
}

/// Every committed seed, sorted by name, with the outcome it is there for.
const SEEDS: &[(&str, u8, Expect)] = &[
    // date-time: the accepted forms and the field boundaries. A lowercase `t`
    // and `z` are accepted; fields outside their range report `OutOfRange`
    // rather than `InvalidFormat` (a recorded difference from the pre-migration
    // by-name classification, see `tests/harness/api.rs`).
    ("00_datetime_utc", 0x00, Expect::Parses),
    ("01_datetime_offset", 0x00, Expect::Parses),
    ("02_datetime_lowercase", 0x00, Expect::Parses),
    ("03_datetime_leap_day", 0x00, Expect::Parses),
    // The internal representation reserves the largest UTC offset at both ends
    // of the four-digit year range, so the last day of year 9999 is outside it
    // (recorded as `design-4.3/year-9999-end`).
    ("04_datetime_year_max", 0x00, Expect::Fails(ParseTimestampError::OutOfRange)),
    ("05_datetime_malformed", 0x00, Expect::Fails(ParseTimestampError::OutOfRange)),
    // http-date: the weekday is not cross-checked, the fraction is rejected.
    ("06_httpdate_valid", 0x01, Expect::Parses),
    ("07_httpdate_weekday_mismatch", 0x01, Expect::Parses),
    ("08_httpdate_fraction", 0x01, Expect::Fails(ParseTimestampError::InvalidFormat)),
    ("09_httpdate_lowercase_gmt", 0x01, Expect::Fails(ParseTimestampError::InvalidFormat)),
    ("10_httpdate_epoch_day", 0x01, Expect::Parses),
    // epoch-seconds: the shortest exact decimal, both signs and the fraction limit.
    ("11_epoch_integer", 0x02, Expect::Parses),
    ("12_epoch_negative_fraction", 0x02, Expect::Parses),
    ("13_epoch_shortest_negative", 0x02, Expect::Parses),
    ("14_epoch_fraction_too_long", 0x02, Expect::Fails(ParseTimestampError::FractionTooLong)),
    ("15_epoch_trailing_zeros", 0x02, Expect::Parses),
];

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let dump = args.iter().any(|arg| arg == "--dump");
    let Some(dir) = args.iter().find(|arg| !arg.starts_with("--")) else {
        eprintln!("usage: check_timestamp_seeds [--dump] <seed-dir>");
        return ExitCode::FAILURE;
    };
    let dir = Path::new(dir);

    let mut seeds = match read_seeds(dir) {
        Ok(seeds) => seeds,
        Err(err) => {
            eprintln!("cannot read {}: {err}", dir.display());
            return ExitCode::FAILURE;
        }
    };

    if dump {
        return dump_seeds(&seeds);
    }

    let mut failures = 0usize;
    for (name, selector, expect) in SEEDS {
        let Some(path) = seeds.remove(*name) else {
            eprintln!("FAIL {name}: no seed file in {}", dir.display());
            failures += 1;
            continue;
        };
        match check(&path, *selector, *expect) {
            Ok(outcome) => println!("ok   {name:<30} {outcome}"),
            Err(err) => {
                eprintln!("FAIL {name}: {err}");
                failures += 1;
            }
        }
    }
    for name in seeds.keys() {
        eprintln!("FAIL {name}: seed without an entry in SEEDS");
        failures += 1;
    }

    if failures == 0 {
        println!("checked {} seeds, all match", SEEDS.len());
        ExitCode::SUCCESS
    } else {
        eprintln!("{failures} of {} seeds failed", SEEDS.len());
        ExitCode::FAILURE
    }
}

/// Prints what every seed does, for authoring and for refreshing [`SEEDS`].
///
/// This is an authoring aid, not a check: it never fails on a mismatch, so a
/// refresh has to be read as a diff rather than accepted because it is green.
fn dump_seeds(seeds: &BTreeMap<String, PathBuf>) -> ExitCode {
    for (name, path) in seeds {
        let Ok(bytes) = fs::read(path) else {
            println!("{name}: unreadable");
            continue;
        };
        let Some((&selector, payload)) = bytes.split_first() else {
            println!("{name}: empty");
            continue;
        };
        let format = format_of(selector);
        let text = String::from_utf8_lossy(payload);
        match Timestamp::parse(format, &text) {
            Ok(value) => {
                let mut buf = Vec::new();
                let written = match value.format(format, &mut buf) {
                    Ok(()) => String::from_utf8_lossy(&buf).into_owned(),
                    Err(err) => format!("<{err}>"),
                };
                println!(
                    "{name:<30} {:#04x} {} parses nanos={} writes {written:?}",
                    selector,
                    name_of(selector),
                    value.unix_nanos()
                );
            }
            Err(err) => println!("{name:<30} {selector:#04x} {} fails {err}", name_of(selector)),
        }
    }
    ExitCode::SUCCESS
}

/// Reads `<dir>/*.bin`, keyed by file stem.
fn read_seeds(dir: &Path) -> std::io::Result<BTreeMap<String, PathBuf>> {
    let mut seeds = BTreeMap::new();
    for entry in fs::read_dir(dir)? {
        let path = entry?.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("bin") {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) else {
            continue;
        };
        seeds.insert(stem.to_owned(), path);
    }
    Ok(seeds)
}

/// Maps the selector byte to the format the target uses.
fn format_of(selector: u8) -> TimestampFormat {
    match selector % 3 {
        0 => TimestampFormat::DateTime,
        1 => TimestampFormat::HttpDate,
        _ => TimestampFormat::EpochSeconds,
    }
}

/// Whether the selected format stores `nanos` without losing information.
fn is_lossless(selector: u8, nanos: i128) -> bool {
    match selector % 3 {
        0 => nanos % NANOS_PER_MILLI == 0,
        1 => nanos % NANOS_PER_SEC == 0,
        _ => true,
    }
}

fn name_of(selector: u8) -> &'static str {
    match selector % 3 {
        0 => "date-time",
        1 => "http-date",
        _ => "epoch-seconds",
    }
}

fn check(path: &Path, selector: u8, expect: Expect) -> Result<String, String> {
    let bytes = fs::read(path).map_err(|err| format!("cannot read: {err}"))?;
    let Some((&actual, payload)) = bytes.split_first() else {
        return Err("empty seed: no selector byte".to_owned());
    };
    if actual != selector {
        return Err(format!(
            "selector byte is {actual:#04x}, expected {selector:#04x} ({} vs {})",
            name_of(actual),
            name_of(selector)
        ));
    }
    let text = std::str::from_utf8(payload).map_err(|err| format!("payload is not UTF-8: {err}"))?;
    let format = format_of(selector);

    match (Timestamp::parse(format, text), expect) {
        (Ok(value), Expect::Parses) => roundtrip(format, value, selector, text),
        (Ok(_), Expect::Fails(want)) => Err(format!("parsed, expected {want}")),
        (Err(err), Expect::Fails(want)) => {
            if err == want {
                Ok(format!("fails with {err}"))
            } else {
                Err(format!("fails with {err}, expected {want}"))
            }
        }
        (Err(err), Expect::Parses) => Err(format!("did not parse: {err}")),
    }
}

/// The target's roundtrip oracle, for the seeds the format represents exactly.
fn roundtrip(format: TimestampFormat, value: Timestamp, selector: u8, text: &str) -> Result<String, String> {
    let mut buf = Vec::new();
    match value.format(format, &mut buf) {
        Ok(()) => {}
        // The parser accepts the full instant range while the wire forms carry
        // four-digit years, so a parsed value can be unwritable.
        Err(FormatTimestampError::OutOfRange) => return Ok(format!("parses {text:?}; unwritable year")),
        Err(err) => return Err(format!("formatting a parsed value failed: {err}")),
    }
    let written = std::str::from_utf8(&buf).map_err(|err| format!("formatted output is not ASCII: {err}"))?;

    if !is_lossless(selector, value.unix_nanos()) {
        return Ok(format!(
            "parses {text:?}; {} drops sub-second digits, writes {written:?}",
            name_of(selector)
        ));
    }

    let reparsed = Timestamp::parse(format, written).map_err(|err| format!("re-parsing {written:?} failed: {err}"))?;
    if reparsed != value {
        return Err(format!("roundtrip differs for {text:?}: wrote {written:?}"));
    }
    Ok(format!("parses {text:?}; roundtrip {written:?}"))
}
