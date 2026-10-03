// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

//! The public contracts around the three wire formats: a writer that fails is
//! reported as an I/O error, and a system time outside the representable range
//! saturates to the end of the range instead of failing.

use std::io;
use std::time::{Duration, SystemTime};

use s3s_time::ConvertTimestampError;
use s3s_time::FormatTimestampError;
use s3s_time::Timestamp;
use s3s_time::TimestampFormat;

/// A writer that rejects the first write.
#[derive(Debug)]
struct FailingWriter;

impl io::Write for FailingWriter {
    fn write(&mut self, _buf: &[u8]) -> io::Result<usize> {
        Err(io::Error::other("the writer failed"))
    }

    fn flush(&mut self) -> io::Result<()> {
        Err(io::Error::other("the writer failed"))
    }
}

#[test]
fn a_failing_writer_is_reported_as_an_io_error() {
    let value = Timestamp::parse(TimestampFormat::DateTime, "2024-06-15T07:00:00.123Z").unwrap();

    for format in [
        TimestampFormat::DateTime,
        TimestampFormat::HttpDate,
        TimestampFormat::EpochSeconds,
    ] {
        let err = value.format(format, &mut FailingWriter).unwrap_err();
        assert!(
            matches!(err, FormatTimestampError::Io(_)),
            "{format:?} must report the writer error, got {err:?}"
        );
    }
}

#[test]
fn system_times_outside_the_range_saturate_to_the_ends() {
    // 8032 years after the epoch, past the upper end of the range.
    let after = Duration::from_hours(70_389_528);
    // 16064 years before the epoch, past the lower end as well.
    let before = Duration::from_hours(140_779_056);

    // The representable range ends at -9999-01-02T01:59:59Z and at
    // 9999-12-30T22:00:00.999999999Z. Only the upper end has a four-digit year
    // spelling the date-time format accepts, so both ends are built from epoch
    // seconds here.
    let latest = Timestamp::from_unix_nanos(253_402_207_200_999_999_999).unwrap();
    let earliest = Timestamp::from_unix_seconds(-377_705_023_201).unwrap();

    // How far back a `SystemTime` reaches depends on the platform: it starts at
    // 1601-01-01 on Windows and goes far below the range elsewhere. The lower end
    // therefore runs only where the platform can express an instant before it,
    // and every case is counted, so a platform that skips it cannot pass in
    // silence.
    let mut checked = 0_u32;
    let mut skipped = 0_u32;

    // The upper end is representable everywhere: it saturates to the last
    // representable instant, and converts back unchanged.
    assert_eq!(Timestamp::from(SystemTime::UNIX_EPOCH + after), latest);
    let round_trip = SystemTime::try_from(latest.clone()).expect("the last representable instant fits a system time");
    assert_eq!(Timestamp::from(round_trip), latest);
    checked += 1;

    // 1601-01-01 is 11_644_473_601 seconds before the epoch, so a platform that
    // can subtract those can also express an instant below the range.
    let covers_before_1601 = SystemTime::UNIX_EPOCH
        .checked_sub(Duration::from_secs(11_644_473_601))
        .is_some();

    if let Some(system_time) = SystemTime::UNIX_EPOCH.checked_sub(before) {
        // Saturates to the first representable instant.
        assert_eq!(Timestamp::from(system_time), earliest);
        checked += 1;
    } else {
        // Failing the larger subtraction implies failing the smaller one.
        assert!(!covers_before_1601, "a clock reaching 1601-01-01 can subtract 16064 years");
        skipped += 1;
    }

    // Converting the first instant back is platform dependent as well: where the
    // clock covers it the instant comes back unchanged, and where it cannot the
    // conversion reports the range error instead of panicking. Only the second
    // arm runs on a clock that starts after the instant, so it is confirmed by
    // the Windows job of the cross platform test run.
    if covers_before_1601 {
        let round_trip = SystemTime::try_from(earliest.clone()).expect("a clock covering the instant accepts it");
        assert_eq!(Timestamp::from(round_trip), earliest);
    } else {
        assert_eq!(SystemTime::try_from(earliest.clone()), Err(ConvertTimestampError::OutOfRange));
    }
    checked += 1;

    // The crate side of both ends does not depend on the platform clock: the
    // boundary instants are reachable through this crate's own conversions.
    assert_eq!(Timestamp::from_unix_seconds(earliest.unix_seconds()).unwrap(), earliest);
    assert_eq!(Timestamp::from_unix_nanos(latest.unix_nanos()).unwrap(), latest);
    checked += 1;

    assert!(checked > 0, "no case of the truncated clock boundary was checked");
    if skipped > 0 {
        println!("skipped {skipped} case(s): this platform's clock does not reach below the range");
    }
}
