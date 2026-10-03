// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

//! Boundary cases of the three wire formats.

use std::time::Duration;
use std::time::SystemTime;

use s3s_time::ConvertTimestampError;
use s3s_time::FormatTimestampError;
use s3s_time::ParseTimestampError;
use s3s_time::Timestamp;
use s3s_time::TimestampFormat;

fn parse(format: TimestampFormat, s: &str) -> Result<Timestamp, ParseTimestampError> {
    Timestamp::parse(format, s)
}

fn write(ts: &Timestamp, format: TimestampFormat) -> Result<String, FormatTimestampError> {
    let mut buf = Vec::new();
    ts.format(format, &mut buf)?;
    Ok(String::from_utf8(buf).expect("the wire formats are ASCII"))
}

fn write_ok(ts: &Timestamp, format: TimestampFormat) -> String {
    write(ts, format).expect("the instant is representable in the format")
}

#[test]
fn default_is_the_unix_epoch() {
    assert_eq!(Timestamp::default(), Timestamp::UNIX_EPOCH);
    assert_eq!(Timestamp::UNIX_EPOCH.unix_seconds(), 0);
    assert_eq!(Timestamp::UNIX_EPOCH.subsec_nanos(), 0);
}

#[test]
fn constructors_reject_out_of_range_values() {
    assert!(Timestamp::from_unix_seconds(i64::MAX).is_err());
    assert!(Timestamp::from_unix_seconds(i64::MIN).is_err());
    assert!(Timestamp::from_unix_nanos(i128::MAX).is_err());
    assert!(Timestamp::from_unix_nanos(i128::MIN).is_err());
    assert!(Timestamp::from_unix_seconds(0).is_ok());
}

#[test]
fn date_time_accepts_four_digit_years() {
    let first = parse(TimestampFormat::DateTime, "0000-01-01T00:00:00Z").unwrap();
    assert_eq!(write_ok(&first, TimestampFormat::DateTime), "0000-01-01T00:00:00.000Z");

    let last = parse(TimestampFormat::DateTime, "9999-12-15T22:46:38Z").unwrap();
    assert_eq!(write_ok(&last, TimestampFormat::DateTime), "9999-12-15T22:46:38.000Z");
}

#[test]
fn the_representable_range_is_narrower_than_the_year_range() {
    // The internal representation reserves the maximum UTC offset at both ends
    // of the four-digit year range, so the first hours of year -9999 and the
    // last hours of year 9999 cannot be represented. The previous
    // implementation covered the whole year range; the difference is recorded
    // for the parity stage.
    let last = 253_402_207_200_i64;
    assert!(Timestamp::from_unix_seconds(last).is_ok());
    assert!(Timestamp::from_unix_seconds(last + 1).is_err());
    assert!(Timestamp::from_unix_nanos(i128::from(last) * 1_000_000_000 + 999_999_999).is_ok());
    assert!(Timestamp::from_unix_nanos(i128::from(last + 1) * 1_000_000_000).is_err());

    let first = -377_705_023_201_i64;
    assert!(Timestamp::from_unix_seconds(first).is_ok());
    assert!(Timestamp::from_unix_seconds(first - 1).is_err());

    assert!(parse(TimestampFormat::DateTime, "9999-12-31T00:00:00Z").is_err());
    assert!(parse(TimestampFormat::EpochSeconds, "253402207201").is_err());
}

#[test]
fn date_time_spells_an_expanded_year_with_six_digits() {
    // The underlying parser requires a sign and six digits for a year outside
    // the four-digit range; the previous implementation accepted the bare form.
    assert!(parse(TimestampFormat::DateTime, "-9999-01-20T23:47:31Z").is_err());
    assert!(parse(TimestampFormat::DateTime, "-009999-01-20T23:47:31Z").is_ok());
}

#[test]
fn date_time_rejects_years_outside_the_wire_format() {
    for value in [
        "10000-01-01T00:00:00Z",
        "-0001-01-01T00:00:00Z",
        "2024-13-01T00:00:00Z",
        "2024-00-01T00:00:00Z",
    ] {
        let result = parse(TimestampFormat::DateTime, value);
        assert!(result.is_err(), "{value}");
    }
}

#[test]
fn date_time_checks_the_calendar() {
    assert!(parse(TimestampFormat::DateTime, "2024-02-29T00:00:00Z").is_ok());
    assert!(parse(TimestampFormat::DateTime, "2023-02-29T00:00:00Z").is_err());
    assert!(parse(TimestampFormat::DateTime, "2024-04-31T00:00:00Z").is_err());
    assert!(parse(TimestampFormat::DateTime, "2024-06-15T24:00:00Z").is_err());
}

#[test]
fn formatting_rejects_years_outside_the_wire_format() {
    // One second before 0000-01-01T00:00:00Z is in the year -1.
    let ts = Timestamp::from_unix_seconds(-62_167_219_201).unwrap();
    assert!(matches!(write(&ts, TimestampFormat::DateTime), Err(FormatTimestampError::OutOfRange)));
    assert!(matches!(write(&ts, TimestampFormat::HttpDate), Err(FormatTimestampError::OutOfRange)));
    assert_eq!(write_ok(&ts, TimestampFormat::EpochSeconds), "-62167219201");
}

#[test]
fn http_date_does_not_check_the_weekday() {
    let correct = parse(TimestampFormat::HttpDate, "Sat, 15 Jun 2024 07:00:00 GMT").unwrap();
    let wrong = parse(TimestampFormat::HttpDate, "Mon, 15 Jun 2024 07:00:00 GMT").unwrap();
    assert_eq!(correct, wrong);
}

#[test]
fn http_date_rejects_fractional_seconds() {
    let result = parse(TimestampFormat::HttpDate, "Sat, 15 Jun 2024 07:00:00.123 GMT");
    assert!(result.is_err());
}

#[test]
fn http_date_names_are_matched_case_insensitively() {
    // The underlying parser accepts any case for the day name and for the zone,
    // while the previous implementation required the exact IMF-fixdate
    // spelling. The difference is recorded for the parity stage.
    assert!(parse(TimestampFormat::HttpDate, "sat, 15 Jun 2024 07:00:00 GMT").is_ok());
    assert!(parse(TimestampFormat::HttpDate, "Sat, 15 Jun 2024 07:00:00 gmt").is_ok());
}

#[test]
fn epoch_seconds_accepts_one_to_nine_fraction_digits() {
    let cases: [(&str, i128); 9] = [
        ("100.1", 100_100_000_000),
        ("100.12", 100_120_000_000),
        ("100.123", 100_123_000_000),
        ("100.1234", 100_123_400_000),
        ("100.12345", 100_123_450_000),
        ("100.123456", 100_123_456_000),
        ("100.1234567", 100_123_456_700),
        ("100.12345678", 100_123_456_780),
        ("100.123456789", 100_123_456_789),
    ];

    for (text, nanos) in cases {
        let ts = parse(TimestampFormat::EpochSeconds, text).unwrap();
        assert_eq!(ts.unix_nanos(), nanos, "{text}");
    }
}

#[test]
fn epoch_seconds_rejects_malformed_input() {
    for text in ["", ".", "1.", ".5", "1e3", " 1", "1 ", "+1", "-", "--1", "1.2.3", "abc"] {
        let result = parse(TimestampFormat::EpochSeconds, text);
        assert!(matches!(result, Err(ParseTimestampError::InvalidFormat)), "{text:?}");
    }

    assert!(matches!(
        parse(TimestampFormat::EpochSeconds, "100.1234567890"),
        Err(ParseTimestampError::FractionTooLong)
    ));
    assert!(matches!(
        parse(TimestampFormat::EpochSeconds, "9223372036854775807"),
        Err(ParseTimestampError::OutOfRange | ParseTimestampError::Overflow)
    ));
    assert!(matches!(
        parse(TimestampFormat::EpochSeconds, "-9223372036854775808"),
        Err(ParseTimestampError::OutOfRange | ParseTimestampError::Overflow)
    ));
}

#[test]
fn epoch_seconds_accepts_leading_zeros_and_negative_zero() {
    let padded = parse(TimestampFormat::EpochSeconds, "007").unwrap();
    assert_eq!(padded.unix_seconds(), 7);

    let negative_zero = parse(TimestampFormat::EpochSeconds, "-0").unwrap();
    assert_eq!(negative_zero, Timestamp::UNIX_EPOCH);
}

#[test]
fn epoch_seconds_fraction_is_always_positive() {
    let ts = parse(TimestampFormat::EpochSeconds, "-1.5").unwrap();
    assert_eq!(ts.unix_nanos(), -500_000_000);
    assert_eq!(ts.unix_seconds(), -1);
    assert_eq!(ts.subsec_nanos(), 500_000_000);
}

#[test]
fn epoch_seconds_writes_the_shortest_exact_decimal() {
    let cases: [(i128, &str); 8] = [
        (0, "0"),
        (100_000_000, "0.1"),
        (10_000, "0.00001"),
        (1, "0.000000001"),
        (123_456_789, "0.123456789"),
        (-1, "-1.999999999"),
        (-500_000_000, "-1.5"),
        (-1_500_000_000, "-2.5"),
    ];

    for (nanos, expected) in cases {
        let ts = Timestamp::from_unix_nanos(nanos).unwrap();
        assert_eq!(write_ok(&ts, TimestampFormat::EpochSeconds), expected, "{nanos}");
    }
}

#[test]
fn subsecond_component_is_never_negative() {
    assert_eq!(Timestamp::from_unix_nanos(-1).unwrap().subsec_nanos(), 999_999_999);
    assert_eq!(Timestamp::from_unix_nanos(1).unwrap().subsec_nanos(), 1);
    assert_eq!(Timestamp::from_unix_nanos(1_000_000_000).unwrap().subsec_nanos(), 0);
}

#[test]
fn the_three_formats_describe_the_same_instant() {
    let ts = Timestamp::from_unix_seconds(1_718_434_800).unwrap();
    assert_eq!(write_ok(&ts, TimestampFormat::DateTime), "2024-06-15T07:00:00.000Z");
    assert_eq!(write_ok(&ts, TimestampFormat::HttpDate), "Sat, 15 Jun 2024 07:00:00 GMT");
    assert_eq!(write_ok(&ts, TimestampFormat::EpochSeconds), "1718434800");

    for format in [
        TimestampFormat::DateTime,
        TimestampFormat::HttpDate,
        TimestampFormat::EpochSeconds,
    ] {
        let text = write_ok(&ts, format);
        assert_eq!(parse(format, &text).unwrap(), ts, "{format:?}");
    }
}

#[test]
fn system_time_round_trip() {
    assert_eq!(Timestamp::from(SystemTime::UNIX_EPOCH), Timestamp::UNIX_EPOCH);

    let later = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
    let ts = Timestamp::from(later);
    assert_eq!(ts.unix_seconds(), 1_700_000_000);
    assert_eq!(SystemTime::try_from(ts).unwrap(), later);
}

#[cfg(feature = "serde")]
#[test]
fn serde_uses_the_date_time_format() {
    let ts = parse(TimestampFormat::DateTime, "2024-06-15T07:00:00.123Z").unwrap();
    let json = serde_json::to_string(&ts).unwrap();
    assert_eq!(json, "\"2024-06-15T07:00:00.123Z\"");
    let back: Timestamp = serde_json::from_str(&json).unwrap();
    assert_eq!(ts, back);
    assert!(serde_json::from_str::<Timestamp>("\"not-a-date\"").is_err());
}

/// The Windows epoch, `1601-01-01T00:00:00Z`, in seconds from the Unix epoch.
///
/// A Windows `SystemTime` counts unsigned 100-nanosecond intervals from this instant, so
/// the second itself is representable there as `intervals: 0` while the second before it is
/// not; a Unix `SystemTime` reaches both as negative seconds.
const WINDOWS_EPOCH_SECONDS: i64 = -11_644_473_600;

/// Builds the same instant with the standard library alone, or none when the platform
/// cannot represent it.
///
/// The reference is split from the total nanoseconds instead of from the floor second
/// and the positive adjustment the conversion uses, so agreement between the two is a
/// statement about the arithmetic rather than a restatement of the implementation.
fn std_instant(seconds: i64, nanoseconds: u32) -> Option<SystemTime> {
    let total = i128::from(seconds) * 1_000_000_000 + i128::from(nanoseconds);
    let magnitude = total.unsigned_abs();
    let whole = u64::try_from(magnitude / 1_000_000_000).expect("the seconds fit in u64");
    let part = u32::try_from(magnitude % 1_000_000_000).expect("the nanoseconds fit in u32");

    if total >= 0 {
        SystemTime::UNIX_EPOCH.checked_add(Duration::new(whole, part))
    } else {
        SystemTime::UNIX_EPOCH.checked_sub(Duration::new(whole, part))
    }
}

#[test]
fn the_system_time_conversion_follows_the_platform_range() {
    // A Windows `SystemTime` starts at the constant above while a Unix one counts back to
    // i64::MIN seconds, so this probe names the range the platform offers: it asks whether the
    // second *before* 1601 can be reached.
    let reaches_before_1601 = SystemTime::UNIX_EPOCH
        .checked_sub(Duration::from_secs((WINDOWS_EPOCH_SECONDS - 1).unsigned_abs()))
        .is_some();

    let cases: [(i64, u32); 7] = [
        (0, 0),
        (-1, 1),
        (-1, 999_999_999),
        (-2, 500_000_000),
        (1_700_000_000, 123_456_789),
        (-62_167_219_201, 0),
        (253_402_207_200, 999_999_999),
    ];

    for (seconds, nanoseconds) in cases {
        let nanos = i128::from(seconds) * 1_000_000_000 + i128::from(nanoseconds);
        let ts = Timestamp::from_unix_nanos(nanos).expect("the instant is representable");
        let expected = std_instant(seconds, nanoseconds);

        match (SystemTime::try_from(ts), expected) {
            (Ok(got), Some(want)) => {
                assert_eq!(got, want, "({seconds}, {nanoseconds}) must equal the instant std builds");
            }
            (Err(err), None) => {
                assert_eq!(err, ConvertTimestampError::OutOfRange, "({seconds}, {nanoseconds}) must report the range");
            }
            (got, want) => {
                panic!("({seconds}, {nanoseconds}): the conversion {got:?} disagrees with the platform range {want:?}")
            }
        }
    }

    if !reaches_before_1601 {
        // The second before the Windows epoch is the first instant Windows cannot represent:
        // the conversion must report the range error rather than panic the way the library
        // conversion did.
        let before_windows_epoch = Timestamp::from_unix_nanos(i128::from(WINDOWS_EPOCH_SECONDS - 1) * 1_000_000_000)
            .expect("the instant is representable");
        assert_eq!(SystemTime::try_from(before_windows_epoch), Err(ConvertTimestampError::OutOfRange));
    }
}

#[test]
fn the_windows_epoch_itself_is_representable_on_every_platform() {
    // 1601-01-01T00:00:00Z is the Windows epoch (`intervals: 0`) and a reachable negative
    // instant on Unix, so every platform converts it. Its predecessor is where Windows stops,
    // which the platform-range test above pins.
    let ts = Timestamp::from_unix_nanos(i128::from(WINDOWS_EPOCH_SECONDS) * 1_000_000_000).expect("the instant is representable");
    let expected = std_instant(WINDOWS_EPOCH_SECONDS, 0).expect("every platform reaches 1601-01-01");
    assert_eq!(SystemTime::try_from(ts).expect("the Windows epoch converts"), expected);
}
