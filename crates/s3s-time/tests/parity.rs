// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

//! The frozen contract of the migration.
//!
//! The fixture records what the pre-migration implementation produced, case by case,
//! before it was deleted; the generator and the oracle adapter went with it, so the
//! fixture is the only oracle now. Every case replays against this crate and must
//! match byte for byte, except the ids listed in `POST_MIGRATION_DIVERGENCES`, which
//! are the differences the migration decided to make. A listed id that stops
//! diverging is a failure, so the list cannot outlive the behaviour it describes.

mod harness;

use harness::api;
use harness::api::POST_MIGRATION_DIVERGENCES;
use harness::support;
use harness::support::Direction;
use harness::support::Expected;
use harness::support::Format;

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
fn the_implementation_replays_the_frozen_fixture() {
    let cases = fixture_cases();
    if !api::CANDIDATE_AVAILABLE {
        println!("the candidate side is not compiled in: the replay is pending the implementation");
        return;
    }

    let report = api::candidate_replay(&cases);
    assert_eq!(
        report.checked,
        cases.len(),
        "the candidate never crosses a platform clock and must replay every case"
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
    assert_eq!(
        report.recorded.len(),
        POST_MIGRATION_DIVERGENCES.len(),
        "every recorded divergence must be observed exactly once"
    );
    println!(
        "the implementation matches the frozen fixture in {} of {} cases; {} recorded divergences",
        cases.len() - report.recorded.len(),
        cases.len(),
        report.recorded.len()
    );
}
