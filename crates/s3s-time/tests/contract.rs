// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

//! The properties that an implementation of the three wire formats must satisfy.
//!
//! The corpus is every instant that the frozen fixture pins down. The properties are
//! the canonical view round trips, the exact formats round trip, formatting is
//! idempotent, and the ordering and the hash agree with the instant order. The property
//! bodies live in the harness, and the replay beside this file is their positive
//! control. The remaining cases keep the platform arithmetic of the harness itself
//! checked: how far back the clock reaches, how it reads back, and how it aligns a
//! fraction to its tick.

mod harness;

use std::time::{Duration, SystemTime};

use harness::api;

/// The fixture cases, or a panic that says what is wrong with the file.
fn fixture_cases() -> Vec<harness::support::Case> {
    harness::support::read_fixture().unwrap_or_else(|error| panic!("the frozen fixture is not usable: {error}"))
}

#[test]
fn the_properties_hold_for_the_candidate() {
    let cases = fixture_cases();
    if !api::CANDIDATE_AVAILABLE {
        println!("the candidate side is not compiled in: the property run is pending the implementation");
        return;
    }

    // The number of instants is pinned, because a floor computed from the same fixture
    // would shrink with it. The candidate run is pinned as well, and pinned from the
    // audit of the corpus rather than recomputed from the rule under test, so a rule
    // that quietly narrows cannot move its own bar.
    //
    // Six of the instants are exactly the ones the swap deliberately rejects: they are
    // the expected values of the range divergences recorded in POST_MIGRATION_DIVERGENCES
    // (253402300799 and 253402207201 with and without a fraction, -377705116800 with and
    // without a fraction, and -377705023202). The pre-migration implementation held them;
    // this crate does not, and the replay beside this file reports each one.
    //
    // The count does not cross a platform clock, so it must be the same whether the
    // platform model is probed or injected: the injected run is what catches a candidate
    // that someone routes through the tick bridge.
    let instants = api::fixture_instants(&cases);
    let total = instants.len();
    assert_eq!(total, 274, "the frozen fixture must pin the instants the property run covers");
    let (checked, skipped) = api::candidate_property_coverage(&cases);
    assert_eq!(
        (checked, skipped),
        (268, 6),
        "the candidate must run every instant it can represent, and the six it cannot are the recorded range divergences"
    );
    assert_eq!(checked + skipped, total, "every instant is either checked or skipped");

    let failures = api::candidate_property_failures(&cases);
    assert!(
        failures.is_empty(),
        "{} properties fail on {checked} instants:\n{}",
        failures.len(),
        failures.join("\n")
    );
    println!("the candidate satisfies the property groups on {checked} instants ({skipped} outside its range)");
}

#[test]
fn the_platform_system_time_helper_is_checked() {
    assert_eq!(
        harness::support::checked_system_time(0, 0),
        Some(SystemTime::UNIX_EPOCH),
        "the epoch is representable on every platform"
    );
    assert!(
        harness::support::checked_system_time(253_402_207_200, 999_999_999).is_some(),
        "a far future instant is representable on every platform"
    );
    // The extreme is delegated to the platform: the helper must answer exactly what the
    // standard library answers, and it must answer at all instead of panicking. A Unix
    // `SystemTime` reaches i64::MIN seconds, a Windows one cannot reach before 1601.
    let extreme = SystemTime::UNIX_EPOCH.checked_sub(Duration::new(i64::MIN.unsigned_abs(), 0));
    assert_eq!(
        harness::support::checked_system_time(i64::MIN, 0),
        extreme,
        "an extreme instant answers like the platform helper instead of panicking"
    );
    println!("checked_system_time: the epoch and a far future instant are representable; the extreme matches std ({extreme:?})");
}

/// The platform answer for an instant, rebuilt from the total nanoseconds with the
/// standard library instead of restating the arithmetic of the helper under test.
fn expected_system_time(seconds: i64, nanoseconds: u32) -> Option<SystemTime> {
    let total = i128::from(seconds) * 1_000_000_000 + i128::from(nanoseconds);
    let magnitude = total.unsigned_abs();
    let whole = u64::try_from(magnitude / 1_000_000_000).ok()?;
    let part = u32::try_from(magnitude % 1_000_000_000).ok()?;
    let duration = Duration::new(whole, part);

    if total >= 0 {
        SystemTime::UNIX_EPOCH.checked_add(duration)
    } else {
        SystemTime::UNIX_EPOCH.checked_sub(duration)
    }
}

#[test]
fn the_platform_system_time_helper_matches_the_platform() {
    // Both directions of the epoch, the negative seconds that carry a fraction (the
    // branch where the normalization can be off by one), both sides of the earliest
    // instant a Windows `SystemTime` can hold (1601-01-01), and the extremes of
    // the internal representation. Each expectation is rebuilt from the total
    // nanoseconds, so a value that is off by a second or a nanosecond fails here on
    // any platform.
    let cases: [(i64, u32); 17] = [
        (0, 0),
        (0, 1),
        (0, 999_999_999),
        (1, 0),
        (-1, 0),
        (-1, 1),
        (-1, 999_999_999),
        (-2, 500_000_000),
        (-11_644_473_600, 0),
        (-11_644_473_600, 1),
        (-11_644_473_601, 0),
        (-62_167_219_201, 0),
        (-377_705_116_800, 0),
        (253_402_207_200, 999_999_999),
        (253_402_300_799, 999_999_999),
        (i64::MIN, 0),
        (i64::MIN, 1),
    ];

    for (seconds, nanoseconds) in cases {
        assert_eq!(
            harness::support::checked_system_time(seconds, nanoseconds),
            expected_system_time(seconds, nanoseconds),
            "checked_system_time({seconds}, {nanoseconds}) must equal the platform answer"
        );
    }
    println!("checked_system_time matches the platform answer on {} instants", cases.len());
}

/// One row of the tick table: the input instant, its alignment on a clock that counts
/// 100-nanosecond ticks, and its alignment on a clock that counts nanoseconds.
type TickCase = ((i64, u32), (i64, u32), (i64, u32));

#[test]
fn the_bridge_quantizes_to_the_platform_tick() {
    // The property run carries the corpus through a `SystemTime` on both sides, so the
    // instants are aligned to the tick of the platform clock: a Windows clock keeps
    // 100-nanosecond ticks and drops the rest. The Windows tick is asserted here on any
    // platform, and the platform probe is checked against the tick it reports.
    let cases: [TickCase; 6] = [
        ((0, 1), (0, 0), (0, 1)),
        ((100, 123_456_789), (100, 123_456_700), (100, 123_456_789)),
        ((0, 999_999_999), (0, 999_999_900), (0, 999_999_999)),
        ((-1, 1), (-1, 0), (-1, 1)),
        ((-1, 999_999_999), (-1, 999_999_900), (-1, 999_999_999)),
        ((-2, 500_000_000), (-2, 500_000_000), (-2, 500_000_000)),
    ];

    for ((secs, nanos), on_windows, on_unix) in cases {
        assert_eq!(
            harness::support::quantize_to_tick(secs, nanos, 100),
            on_windows,
            "({secs}, {nanos}) must align to the 100-nanosecond tick of a Windows clock"
        );
        assert_eq!(
            harness::support::quantize_to_tick(secs, nanos, 1),
            on_unix,
            "({secs}, {nanos}) must keep its nanosecond on a clock that counts nanoseconds"
        );
    }

    let tick = harness::support::platform_tick_nanos();
    assert!(
        matches!(tick, 1 | 100),
        "the platform clock ticks every nanosecond or every 100 nanoseconds, got {tick}"
    );
    let on_tick = harness::support::checked_system_time(0, tick).expect("the epoch is representable");
    assert_eq!(
        harness::support::read_back_system_time(on_tick),
        Some((0, tick)),
        "an instant on a tick boundary must survive the bridge"
    );
    let before_epoch = harness::support::checked_system_time(-1, 500_000_000).expect("the instant is representable");
    assert_eq!(
        harness::support::read_back_system_time(before_epoch),
        Some((-1, 500_000_000)),
        "an instant before the epoch reads back as its floor second and a positive adjustment"
    );
    println!("the bridge aligns to {tick} ns on this platform; a Windows clock (100 ns) drops the sub-tick part");
}

#[test]
fn the_candidate_reports_an_instant_it_cannot_represent() {
    // A format case the implementation cannot represent is reported, not panicked: the
    // largest expressible instant plus a second is outside the range of this crate, so
    // the candidate refuses it while the replay machinery carries on and reports the
    // refusal as a problem instead of failing the run with a panic.
    let past_the_end = harness::support::Case {
        direction: harness::support::Direction::Format,
        format: harness::support::Format::DateTime,
        input: harness::support::encode_instant(253_402_300_800, 0),
        expected: harness::support::Expected::Error("pending".to_owned()),
        intent: "synthetic/past-the-end".to_owned(),
    };
    assert!(
        harness::api::try_run::<harness::api::Candidate>(past_the_end.direction, past_the_end.format, &past_the_end.input)
            .is_none(),
        "an instant past the end of the range must be reported, not panicked"
    );
    let report = harness::api::candidate_replay(std::slice::from_ref(&past_the_end));
    assert_eq!(report.checked, 0, "a case the candidate cannot represent is not counted as checked");
    assert!(
        report
            .problems
            .iter()
            .any(|line| line.contains("the candidate cannot represent a case of the corpus")),
        "the refusal must be reported: {:?}",
        report.problems
    );
    println!("a format case past the end of the range is refused and reported, not panicked");
}

#[test]
fn the_platform_quantization_is_consistent() {
    // Holds on every platform: aligning an instant moves it back by less than one tick and
    // aligning it again changes nothing. How the real clock rounds a fraction below its tick
    // is a fact for the log rather than an assertion, because it can differ by platform and
    // is exactly what CI has to settle.
    let model = harness::support::platform_model();
    let cases: [(i64, u32); 8] = [
        (0, 0),
        (0, 1),
        (0, 999_999_999),
        (100, 123_456_789),
        (-1, 1),
        (-1, 999_999_999),
        (-2, 500_000_000),
        (-11_644_473_601, 999_999_999),
    ];

    for (secs, nanos) in cases {
        let (aligned_secs, aligned_nanos) = model.quantize(secs, nanos);
        let moved = i128::from(secs) * 1_000_000_000 + i128::from(nanos)
            - (i128::from(aligned_secs) * 1_000_000_000 + i128::from(aligned_nanos));
        assert!(moved >= 0, "({secs}, {nanos}) is aligned forward to ({aligned_secs}, {aligned_nanos})");
        assert!(
            moved < i128::from(model.tick_nanos),
            "({secs}, {nanos}) moves by {moved} ns, which is a whole tick or more"
        );
        assert_eq!(
            model.quantize(aligned_secs, aligned_nanos),
            (aligned_secs, aligned_nanos),
            "the alignment of ({secs}, {nanos}) is idempotent"
        );
    }

    let read_back = harness::support::checked_system_time(0, 150).and_then(harness::support::read_back_system_time);
    println!(
        "platform model: tick={} floor={} (injected: {}); the clock reads (0, 150) back as {:?}",
        model.tick_nanos, model.floor_seconds, model.injected, read_back
    );
}
