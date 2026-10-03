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

use harness::api::{self, Oracle};

/// The fixture cases, or a panic that says what is wrong with the file.
fn fixture_cases() -> Vec<harness::support::Case> {
    harness::support::read_fixture().unwrap_or_else(|error| panic!("the frozen fixture is not usable: {error}"))
}

#[test]
fn the_properties_hold_for_the_oracle() {
    let cases = fixture_cases();
    let instants = api::fixture_instants(&cases);
    assert!(!instants.is_empty(), "the fixture must pin instants");

    let failures = api::property_failures::<Oracle>(&cases);
    assert!(
        failures.is_empty(),
        "{} properties fail on {} instants:\n{}",
        failures.len(),
        instants.len(),
        failures.join("\n")
    );
    println!("the oracle satisfies the property groups on {} instants", instants.len());
}

#[test]
fn the_properties_hold_for_the_candidate() {
    let cases = fixture_cases();
    if !api::CANDIDATE_AVAILABLE {
        println!("the candidate side is not compiled in: the property run is pending the implementation");
        return;
    }

    let instants = api::fixture_instants(&cases);
    let failures = api::candidate_property_failures(&cases);
    assert!(
        failures.is_empty(),
        "{} properties fail on {} instants:\n{}",
        failures.len(),
        instants.len(),
        failures.join("\n")
    );
    println!("the candidate satisfies the property groups on {} instants", instants.len());
}
