// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

//! The differential harness of the migration.
//!
//! Three sides have to agree on every case of the frozen fixture: the pre-migration
//! implementation that the s3s crate ships today (the oracle), the expectations that
//! the generator recorded from the oracle (the fixture), and this crate's own
//! implementation (the candidate). The oracle replay runs everywhere; the candidate
//! run needs the implementation to be compiled in, which the harness reports instead
//! of passing silently.
//!
//! A difference is not a failure by itself: it has to be explained, and the harness
//! keeps the explained ones in a table so that a difference cannot be forgotten and a
//! stale entry cannot survive.

mod harness;

use std::time::{Duration, SystemTime};

use harness::api;
use harness::support::{self, Direction, Expected, Format};

/// The fixture cases, or a panic that says what is wrong with the file.
fn fixture_cases() -> Vec<support::Case> {
    support::read_fixture().unwrap_or_else(|error| panic!("the frozen fixture is not usable: {error}"))
}

#[test]
fn the_fixture_covers_the_documented_corpus() {
    let cases = fixture_cases();
    assert!(!cases.is_empty(), "the frozen fixture must not be empty");

    for direction in [Direction::Parse, Direction::Format] {
        for format in [Format::DateTime, Format::HttpDate, Format::EpochSeconds] {
            let present = cases.iter().any(|case| case.direction == direction && case.format == format);
            assert!(present, "the fixture has no {} case for {}", direction.name(), format.name());
        }
    }

    let errors = cases
        .iter()
        .filter(|case| matches!(case.expected, Expected::Error(_)))
        .count();
    assert!(errors > 0, "the fixture must pin the error classification");

    let vectors = cases.iter().filter(|case| case.id().starts_with("smithy/")).count();
    let boundary = cases.iter().filter(|case| case.id().starts_with("design-")).count();
    assert_eq!(vectors + boundary, cases.len(), "every case must belong to a corpus section");

    println!(
        "fixture: {} cases ({vectors} external vectors, {boundary} boundary inputs), {errors} errors",
        cases.len()
    );
}

#[test]
fn the_oracle_replays_the_frozen_fixture() {
    let cases = fixture_cases();
    let tick = support::platform_tick_nanos();
    let (coverage, failures) = api::oracle_replay(&cases, tick);
    assert!(coverage.checked > 0, "the oracle must replay the cases the corpus pins");
    assert_eq!(
        coverage.checked + coverage.skipped,
        cases.len(),
        "every case is either replayed or left out"
    );
    assert_eq!(
        coverage.checked,
        cases.iter().filter(|case| api::case_is_carried(case, tick)).count(),
        "the oracle must replay every case this platform clock can carry"
    );
    assert!(
        failures.is_empty(),
        "{} of {} cases diverge between the oracle and the frozen fixture:\n{}",
        failures.len(),
        cases.len(),
        failures.join("\n")
    );
    if coverage.skipped > 0 {
        println!(
            "the oracle left out {} of {} cases on this platform: {}",
            coverage.skipped,
            cases.len(),
            api::BRIDGE_REASON
        );
    }
    println!(
        "the oracle replays {} of {} cases at a tick of {tick} ns ({} skipped)",
        coverage.checked,
        cases.len(),
        coverage.skipped
    );
}

#[test]
fn the_candidate_agrees_with_the_oracle_and_the_fixture() {
    let cases = fixture_cases();
    if !api::CANDIDATE_AVAILABLE {
        println!("the candidate side is not compiled in: the differential run is pending the implementation");
        return;
    }

    let tick = support::platform_tick_nanos();
    let report = api::candidate_differential(&cases, tick);
    assert_eq!(
        report.candidate_checked,
        cases.len(),
        "the candidate never crosses the bridge and must run every case"
    );
    assert_eq!(
        report.oracle_checked + report.oracle_skipped,
        cases.len(),
        "every case is either checked or skipped on the oracle side"
    );
    assert_eq!(
        report.oracle_checked,
        cases.iter().filter(|case| api::case_is_carried(case, tick)).count(),
        "the oracle side must run every case this platform clock can carry"
    );
    for line in &report.recorded {
        println!("recorded divergence: {line}");
    }
    assert!(
        report.problems.is_empty(),
        "{} differences need an explanation or a fix:\n{}",
        report.problems.len(),
        report.problems.join("\n")
    );
    if report.oracle_skipped > 0 {
        println!(
            "the oracle side left out {} of {} cases on this platform: {}",
            report.oracle_skipped,
            cases.len(),
            api::BRIDGE_REASON
        );
    }
    println!(
        "the candidate matches the oracle and the frozen fixture in all {} cases (oracle checked {}, skipped {})",
        cases.len(),
        report.oracle_checked,
        report.oracle_skipped
    );
}

#[test]
fn the_oracle_panics_on_system_times_outside_its_range() {
    // The format cases of the fixture stay inside the range that both implementations
    // hold, because the pre-migration conversion adds a duration to the epoch and
    // panics when the result leaves its representable range. Pinning the behaviour here
    // means that a change of the oracle is noticed before the corpus is regenerated.
    let after_the_last_year = SystemTime::UNIX_EPOCH + Duration::from_secs(253_402_300_800);
    let outcome = std::panic::catch_unwind(|| {
        let _ = s3s::dto::Timestamp::from(after_the_last_year);
    });
    assert!(outcome.is_err(), "the oracle is expected to panic past its representable range");
}
