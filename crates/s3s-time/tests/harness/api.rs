// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

//! The behaviour under test, behind one trait.
//!
//! The oracle is the pre-migration implementation that the s3s crate ships today: the
//! fixture is generated from it and replayed against it. The candidate is this crate's
//! own implementation, compiled in as the evaluation replaces the oracle dependency.
//! The whole adapter, the oracle included, is development-only and is removed with the
//! dependency once the migration is complete.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fmt;
use std::hash::Hash;

use crate::harness::support::{
    Case, Direction, Expected, Format, carries_faithfully, decode_epoch_literal, decode_instant, platform_tick_nanos,
    render_expected, system_time,
};

/// One wire-format operation, as the frozen fixture describes it.
pub trait TimeApi: Sized + Clone + Eq + Ord + Hash + fmt::Debug {
    /// The error type of the implementation.
    type Error: fmt::Debug;

    /// Builds an instant from canonical seconds and a non-negative adjustment.
    fn from_instant(secs: i64, nanos: u32) -> Self;

    /// Parses the text of the given format.
    fn parse(format: Format, text: &str) -> Result<Self, Self::Error>;

    /// Writes the instant in the given format.
    fn format(&self, format: Format) -> Result<String, Self::Error>;

    /// The canonical seconds and nanoseconds of the instant.
    fn canonical(&self) -> (i64, u32);

    /// The contract name of an error, as the fixture records it.
    fn error_name(error: &Self::Error) -> &'static str;
}

/// The result of one case, in the vocabulary of the fixture.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// The case produced an instant.
    Instant { secs: i64, nanos: u32 },
    /// The case produced this exact text.
    Text(String),
    /// The case failed with this contract error name.
    Error(&'static str),
}

impl Outcome {
    /// The expected value that this outcome satisfies.
    pub fn expected(&self) -> Expected {
        match self {
            Self::Instant { secs, nanos } => Expected::Instant {
                secs: *secs,
                nanos: *nanos,
            },
            Self::Text(text) => Expected::Text(text.clone()),
            Self::Error(name) => Expected::Error((*name).to_owned()),
        }
    }

    /// Whether the outcome satisfies a recorded expectation.
    pub fn matches(&self, expected: &Expected) -> bool {
        self.expected() == *expected
    }

    /// Renders the outcome as the expected column of the fixture.
    pub fn render(&self) -> String {
        render_expected(&self.expected())
    }
}

/// Runs one case against an implementation.
///
/// # Panics
///
/// Panics when a format case does not carry a canonical instant, which the fixture
/// parser rejects before a test runs.
pub fn run<T: TimeApi>(direction: Direction, format: Format, input: &str) -> Outcome {
    match direction {
        Direction::Parse => match T::parse(format, input) {
            Ok(timestamp) => {
                let (secs, nanos) = timestamp.canonical();
                Outcome::Instant { secs, nanos }
            }
            Err(error) => Outcome::Error(T::error_name(&error)),
        },
        Direction::Format => {
            let (secs, nanos) = decode_instant(input).expect("a format case carries a canonical instant");
            match T::from_instant(secs, nanos).format(format) {
                Ok(text) => Outcome::Text(text),
                Err(error) => Outcome::Error(T::error_name(&error)),
            }
        }
    }
}

/// Runs one fixture case against an implementation.
pub fn run_case<T: TimeApi>(case: &Case) -> Outcome {
    run::<T>(case.direction, case.format, &case.input)
}

/// The pre-migration implementation, taken from the data transfer objects of s3s.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Oracle(s3s::dto::Timestamp);

impl TimeApi for Oracle {
    type Error = OracleError;

    fn from_instant(secs: i64, nanos: u32) -> Self {
        Self(s3s::dto::Timestamp::from(system_time(secs, nanos)))
    }

    fn parse(format: Format, text: &str) -> Result<Self, Self::Error> {
        s3s::dto::Timestamp::parse(oracle_format(format), text)
            .map(Self)
            .map_err(OracleError::Parse)
    }

    fn format(&self, format: Format) -> Result<String, Self::Error> {
        let mut buf = Vec::new();
        self.0.format(oracle_format(format), &mut buf).map_err(OracleError::Format)?;
        Ok(String::from_utf8(buf).expect("the wire formats are ASCII"))
    }

    fn canonical(&self) -> (i64, u32) {
        // The oracle exposes no accessor, so the instant is read back from its exact
        // epoch-seconds rendering, which is the shortest decimal of the same instant.
        let text = self.format(Format::EpochSeconds).expect("epoch-seconds always formats");
        decode_epoch_literal(&text).expect("an epoch-seconds rendering decodes")
    }

    fn error_name(error: &Self::Error) -> &'static str {
        error.contract_name()
    }
}

/// Maps a fixture format onto the oracle format.
fn oracle_format(format: Format) -> s3s::dto::TimestampFormat {
    match format {
        Format::DateTime => s3s::dto::TimestampFormat::DateTime,
        Format::HttpDate => s3s::dto::TimestampFormat::HttpDate,
        Format::EpochSeconds => s3s::dto::TimestampFormat::EpochSeconds,
    }
}

/// The error of the oracle, keeping the contract classification.
#[derive(Debug)]
pub enum OracleError {
    /// The parser rejected the input.
    Parse(s3s::dto::ParseTimestampError),
    /// The writer rejected the instant.
    Format(s3s::dto::FormatTimestampError),
}

impl OracleError {
    /// Maps the pre-migration variants onto the contract names of the fixture.
    ///
    /// The pre-migration parser reports a grammar mismatch and an unparsable integer
    /// with two variants, both of which mean that the input does not match the
    /// grammar; its only overflow is an epoch-seconds fraction that is too long, and
    /// the component range carries every out-of-range field and instant.
    fn contract_name(&self) -> &'static str {
        match self {
            Self::Parse(error) => match error {
                s3s::dto::ParseTimestampError::Time(_) | s3s::dto::ParseTimestampError::Int(_) => "InvalidFormat",
                s3s::dto::ParseTimestampError::Overflow => "FractionTooLong",
                s3s::dto::ParseTimestampError::ComponentRange(_) => "OutOfRange",
            },
            Self::Format(error) => match error {
                s3s::dto::FormatTimestampError::Time(_) => "OutOfRange",
                s3s::dto::FormatTimestampError::Io(_) => "Io",
            },
        }
    }
}

/// Every instant that the fixture pins down, without duplicates and in order.
pub fn fixture_instants(cases: &[Case]) -> Vec<(i64, u32)> {
    let mut instants = BTreeSet::new();
    for case in cases {
        match (&case.expected, case.direction) {
            (Expected::Instant { secs, nanos }, _) => {
                instants.insert((*secs, *nanos));
            }
            (_, Direction::Format) => {
                instants.insert(case.instant());
            }
            _ => {}
        }
    }
    instants.into_iter().collect()
}

/// The three formats, in a fixed order.
fn formats() -> [Format; 3] {
    [Format::DateTime, Format::HttpDate, Format::EpochSeconds]
}

/// The property failures of an implementation over the instants of the fixture.
///
/// The properties are the ones the crate promises: the canonical view round trips,
/// the exact formats round trip, formatting is idempotent, and the ordering and the
/// hash agree with the instant order.
pub fn property_failures<T: TimeApi>(cases: &[Case]) -> Vec<String> {
    let instants = fixture_instants(cases);
    let mut failures = Vec::new();
    check_canonical::<T>(&instants, &mut failures);
    check_round_trip::<T>(&instants, &mut failures);
    check_idempotence::<T>(&instants, &mut failures);
    check_order::<T>(&instants, &mut failures);
    failures
}

/// The canonical view must survive construction.
fn check_canonical<T: TimeApi>(instants: &[(i64, u32)], failures: &mut Vec<String>) {
    for &(secs, nanos) in instants {
        let read_back = T::from_instant(secs, nanos).canonical();
        if read_back != (secs, nanos) {
            failures.push(format!("from_instant({secs}, {nanos}) reads back as {read_back:?}"));
        }
    }
}

/// Parsing the output of a format must return the instant it can carry exactly.
fn check_round_trip<T: TimeApi>(instants: &[(i64, u32)], failures: &mut Vec<String>) {
    for &(secs, nanos) in instants {
        let timestamp = T::from_instant(secs, nanos);
        for format in formats() {
            let Ok(text) = timestamp.format(format) else {
                continue;
            };
            let exact = match format {
                Format::DateTime => nanos % 1_000_000 == 0,
                Format::HttpDate => nanos == 0,
                Format::EpochSeconds => true,
            };
            match T::parse(format, &text) {
                Ok(back) => {
                    if exact && back.canonical() != (secs, nanos) {
                        failures.push(format!(
                            "{} does not round trip: {secs}:{nanos} then {text:?} then {:?}",
                            format.name(),
                            back.canonical()
                        ));
                    }
                }
                Err(error) => {
                    failures.push(format!("{} cannot parse its own output {text:?}: {error:?}", format.name()));
                }
            }
        }
    }
}

/// Writing the parse of a rendering must reproduce the rendering.
fn check_idempotence<T: TimeApi>(instants: &[(i64, u32)], failures: &mut Vec<String>) {
    for &(secs, nanos) in instants {
        let timestamp = T::from_instant(secs, nanos);
        for format in formats() {
            let Ok(text) = timestamp.format(format) else {
                continue;
            };
            let Ok(back) = T::parse(format, &text) else {
                continue;
            };
            match back.format(format) {
                Ok(again) if again == text => {}
                Ok(again) => failures.push(format!("{} is not idempotent: {text:?} then {again:?}", format.name())),
                Err(error) => failures.push(format!("{} cannot rewrite {text:?}: {error:?}", format.name())),
            }
        }
    }
}

/// Ordering and hashing must agree with the instant order.
fn check_order<T: TimeApi>(instants: &[(i64, u32)], failures: &mut Vec<String>) {
    let mut values = BTreeMap::new();
    for &(secs, nanos) in instants {
        values.insert((secs, nanos), T::from_instant(secs, nanos));
    }
    let keys: Vec<(i64, u32)> = values.keys().copied().collect();
    for (index, key) in keys.iter().enumerate() {
        let left = &values[key];
        for other in keys.iter().skip(index + 1) {
            if left >= &values[other] {
                failures.push(format!("ordering disagrees with the instants: {key:?} is not before {other:?}"));
            }
        }
    }
    let unique: HashSet<&T> = values.values().collect();
    if unique.len() != values.len() {
        failures.push(format!(
            "hashing disagrees with equality: {} distinct values for {} instants",
            unique.len(),
            values.len()
        ));
    }
}

/// The result of the differential run of the candidate.
#[derive(Debug, Default)]
pub struct Differential {
    /// Differences that are neither absent nor recorded: fix the candidate or record
    /// the difference in the migration report and in the table below.
    pub problems: Vec<String>,
    /// The recorded divergences that were observed, with the reason of each.
    pub recorded: Vec<String>,
    /// The cases the oracle side replayed.
    pub oracle_checked: usize,
    /// The cases the oracle side left out because the platform clock cannot carry them.
    pub oracle_skipped: usize,
    /// The cases the candidate side replayed. The candidate never crosses the bridge, so
    /// it runs every case on every platform.
    pub candidate_checked: usize,
}

/// Whether the candidate side is compiled in.
pub const CANDIDATE_AVAILABLE: bool = true;

/// Cases where the candidate diverges on purpose, with the recorded reason.
///
/// An entry is only for a difference that the crate design accepts; an entry that
/// stops diverging is reported as a problem, so the table cannot go stale.
const KNOWN_DIVERGENCES: &[(&str, &str)] = &[];

/// How many cases a side of a run replayed, and how many the platform clock kept it
/// from carrying.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Coverage {
    /// The cases the side replayed.
    pub checked: usize,
    /// The cases the side left out because the platform clock cannot carry the instant.
    pub skipped: usize,
}

/// The reason the oracle side leaves a case out on a platform whose clock is coarse.
pub const BRIDGE_REASON: &str = "the platform clock cannot carry this fraction";

/// Whether a case can be replayed through the platform `SystemTime` at `tick_nanos`.
///
/// A parse case feeds text to an implementation and never crosses the bridge; a format
/// case carries its instant in the input, so its fraction must be a multiple of the tick.
pub fn case_is_carried(case: &Case, tick_nanos: u32) -> bool {
    match case.direction {
        Direction::Parse => true,
        Direction::Format => {
            let (_, nanos) = decode_instant(&case.input).expect("a format case carries a canonical instant");
            carries_faithfully(nanos, tick_nanos)
        }
    }
}

/// Replays the corpus through the oracle at a bridge tick and compares every carried
/// case against the frozen expectation.
///
/// The corpus keeps the values of the fixture; a case the platform clock cannot carry is
/// counted as skipped rather than being silently truncated into another instant.
pub fn oracle_replay(cases: &[Case], tick_nanos: u32) -> (Coverage, Vec<String>) {
    let mut coverage = Coverage::default();
    let mut failures = Vec::new();
    for case in cases {
        if !case_is_carried(case, tick_nanos) {
            coverage.skipped += 1;
            continue;
        }
        coverage.checked += 1;
        let outcome = run_case::<Oracle>(case);
        if !outcome.matches(&case.expected) {
            failures.push(format!(
                "{}: fixture {} but oracle {}",
                case.id(),
                render_expected(&case.expected),
                outcome.render()
            ));
        }
    }
    (coverage, failures)
}

/// Compares the oracle and the candidate against the frozen expectation of one case.
///
/// Returns none when the platform clock cannot carry the case, so the oracle side is
/// left out and the caller counts it as skipped. The candidate is lossless and runs
/// every case; the differences are reported against both sides, so the message says
/// which side moved.
pub fn compare_case(case: &Case, tick_nanos: u32) -> Option<Vec<String>> {
    if !case_is_carried(case, tick_nanos) {
        return None;
    }
    let oracle = run_case::<Oracle>(case);
    let candidate = run_case::<Candidate>(case);
    let mut detail = Vec::new();
    if candidate.render() != oracle.render() {
        detail.push(format!("oracle {} but candidate {}", oracle.render(), candidate.render()));
    }
    if !candidate.matches(&case.expected) {
        detail.push(format!(
            "fixture {} but candidate {}",
            render_expected(&case.expected),
            candidate.render()
        ));
    }
    Some(detail)
}

/// Runs every case against the oracle, the fixture and the candidate.
///
/// The candidate runs on every case; the oracle runs only on the cases the platform
/// clock can carry, and the rest are counted as skipped. The difference table is checked
/// for stale entries at the end, because the table belongs to the frozen corpus.
pub fn candidate_differential(cases: &[Case], tick_nanos: u32) -> Differential {
    let mut report = Differential::default();
    let mut recorded = BTreeSet::new();
    for case in cases {
        report.candidate_checked += 1;
        let Some(detail) = compare_case(case, tick_nanos) else {
            report.oracle_skipped += 1;
            let candidate = run_case::<Candidate>(case);
            if !candidate.matches(&case.expected) {
                report.problems.push(format!(
                    "{}: fixture {} but candidate {} (the oracle side is left out here: {BRIDGE_REASON})",
                    case.id(),
                    render_expected(&case.expected),
                    candidate.render()
                ));
            }
            continue;
        };
        report.oracle_checked += 1;
        if detail.is_empty() {
            continue;
        }
        match KNOWN_DIVERGENCES.iter().find(|(id, _)| *id == case.id()) {
            Some((id, reason)) => {
                recorded.insert(*id);
                report.recorded.push(format!("{id}: {reason}: {}", detail.join("; ")));
            }
            None => report
                .problems
                .push(format!("{}: unexplained difference: {}", case.id(), detail.join("; "))),
        }
    }
    for (id, reason) in KNOWN_DIVERGENCES {
        if !recorded.contains(id) {
            report
                .problems
                .push(format!("{id}: the recorded divergence ({reason}) no longer occurs"));
        }
    }
    report
}

/// The property failures of the candidate over the instants of the fixture.
pub fn candidate_property_failures(cases: &[Case]) -> Vec<String> {
    property_failures::<Candidate>(cases)
}

/// The implementation that this crate ships.
///
/// The adapter only uses the public surface: the value type, the format selector, the
/// parse and format entry points, the canonical accessors, and the error enums.
mod candidate {
    use crate::harness::support::{Format, system_time};

    use super::TimeApi;

    /// The candidate implementation, taken from this crate.
    #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
    pub struct Candidate(s3s_time::Timestamp);

    impl TimeApi for Candidate {
        type Error = CandidateError;

        fn from_instant(secs: i64, nanos: u32) -> Self {
            Self(s3s_time::Timestamp::from(system_time(secs, nanos)))
        }

        fn parse(format: Format, text: &str) -> Result<Self, Self::Error> {
            s3s_time::Timestamp::parse(candidate_format(format), text)
                .map(Self)
                .map_err(CandidateError::Parse)
        }

        fn format(&self, format: Format) -> Result<String, Self::Error> {
            let mut buf = Vec::new();
            self.0
                .format(candidate_format(format), &mut buf)
                .map_err(CandidateError::Format)?;
            Ok(String::from_utf8(buf).expect("the wire formats are ASCII"))
        }

        fn canonical(&self) -> (i64, u32) {
            (self.0.unix_seconds(), self.0.subsec_nanos())
        }

        fn error_name(error: &Self::Error) -> &'static str {
            error.contract_name()
        }
    }

    /// Maps a fixture format onto the candidate format.
    fn candidate_format(format: Format) -> s3s_time::TimestampFormat {
        match format {
            Format::DateTime => s3s_time::TimestampFormat::DateTime,
            Format::HttpDate => s3s_time::TimestampFormat::HttpDate,
            Format::EpochSeconds => s3s_time::TimestampFormat::EpochSeconds,
        }
    }

    /// The error of the candidate, keeping the contract classification.
    #[derive(Debug)]
    pub enum CandidateError {
        /// The parser rejected the input.
        Parse(s3s_time::ParseTimestampError),
        /// The writer rejected the instant.
        Format(s3s_time::FormatTimestampError),
    }

    impl CandidateError {
        /// Maps the variants onto the contract names of the fixture.
        fn contract_name(&self) -> &'static str {
            match self {
                Self::Parse(error) => match error {
                    s3s_time::ParseTimestampError::InvalidFormat => "InvalidFormat",
                    s3s_time::ParseTimestampError::OutOfRange => "OutOfRange",
                    s3s_time::ParseTimestampError::FractionTooLong => "FractionTooLong",
                    s3s_time::ParseTimestampError::Overflow => "Overflow",
                },
                Self::Format(error) => match error {
                    s3s_time::FormatTimestampError::OutOfRange => "OutOfRange",
                    s3s_time::FormatTimestampError::Io(_) => "Io",
                },
            }
        }
    }
}

pub use candidate::Candidate;
