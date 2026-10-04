// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

//! The properties that an implementation of the three wire formats must satisfy.
//!
//! The corpus is every instant that the frozen fixture pins down. The properties are
//! the canonical view round trips, the exact formats round trip, formatting is
//! idempotent, and the ordering and the hash agree with the instant order. The
//! property bodies live in the harness, because the same run applies to the oracle and
//! to this crate's own implementation; the oracle run is the positive control of the
//! harness itself.

mod harness;

use std::time::{Duration, SystemTime};

use harness::api::{self, Oracle};

/// The fixture cases, or a panic that says what is wrong with the file.
fn fixture_cases() -> Vec<harness::support::Case> {
    harness::support::read_fixture().unwrap_or_else(|error| panic!("the frozen fixture is not usable: {error}"))
}

#[test]
fn the_properties_hold_for_the_oracle() {
    let cases = fixture_cases();
    let (checked, skipped) = api::property_coverage::<Oracle>(&cases);
    // The property run must cover every instant the platform can represent, so the
    // expectation is an equality whose floor the platform itself gives: a probe on the
    // standard library says whether it reaches before 1601, the earliest instant a
    // Windows `SystemTime` can hold. The number of instants is pinned as well,
    // because a floor computed from the same fixture would shrink with it.
    let instants = api::fixture_instants(&cases);
    let total = instants.len();
    assert_eq!(total, 274, "the frozen fixture must pin the instants the property run covers");
    // The numbers of each platform branch are pinned from the audit of the corpus rather
    // than recomputed from the rule under test, so a rule that quietly widens cannot move
    // its own bar: the Unix clock covers all 274 instants, the Windows clock 123 of them.
    let model = harness::support::platform_model();
    let (expected_checked, expected_skipped) = match (model.tick_nanos, model.floor_seconds) {
        (1, i64::MIN) => (274, 0),
        (100, -11_644_473_600) => (123, 151),
        (tick, floor) => {
            panic!("the platform model (tick={tick}, floor={floor}) has no pinned coverage yet")
        }
    };
    assert_eq!(
        (checked, skipped),
        (expected_checked, expected_skipped),
        "the oracle must cover the pinned instants of the platform branch"
    );
    assert_eq!(checked + skipped, total, "every instant is either checked or skipped");

    let failures = api::property_failures::<Oracle>(&cases);
    assert!(
        failures.is_empty(),
        "{} properties fail on {checked} instants:\n{}",
        failures.len(),
        failures.join("\n")
    );
    println!("the oracle satisfies the property groups on {checked} instants ({skipped} skipped)");
}

#[test]
fn the_properties_hold_for_the_candidate() {
    let cases = fixture_cases();
    if !api::CANDIDATE_AVAILABLE {
        println!("the candidate side is not compiled in: the property run is pending the implementation");
        return;
    }

    let (checked, skipped) = api::candidate_property_coverage(&cases);
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

/// A synthetic corpus for the tick control: four format cases, two of whose fractions a
/// 100-nanosecond clock keeps, and one parse case that never crosses the bridge. The
/// expectations come from the candidate, which is lossless, so the control measures the
/// division of the run and not the bytes of a format.
fn tick_control_cases() -> Vec<harness::support::Case> {
    let mut cases = Vec::new();
    for (secs, nanos) in [(100_i64, 123_456_700_u32), (0, 0), (0, 1), (0, 999_999_999)] {
        let mut case = harness::support::Case {
            direction: harness::support::Direction::Format,
            format: harness::support::Format::DateTime,
            input: harness::support::encode_instant(secs, nanos),
            expected: harness::support::Expected::Error("pending".to_owned()),
            intent: "synthetic/tick".to_owned(),
        };
        case.expected = harness::api::run_case::<harness::api::Candidate>(&case)
            .expect("the candidate represents the control instants")
            .expected();
        cases.push(case);
    }

    let mut parsed = harness::support::Case {
        direction: harness::support::Direction::Parse,
        format: harness::support::Format::DateTime,
        input: "1985-04-12T23:20:50Z".to_owned(),
        expected: harness::support::Expected::Error("pending".to_owned()),
        intent: "synthetic/tick".to_owned(),
    };
    parsed.expected = harness::api::run_case::<harness::api::Candidate>(&parsed)
        .expect("the candidate represents the control instants")
        .expected();
    cases.push(parsed);
    cases
}

#[test]
fn the_bridge_division_holds_at_a_forced_tick() {
    // The corpus keeps the instants of the fixture; only the oracle side, which crosses the
    // platform clock, leaves out the fractions the clock cannot carry. The clock of the run
    // decides the division, and the numbers of each branch are pinned from the audit rather
    // than recomputed from the rule under test, so the Windows branch runs on Linux through
    // the injected model of the environment.
    let cases = tick_control_cases();
    assert_eq!(cases.len(), 5, "the control corpus is four format cases and one parse case");
    let model = harness::support::platform_model();
    let (expected_checked, expected_skipped) = match (model.tick_nanos, model.floor_seconds) {
        (1, i64::MIN) => (5, 0),
        (100, -11_644_473_600) => (3, 2),
        (tick, floor) => panic!("the platform model (tick={tick}, floor={floor}) has no pinned division yet"),
    };

    let (coverage, failures) = harness::api::oracle_replay(&cases, model);
    assert!(failures.is_empty(), "the oracle must reproduce the corpus:\n{}", failures.join("\n"));
    assert_eq!(
        (coverage.checked, coverage.skipped),
        (expected_checked, expected_skipped),
        "the division follows the clock of the run"
    );
    assert_eq!(coverage.checked + coverage.skipped, cases.len(), "every case is counted");

    let report = harness::api::candidate_differential(&cases, model);
    assert_eq!(report.candidate_checked, cases.len(), "the candidate is lossless and must run every case");
    assert_eq!(
        (report.oracle_checked, report.oracle_skipped),
        (expected_checked, expected_skipped),
        "the differential checks the oracle only on the cases the clock carries"
    );
    assert_eq!(report.oracle_checked + report.oracle_skipped, cases.len(), "every case is counted");
    // The staleness check of the difference table belongs to the frozen corpus, so the
    // control asserts the verdict of each synthetic case instead.
    for case in &cases {
        match harness::api::compare_case(case, model) {
            Some(detail) => assert!(
                detail.is_empty(),
                "{}: the carried case must agree across the three sides:\n{}",
                case.id(),
                detail.join("\n")
            ),
            None => assert!(
                !harness::api::case_is_carried(case, model),
                "{}: a carried case must be compared",
                case.id()
            ),
        }
    }
    println!(
        "the division on {:?}: the oracle checked {} and skipped {} of {} cases; the candidate checked {}",
        (model.tick_nanos, model.floor_seconds),
        coverage.checked,
        coverage.skipped,
        cases.len(),
        report.candidate_checked
    );
}

#[test]
fn the_format_direction_reports_an_uncarried_instant() {
    // The format direction crosses the platform bridge, so the control pins that a case
    // the clock cannot carry is reported as skipped on any platform instead of panicking:
    // at a forced 100-nanosecond tick the lossy cases are left out of the oracle side and
    // counted, and at a nanosecond tick every control case runs.
    let cases: Vec<_> = tick_control_cases()
        .into_iter()
        .filter(|case| case.direction == harness::support::Direction::Format)
        .collect();
    assert_eq!(cases.len(), 4, "the control corpus carries four format cases");
    let model = harness::support::platform_model();
    let (expected_checked, expected_skipped) = match (model.tick_nanos, model.floor_seconds) {
        (1, i64::MIN) => (4, 0),
        (100, -11_644_473_600) => (2, 2),
        (tick, floor) => panic!("the platform model (tick={tick}, floor={floor}) has no pinned format division yet"),
    };

    for case in &cases {
        let (single, failures) = harness::api::oracle_replay(std::slice::from_ref(case), model);
        assert!(failures.is_empty(), "{}: the control case must not fail", case.id());
        assert_eq!(single.checked + single.skipped, 1, "{}: the control case is counted", case.id());
        assert_eq!(
            single.skipped,
            usize::from(!harness::api::case_is_carried(case, model)),
            "{}: the tick decides whether the case is carried",
            case.id()
        );
    }

    let (at_platform, failures) = harness::api::oracle_replay(&cases, model);
    assert!(failures.is_empty(), "the carried control cases must not fail:\n{}", failures.join("\n"));
    assert_eq!(
        (at_platform.checked, at_platform.skipped),
        (expected_checked, expected_skipped),
        "the format control follows the clock of the run"
    );

    // A format case the implementation cannot represent is reported, not panicked: the
    // largest expressible instant plus a second is outside the range of this crate, so the
    // candidate refuses it while the replay machinery carries on.
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
    let model = harness::support::platform_model();
    let report = harness::api::candidate_differential(std::slice::from_ref(&past_the_end), model);
    assert_eq!(
        report.candidate_checked, 0,
        "a case the candidate cannot represent is not counted as checked"
    );
    assert!(
        report
            .problems
            .iter()
            .any(|line| line.contains("the candidate cannot represent a case of the corpus")),
        "the refusal must be reported: {:?}",
        report.problems
    );
    println!(
        "the format direction on {:?}: {} of {} control cases are checked and {} are skipped; a case past the end of the range is refused",
        (model.tick_nanos, model.floor_seconds),
        at_platform.checked,
        cases.len(),
        at_platform.skipped
    );
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
