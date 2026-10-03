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

/// The earliest instant a Windows system clock can hold, in canonical seconds:
/// 1601-01-01T00:00:00Z.
pub const WINDOWS_EPOCH_SECONDS: i64 = -11_644_473_600;

/// The variable that injects the tick of the clock, in nanoseconds.
pub const TICK_VARIABLE: &str = "S3S_TIME_TICK_NANOS";

/// The variable that injects the earliest instant the clock can hold, in seconds.
pub const FLOOR_VARIABLE: &str = "S3S_TIME_FLOOR_SECONDS";

/// The clock that the bridge goes through: how fine it is and how far back it reaches.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlatformModel {
    /// The tick of the clock, in nanoseconds.
    pub tick_nanos: u32,
    /// The earliest instant the clock can hold, in canonical seconds.
    pub floor_seconds: i64,
    /// Whether the model came from the environment instead of the machine.
    pub injected: bool,
}

impl PlatformModel {
    /// The Unix clock: every nanosecond, down to the smallest seconds value.
    pub const UNIX: Self = Self {
        tick_nanos: 1,
        floor_seconds: i64::MIN,
        injected: false,
    };

    /// The Windows clock: hundred-nanosecond ticks from 1601-01-01.
    pub const WINDOWS: Self = Self {
        tick_nanos: 100,
        floor_seconds: WINDOWS_EPOCH_SECONDS,
        injected: false,
    };

    /// Whether the clock keeps the instant exactly.
    pub fn carries(&self, secs: i64, nanos: u32) -> bool {
        secs >= self.floor_seconds && carries_faithfully(nanos, self.tick_nanos)
    }

    /// Rounds an instant down to the tick of this clock.
    pub fn quantize(&self, secs: i64, nanos: u32) -> (i64, u32) {
        quantize_to_tick(secs, nanos, self.tick_nanos)
    }
}

/// The platform model of this run.
///
/// Without the environment the model is probed from the machine: the tick is the smallest
/// adjustment the clock keeps, and the floor is the earliest instant it holds. The two
/// variables inject a model instead, which lets a Linux run rehearse another clock:
///
/// `S3S_TIME_TICK_NANOS=100 S3S_TIME_FLOOR_SECONDS=-11644473600 cargo test -p s3s-time --all-features`
///
/// A value that does not parse stops the run: a typo must never fall back to the machine
/// silently. An injected model is announced on stdout, so a log shows which platform a
/// run rehearsed.
///
/// # Panics
///
/// Panics when an injected value is not a positive tick or a signed number of seconds.
pub fn platform_model() -> PlatformModel {
    static MODEL: std::sync::OnceLock<PlatformModel> = std::sync::OnceLock::new();
    *MODEL.get_or_init(|| {
        let tick = std::env::var(TICK_VARIABLE).ok();
        let floor = std::env::var(FLOOR_VARIABLE).ok();
        if tick.is_none() && floor.is_none() {
            let model = probe_platform_model();
            println!(
                "platform model: tick={} floor={} (probed on this machine)",
                model.tick_nanos, model.floor_seconds
            );
            return model;
        }
        let probed = probe_platform_model();
        let model = PlatformModel {
            tick_nanos: parse_tick(tick.as_deref()).unwrap_or(probed.tick_nanos),
            floor_seconds: parse_floor(floor.as_deref()).unwrap_or(probed.floor_seconds),
            injected: true,
        };
        println!(
            "simulated platform: tick={} floor={} (injected through {TICK_VARIABLE}/{FLOOR_VARIABLE})",
            model.tick_nanos, model.floor_seconds
        );
        model
    })
}

/// The tick of the platform clock, in nanoseconds, from the model of this run.
pub fn platform_tick_nanos() -> u32 {
    platform_model().tick_nanos
}

/// Reads a tick from an injected value, or stops the run when it does not parse.
fn parse_tick(value: Option<&str>) -> Option<u32> {
    let text = value?;
    match text.parse::<u32>() {
        Ok(tick) if tick > 0 => Some(tick),
        _ => panic!("{TICK_VARIABLE}={text} is not a positive number of nanoseconds"),
    }
}

/// Reads a floor from an injected value, or stops the run when it does not parse.
fn parse_floor(value: Option<&str>) -> Option<i64> {
    let text = value?;
    match text.parse::<i64>() {
        Ok(seconds) => Some(seconds),
        Err(error) => panic!("{FLOOR_VARIABLE}={text} is not a number of seconds: {error}"),
    }
}

/// Probes the machine: the tick it keeps and the earliest instant it holds.
fn probe_platform_model() -> PlatformModel {
    let reaches_before_windows = SystemTime::UNIX_EPOCH
        .checked_sub(Duration::from_secs(WINDOWS_EPOCH_SECONDS.unsigned_abs() + 1))
        .is_some();
    let floor_seconds = if reaches_before_windows {
        i64::MIN
    } else {
        WINDOWS_EPOCH_SECONDS
    };
    for tick in [1_u32, 100, 1_000, 10_000, 100_000, 1_000_000] {
        let Some(time) = checked_system_time(0, tick) else {
            continue;
        };
        if read_back_system_time(time) == Some((0, tick)) {
            return PlatformModel {
                tick_nanos: tick,
                floor_seconds,
                injected: false,
            };
        }
    }
    PlatformModel {
        tick_nanos: 1_000_000_000,
        floor_seconds,
        injected: false,
    }
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
        if let Some(version) = line.strip_prefix("version = ")
            && package == Some("jiff")
        {
            return Some(version.trim_matches('"').to_owned());
        }
    }
    None
}
