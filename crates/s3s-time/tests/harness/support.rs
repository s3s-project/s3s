// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

//! The schema of the frozen fixture and the arithmetic that both sides share.
//!
//! The fixture in tests/golden/contract.txt holds one line per case: the direction
//! together with the wire format, the input, the expected result, and the intent of
//! the case. The generator example and the integration tests compile this module, so
//! the writer and the readers cannot drift apart.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

/// The fixture path relative to the crate root.
pub const FIXTURE_PATH: &str = "tests/golden/contract.txt";

/// The Smithy wire formats covered by the fixture.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    /// RFC 3339 date-time, written with exactly three fractional digits.
    DateTime,
    /// IMF-fixdate, the date form used by HTTP.
    HttpDate,
    /// Seconds since the Unix epoch, written as the shortest exact decimal.
    EpochSeconds,
}

impl Format {
    /// The column name of the format.
    pub fn name(self) -> &'static str {
        match self {
            Self::DateTime => "date-time",
            Self::HttpDate => "http-date",
            Self::EpochSeconds => "epoch-seconds",
        }
    }

    /// Reads the column name of the format.
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "date-time" => Some(Self::DateTime),
            "http-date" => Some(Self::HttpDate),
            "epoch-seconds" => Some(Self::EpochSeconds),
            _ => None,
        }
    }
}

/// The direction of a fixture line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    /// The input is text to parse.
    Parse,
    /// The input is an instant to write.
    Format,
}

impl Direction {
    /// The column name of the direction.
    pub fn name(self) -> &'static str {
        match self {
            Self::Parse => "parse",
            Self::Format => "format",
        }
    }

    /// Reads the column name of the direction.
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "parse" => Some(Self::Parse),
            "format" => Some(Self::Format),
            _ => None,
        }
    }
}

/// The expected result of one case.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Expected {
    /// The case produces an instant, as canonical seconds and nanoseconds.
    Instant { secs: i64, nanos: u32 },
    /// The case produces this exact text.
    Text(String),
    /// The case fails with this contract error name.
    Error(String),
}

/// One corner of the fixed corpus, before the oracle has produced its result.
#[derive(Debug, Clone)]
pub struct Spec {
    /// The direction of the case.
    pub direction: Direction,
    /// The wire format of the case.
    pub format: Format,
    /// The input text; a format case spells an instant as "<seconds>:<nanoseconds>".
    pub input: String,
    /// The reason why the case belongs to the corpus.
    pub intent: String,
}

impl Spec {
    /// Builds a parse case.
    pub fn parse(format: Format, input: &str, id: &str, note: &str) -> Self {
        Self {
            direction: Direction::Parse,
            format,
            input: input.to_owned(),
            intent: intent_of(id, note),
        }
    }

    /// Builds a format case from a canonical instant.
    pub fn format(format: Format, secs: i64, nanos: u32, id: &str, note: &str) -> Self {
        Self {
            direction: Direction::Format,
            format,
            input: encode_instant(secs, nanos),
            intent: intent_of(id, note),
        }
    }
}

/// Joins a case id and a note into the intent column.
fn intent_of(id: &str, note: &str) -> String {
    if note.is_empty() {
        id.to_owned()
    } else {
        format!("{id} {note}")
    }
}

/// One fixture line.
#[derive(Debug, Clone)]
pub struct Case {
    /// The direction of the case.
    pub direction: Direction,
    /// The wire format of the case.
    pub format: Format,
    /// The input text in its fixture encoding.
    pub input: String,
    /// The expected result, produced by the oracle at generation time.
    pub expected: Expected,
    /// The case id followed by an optional note.
    pub intent: String,
}

impl Case {
    /// Attaches an observed result to a corpus case.
    ///
    /// # Errors
    ///
    /// Returns the reason when the input of a format case is not a canonical instant,
    /// which is a defect of the corpus.
    pub fn new(spec: &Spec, expected: Expected) -> Result<Self, String> {
        let input = match spec.direction {
            Direction::Parse => spec.input.clone(),
            Direction::Format => {
                let (secs, nanos) = decode_instant(&spec.input)?;
                encode_instant(secs, nanos)
            }
        };
        Ok(Self {
            direction: spec.direction,
            format: spec.format,
            input,
            expected,
            intent: spec.intent.clone(),
        })
    }

    /// The case identifier: the first token of the intent column.
    pub fn id(&self) -> &str {
        self.intent.split_whitespace().next().unwrap_or("")
    }

    /// The canonical instant of a format case.
    ///
    /// # Panics
    ///
    /// Panics when the input is not a canonical instant; the fixture parser rejects
    /// such a line before it reaches a test.
    pub fn instant(&self) -> (i64, u32) {
        match decode_instant(&self.input) {
            Ok(instant) => instant,
            Err(error) => panic!("{}: {error}", self.id()),
        }
    }
}

/// Renders the expected column of one case.
pub fn render_expected(expected: &Expected) -> String {
    match expected {
        Expected::Instant { secs, nanos } => format!("instant:{secs}:{nanos}"),
        Expected::Text(text) => format!("text:{}", escape(text)),
        Expected::Error(name) => format!("error:{name}"),
    }
}

/// Renders one fixture line, without the trailing newline.
pub fn render_line(case: &Case) -> String {
    let column = format!("{}/{}", case.direction.name(), case.format.name());
    let input = match case.direction {
        Direction::Parse => escape(&case.input),
        Direction::Format => case.input.clone(),
    };
    format!("{column}\t{input}\t{}\t{}", render_expected(&case.expected), case.intent)
}

/// Reads the fixture that is committed next to the tests.
///
/// # Errors
///
/// Returns the reason when the fixture cannot be read or does not follow the schema.
pub fn read_fixture() -> Result<Vec<Case>, String> {
    let path = fixture_path();
    let text = std::fs::read_to_string(&path).map_err(|error| format!("cannot read {}: {error}", path.display()))?;
    parse_fixture(&text)
}

/// Parses the fixture and checks the documented shape of every line.
///
/// # Errors
///
/// Returns the reason of the first malformed line.
pub fn parse_fixture(text: &str) -> Result<Vec<Case>, String> {
    let mut cases = Vec::new();
    let mut ids = BTreeSet::new();
    for (index, line) in text.lines().enumerate() {
        let number = index + 1;
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let fields: Vec<&str> = line.split('\t').collect();
        if fields.len() != 4 {
            return Err(format!("line {number}: expected four tab-separated fields, found {}", fields.len()));
        }
        let (column, input, expected, intent) = (fields[0], fields[1], fields[2], fields[3]);
        let Some((direction, format)) = column.split_once('/') else {
            return Err(format!("line {number}: invalid column {column:?}"));
        };
        let Some(direction) = Direction::from_name(direction) else {
            return Err(format!("line {number}: invalid direction in {column:?}"));
        };
        let Some(format) = Format::from_name(format) else {
            return Err(format!("line {number}: invalid format in {column:?}"));
        };
        let input = match direction {
            Direction::Parse => unescape(input).map_err(|error| format!("line {number}: {error}"))?,
            Direction::Format => {
                let (secs, nanos) = decode_instant(input).map_err(|error| format!("line {number}: {error}"))?;
                encode_instant(secs, nanos)
            }
        };
        let expected = parse_expected(expected, number)?;
        let wrong_kind = matches!(
            (direction, &expected),
            (Direction::Parse, Expected::Text(_)) | (Direction::Format, Expected::Instant { .. })
        );
        if wrong_kind {
            return Err(format!("line {number}: the expectation does not belong to the direction {direction:?}"));
        }
        let id = intent.split_whitespace().next().unwrap_or("");
        if id.is_empty() {
            return Err(format!("line {number}: the intent column needs a case id"));
        }
        if !ids.insert(id.to_owned()) {
            return Err(format!("line {number}: duplicate case id {id}"));
        }
        cases.push(Case {
            direction,
            format,
            input,
            expected,
            intent: intent.to_owned(),
        });
    }
    Ok(cases)
}

/// Parses the expected column of one case.
fn parse_expected(field: &str, number: usize) -> Result<Expected, String> {
    if let Some(rest) = field.strip_prefix("instant:") {
        let (secs, nanos) = decode_instant(rest).map_err(|error| format!("line {number}: {error}"))?;
        return Ok(Expected::Instant { secs, nanos });
    }
    if let Some(rest) = field.strip_prefix("text:") {
        return Ok(Expected::Text(unescape(rest).map_err(|error| format!("line {number}: {error}"))?));
    }
    if let Some(rest) = field.strip_prefix("error:") {
        return Ok(Expected::Error(rest.to_owned()));
    }
    Err(format!("line {number}: invalid expected field {field:?}"))
}

/// Escapes the characters that a fixture line cannot carry literally.
pub fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '\t' => out.push_str("\\t"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            _ => out.push(ch),
        }
    }
    out
}

/// Reverses the escaping of the fixture text fields.
///
/// # Errors
///
/// Returns the reason when the text carries an unknown or a dangling escape.
pub fn unescape(text: &str) -> Result<String, String> {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(ch) = chars.next() {
        if ch != '\\' {
            out.push(ch);
            continue;
        }
        let Some(escaped) = chars.next() else {
            return Err(format!("dangling escape in {text:?}"));
        };
        match escaped {
            '\\' => out.push('\\'),
            't' => out.push('\t'),
            'n' => out.push('\n'),
            'r' => out.push('\r'),
            other => return Err(format!("unknown escape \\{other} in {text:?}")),
        }
    }
    Ok(out)
}

/// Writes a canonical instant as the input column of a format case.
pub fn encode_instant(secs: i64, nanos: u32) -> String {
    format!("{secs}:{nanos}")
}

/// Reads the input column of a format case.
///
/// # Errors
///
/// Returns the reason when the field is not a canonical instant.
pub fn decode_instant(text: &str) -> Result<(i64, u32), String> {
    let Some((secs, nanos)) = text.split_once(':') else {
        return Err(format!("invalid instant {text:?}"));
    };
    let secs: i64 = secs.parse().map_err(|_| format!("invalid instant seconds in {text:?}"))?;
    let nanos: u32 = nanos
        .parse()
        .map_err(|_| format!("invalid instant nanoseconds in {text:?}"))?;
    if nanos >= 1_000_000_000 {
        return Err(format!("instant nanoseconds are not below one second in {text:?}"));
    }
    Ok((secs, nanos))
}

/// Builds a system time from canonical seconds and a non-negative adjustment.
///
/// The seconds value is the floor second and the adjustment is always positive, which
/// is the epoch-seconds convention: minus one second and five hundred million
/// nanoseconds is half a second before the epoch, not one and a half seconds before it.
///
/// # Panics
///
/// Panics when the adjustment is not below one second.
pub fn system_time(secs: i64, nanos: u32) -> SystemTime {
    assert!(nanos < 1_000_000_000, "the nanosecond adjustment must be below one second");
    if secs >= 0 {
        let magnitude = u64::try_from(secs).expect("a non-negative i64 fits in u64");
        SystemTime::UNIX_EPOCH + Duration::new(magnitude, nanos)
    } else {
        let magnitude = secs.unsigned_abs();
        let (whole, part) = if nanos == 0 {
            (magnitude, 0)
        } else {
            (magnitude - 1, 1_000_000_000 - nanos)
        };
        SystemTime::UNIX_EPOCH - Duration::new(whole, part)
    }
}

/// Builds a system time from canonical seconds and a non-negative adjustment, when the
/// platform can represent the instant.
///
/// The representable range is platform dependent: a Unix `SystemTime` counts signed
/// seconds from the epoch, while a Windows `SystemTime` counts unsigned 100-nanosecond
/// ticks from 1601-01-01, so instants before that date cannot be built there. The helper
/// therefore returns none instead of panicking, and the property runs count the instants
/// the platform cannot represent as skipped on every platform.
///
/// # Panics
///
/// Panics when the adjustment is not below one second.
pub fn checked_system_time(secs: i64, nanos: u32) -> Option<SystemTime> {
    assert!(nanos < 1_000_000_000, "the nanosecond adjustment must be below one second");
    if secs >= 0 {
        let magnitude = u64::try_from(secs).expect("a non-negative i64 fits in u64");
        SystemTime::UNIX_EPOCH.checked_add(Duration::new(magnitude, nanos))
    } else {
        let magnitude = secs.unsigned_abs();
        let (whole, part) = if nanos == 0 {
            (magnitude, 0)
        } else {
            (magnitude - 1, 1_000_000_000 - nanos)
        };
        SystemTime::UNIX_EPOCH.checked_sub(Duration::new(whole, part))
    }
}

/// Reads a platform `SystemTime` back as canonical seconds and a non-negative
/// adjustment, the way the epoch-seconds convention writes an instant.
///
/// Returns none when the instant cannot be expressed relative to the Unix epoch, which
/// is the case on a platform whose clock starts later than 1970.
pub fn read_back_system_time(time: SystemTime) -> Option<(i64, u32)> {
    match time.duration_since(SystemTime::UNIX_EPOCH) {
        Ok(duration) => Some((i64::try_from(duration.as_secs()).ok()?, duration.subsec_nanos())),
        Err(error) => {
            let duration = error.duration();
            let secs = i64::try_from(duration.as_secs()).ok()?;
            let nanos = duration.subsec_nanos();
            if nanos == 0 {
                Some((-secs, 0))
            } else {
                Some((-secs - 1, 1_000_000_000 - nanos))
            }
        }
    }
}

/// The tick of the platform `SystemTime`, in nanoseconds.
///
/// The property run carries every instant through a `SystemTime`, so its precision is
/// the precision of that bridge: a Unix clock counts nanoseconds, a Windows one counts
/// 100-nanosecond ticks from 1601 and drops anything finer. The tick is probed on the
/// platform itself instead of assumed, so both the corpus alignment and its test follow
/// the platform.
pub fn platform_tick_nanos() -> u32 {
    for tick in [1_u32, 100, 1_000, 10_000, 100_000, 1_000_000] {
        let Some(time) = checked_system_time(0, tick) else {
            continue;
        };
        if read_back_system_time(time) == Some((0, tick)) {
            return tick;
        }
    }
    1_000_000_000
}

/// Rounds an instant down to a multiple of `tick_nanos`.
///
/// The rounding is toward the past, so the aligned instant is the last tick boundary at
/// or before the input, which is the instant a platform clock stores when it is built
/// from a finer value. A tick of one nanosecond keeps the input unchanged.
pub fn quantize_to_tick(secs: i64, nanos: u32, tick_nanos: u32) -> (i64, u32) {
    let tick = i128::from(tick_nanos.max(1));
    let total = i128::from(secs) * 1_000_000_000 + i128::from(nanos);
    let aligned = total.div_euclid(tick) * tick;
    let secs = i64::try_from(aligned.div_euclid(1_000_000_000)).expect("the seconds fit in i64");
    let nanos = u32::try_from(aligned.rem_euclid(1_000_000_000)).expect("the nanoseconds fit in u32");
    (secs, nanos)
}

/// Whether a clock that ticks every `tick_nanos` keeps the fraction of an instant.
///
/// The oracle is reached through a platform `SystemTime`, so its precision is the
/// precision of that bridge: a fraction the clock cannot carry would be silently
/// truncated and the oracle would answer for a different instant. Such a case is left
/// out of the oracle side of a run and counted as skipped instead.
pub fn carries_faithfully(nanos: u32, tick_nanos: u32) -> bool {
    nanos.is_multiple_of(tick_nanos.max(1))
}

/// Reads the shortest exact decimal of an epoch-seconds value.
///
/// Both implementations write the integral part as the floor second and the fraction
/// as a positive adjustment, so minus one point five denotes minus half a second.
///
/// # Errors
///
/// Returns the reason when the text is not such a decimal.
pub fn decode_epoch_literal(text: &str) -> Result<(i64, u32), String> {
    let (integral, fraction) = match text.split_once('.') {
        Some((integral, fraction)) => (integral, Some(fraction)),
        None => (text, None),
    };
    let secs: i64 = integral
        .parse()
        .map_err(|_| format!("invalid epoch-seconds literal {text:?}"))?;
    let Some(fraction) = fraction else {
        return Ok((secs, 0));
    };
    if fraction.is_empty() || fraction.len() > 9 || !fraction.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(format!("invalid epoch-seconds fraction in {text:?}"));
    }
    if secs == 0 && text.starts_with('-') && fraction.bytes().any(|byte| byte != b'0') {
        return Err(format!("a negative fraction needs a negative floor second: {text:?}"));
    }
    let padded = format!("{fraction:0<9}");
    let nanos: u32 = padded
        .parse()
        .map_err(|_| format!("invalid epoch-seconds fraction in {text:?}"))?;
    Ok((secs, nanos))
}

/// The crate root directory.
pub fn manifest_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// The committed fixture.
pub fn fixture_path() -> PathBuf {
    manifest_dir().join(FIXTURE_PATH)
}

/// The workspace root, two levels above the crate root.
pub fn workspace_root() -> PathBuf {
    let manifest = manifest_dir();
    match manifest.parent().and_then(|path| path.parent()) {
        Some(root) => root.to_path_buf(),
        None => manifest,
    }
}

/// The version of the time library that the workspace lock file resolved.
///
/// The lock file is the only place that knows the version at generation time, and the
/// fixture header records it because the accepted input set follows the library
/// version.
pub fn locked_jiff_version() -> Option<String> {
    let lock = std::fs::read_to_string(workspace_root().join("Cargo.lock")).ok()?;
    let mut package: Option<&str> = None;
    for line in lock.lines() {
        let line = line.trim();
        if line == "[[package]]" {
            package = None;
            continue;
        }
        if let Some(name) = line.strip_prefix("name = ") {
            package = Some(name.trim_matches('"'));
            continue;
        }
        if let Some(version) = line.strip_prefix("version = ") {
            if package == Some("jiff") {
                return Some(version.trim_matches('"').to_owned());
            }
        }
    }
    None
}
