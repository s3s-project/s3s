// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

//! The behaviour under test, behind one trait.
//!
//! The fixture was generated from the pre-migration implementation before it was
//! deleted, and the generator and the oracle adapter went with it; the fixture is the
//! only oracle now. The candidate is this crate's own implementation, and the replay
//! compares it against the fixture case by case: every case must match byte for byte
//! except the ids listed in the post-migration table below.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fmt;
use std::hash::Hash;

use crate::harness::support::{Case, Direction, Expected, Format, decode_instant, render_expected};

/// One wire-format operation, as the frozen fixture describes it.
pub trait TimeApi: Sized + Clone + Eq + Ord + Hash + fmt::Debug {
    /// The error type of the implementation.
    type Error: fmt::Debug;

    /// Builds an instant from canonical seconds and a non-negative adjustment, or
    /// returns none when the implementation cannot represent the instant.
    fn try_from_instant(secs: i64, nanos: u32) -> Option<Self>;

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

/// Runs one case against an implementation, or nothing when the implementation cannot
/// represent the instant a format case carries.
///
/// # Panics
///
/// Panics when a format case does not carry a canonical instant, which the fixture
/// parser rejects before a test runs.
pub fn try_run<T: TimeApi>(direction: Direction, format: Format, input: &str) -> Option<Outcome> {
    match direction {
        Direction::Parse => Some(match T::parse(format, input) {
            Ok(timestamp) => {
                let (secs, nanos) = timestamp.canonical();
                Outcome::Instant { secs, nanos }
            }
            Err(error) => Outcome::Error(T::error_name(&error)),
        }),
        Direction::Format => {
            let (secs, nanos) = decode_instant(input).expect("a format case carries a canonical instant");
            // The instant of a format case comes from the fixture, so a refusal is a
            // property of the implementation rather than of the corpus: the case is
            // reported instead of being written as a different instant.
            let timestamp = T::try_from_instant(secs, nanos)?;
            Some(match timestamp.format(format) {
                Ok(text) => Outcome::Text(text),
                Err(error) => Outcome::Error(T::error_name(&error)),
            })
        }
    }
}

/// Runs one fixture case against an implementation, or nothing when the implementation
/// cannot represent the instant of a format case.
pub fn run_case<T: TimeApi>(case: &Case) -> Option<Outcome> {
    try_run::<T>(case.direction, case.format, &case.input)
}

/// Every instant that the fixture pins down, without duplicates and in order.
///
/// The corpus keeps the values of the fixture; an implementation that cannot represent
/// an instant is counted rather than being dropped silently.
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

/// How many instants of the fixture the implementation can represent, and how many
/// the property run skips because it cannot.
pub fn property_coverage<T: TimeApi>(cases: &[Case]) -> (usize, usize) {
    let instants = fixture_instants(cases);
    let representable = instants
        .iter()
        .filter(|&&(secs, nanos)| T::try_from_instant(secs, nanos).is_some())
        .count();
    (representable, instants.len() - representable)
}

/// The canonical view must survive construction.
fn check_canonical<T: TimeApi>(instants: &[(i64, u32)], failures: &mut Vec<String>) {
    for &(secs, nanos) in instants {
        let Some(timestamp) = T::try_from_instant(secs, nanos) else {
            continue;
        };
        let read_back = timestamp.canonical();
        if read_back != (secs, nanos) {
            failures.push(format!("try_from_instant({secs}, {nanos}) reads back as {read_back:?}"));
        }
    }
}

/// Parsing the output of a format must return the instant it can carry exactly.
fn check_round_trip<T: TimeApi>(instants: &[(i64, u32)], failures: &mut Vec<String>) {
    for &(secs, nanos) in instants {
        let Some(timestamp) = T::try_from_instant(secs, nanos) else {
            continue;
        };
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
        let Some(timestamp) = T::try_from_instant(secs, nanos) else {
            continue;
        };
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
        if let Some(timestamp) = T::try_from_instant(secs, nanos) {
            values.insert((secs, nanos), timestamp);
        }
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

/// Whether the candidate side is compiled in.
pub const CANDIDATE_AVAILABLE: bool = true;

/// The result of replaying the frozen fixture against this crate.
#[derive(Debug, Default)]
pub struct Replay {
    /// Differences that are neither absent nor recorded: fix the implementation or
    /// record the difference here and in the migration report.
    pub problems: Vec<String>,
    /// The recorded divergences that were observed, with the reason of each.
    pub recorded: Vec<String>,
    /// The cases that were replayed against the implementation. The implementation does
    /// not cross a platform clock, so it runs every case the corpus holds.
    pub checked: usize,
}

/// Cases where this crate deliberately differs from the frozen fixture.
///
/// The fixture records what the pre-migration implementation produced, so a difference
/// is expected exactly where the migration decided to change the behaviour. The id is
/// the first token of the fixture intent column. An entry whose case stops diverging is
/// a problem, and so is an entry whose id is not a case of the fixture, so the table
/// cannot go stale and a typo cannot hide in it.
pub const POST_MIGRATION_DIVERGENCES: &[(&str, &str)] = &[
    // The internal representation reserves the largest UTC offset at both ends of the
    // four-digit year range, which narrows the accepted instants by 93599 seconds at
    // each end.
    ("design-4.1/year-max", "intended: outside the internal range"),
    ("design-4.1/range-max-plus-one", "intended: outside the internal range"),
    ("design-4.3/year-9999-end", "intended: outside the internal range"),
    ("design-4.3/year-9999-end-fraction", "intended: outside the internal range"),
    ("design-4.3/year-minus-9999-start", "intended: outside the internal range"),
    ("design-4.3/year-minus-9999-fraction", "intended: outside the internal range"),
    ("design-4.3/range-min-minus-one", "intended: outside the internal range"),
    ("design-4.3/range-max-plus-one", "intended: outside the internal range"),
    // An explicit plus sign is not part of the grammar of the format.
    ("design-4.3/plus-prefix", "intended: no plus sign in the grammar"),
    // Both parsers reject the input; only the name of the error changes, because a
    // field outside its range reports the out-of-range variant.
    ("design-4.1/non-leap-day", "intended: out-of-range reports OutOfRange"),
    ("design-4.1/day-out-of-range", "intended: out-of-range reports OutOfRange"),
    ("design-4.1/month-out-of-range", "intended: out-of-range reports OutOfRange"),
    ("design-4.1/hour-out-of-range", "intended: out-of-range reports OutOfRange"),
    ("design-4.1/minute-out-of-range", "intended: out-of-range reports OutOfRange"),
    ("design-4.1/second-out-of-range", "intended: out-of-range reports OutOfRange"),
    ("design-4.2/non-leap-day", "intended: out-of-range reports OutOfRange"),
    ("design-4.2/day-out-of-range", "intended: out-of-range reports OutOfRange"),
    ("design-4.2/hour-out-of-range", "intended: out-of-range reports OutOfRange"),
];

/// Replays every case against this crate and the frozen fixture.
///
/// A case outside the post-migration table must match the fixture exactly: for a parse
/// case the parsed instant or the error name, for a format case the written bytes. A
/// case the implementation cannot represent is reported instead of being counted.
pub fn candidate_replay(cases: &[Case]) -> Replay {
    let mut report = Replay::default();
    let mut recorded = BTreeSet::new();

    for case in cases {
        let Some(outcome) = run_case::<Candidate>(case) else {
            report
                .problems
                .push(format!("{}: the candidate cannot represent a case of the corpus", case.id()));
            continue;
        };
        report.checked += 1;
        if outcome.matches(&case.expected) {
            continue;
        }
        match POST_MIGRATION_DIVERGENCES.iter().find(|(id, _)| *id == case.id()) {
            Some((id, reason)) => {
                recorded.insert(*id);
                report.recorded.push(format!(
                    "{id}: {reason}: fixture {} but implementation {}",
                    render_expected(&case.expected),
                    outcome.render()
                ));
            }
            None => report.problems.push(format!(
                "{}: unexpected difference from the frozen fixture: fixture {} but implementation {}",
                case.id(),
                render_expected(&case.expected),
                outcome.render()
            )),
        }
    }

    let fixture_ids: BTreeSet<&str> = cases.iter().map(Case::id).collect();

    for (id, reason) in POST_MIGRATION_DIVERGENCES {
        if !fixture_ids.contains(*id) {
            report
                .problems
                .push(format!("{id}: the recorded divergence is not a case of the frozen fixture"));
        } else if !recorded.contains(*id) {
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

/// The instants of the fixture that the candidate can represent.
pub fn candidate_property_coverage(cases: &[Case]) -> (usize, usize) {
    property_coverage::<Candidate>(cases)
}

/// The implementation that this crate ships.
///
/// The adapter only uses the public surface: the value type, the format selector, the
/// parse and format entry points, the canonical accessors, and the error enums.
mod candidate {
    use crate::harness::support::Format;

    use super::TimeApi;

    /// The candidate implementation, taken from this crate.
    #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
    pub struct Candidate(s3s_time::Timestamp);

    impl TimeApi for Candidate {
        type Error = CandidateError;

        fn try_from_instant(secs: i64, nanos: u32) -> Option<Self> {
            let nanoseconds = i128::from(secs) * 1_000_000_000 + i128::from(nanos);
            s3s_time::Timestamp::from_unix_nanos(nanoseconds).ok().map(Self)
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
