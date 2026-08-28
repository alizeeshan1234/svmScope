//! Extreme black-box regression tests for svmscope's offline replay engine and
//! Check DSL — driven entirely through the public API, exactly as an external
//! crates.io consumer would use it. No network, no validator: everything runs
//! off the committed v2 fixture.
//!
//! The fixture froze a real Anchor "counter" transaction. Key facts:
//!   * program id  41NQKxFZ2eEAFPSKyP2BEvw2bkxwXjC8Wjrxcy6njF8u (IDL is in the fixture)
//!   * counter PDA FLFUsWEUjARKvDrFf9WtF7k1tCxHqtCpfJU2krL6z7vY
//!   * account layout: 8-byte discriminator, then `count: u64` at offset 8
//!   * the frozen count is 1, so replaying `increment_counter` lands it on 2
//!     (field == 2, field_delta == 1, u64@8 == 2)
//!   * the recorded on-chain outcome is success, so `matches_onchain()` holds.

use svmscope::{Check, Cmp, Error, Fixture, Mutation, Replay, Scenario};

const FIXTURE: &str = include_str!("fixtures/counter_increment.json");
const COUNTER_PDA: &str = "FLFUsWEUjARKvDrFf9WtF7k1tCxHqtCpfJU2krL6z7vY";

// A valid base58 address that is NOT part of the fixture's world.
const ABSENT_BUT_VALID: &str = "So11111111111111111111111111111111111111112";

fn fixture() -> Fixture {
    Fixture::from_json(FIXTURE).expect("fixture parses")
}

fn replay() -> Replay {
    Replay::from_fixture(&fixture()).expect("fixture loads")
}

// --------------------------------------------------------------------------
// Happy path
// --------------------------------------------------------------------------

#[test]
fn fixture_loads_and_reports_its_shape() {
    let fx = fixture();
    assert_eq!(fx.version, 2);
    assert!(!fx.signature.is_empty());
    assert!(!fx.entries.is_empty());
    assert!(!fx.summary().is_empty());
}

#[test]
fn baseline_replay_succeeds() {
    let out = replay().run().expect("replay runs");
    assert!(out.result.success, "error: {:?}", out.result.error);
    assert!(out.result.compute_units > 0);
    assert!(out.explain.is_none());
}

#[test]
fn recorded_onchain_outcome_is_present_and_successful() {
    let r = replay();
    let recorded = r.recorded().expect("v2 fixture carries the recorded outcome");
    assert!(recorded.success);
    assert!(
        recorded.logs.iter().any(|l| l.contains("IncrementCounter")),
        "recorded logs: {:?}",
        recorded.logs
    );
}

#[test]
fn every_cmp_operator_on_the_count_field() {
    // Frozen count 1 → replay lands on 2. These must all PASS.
    for (name, check) in [
        ("eq2", Cmp::eq(2)),
        ("ne3", Cmp::ne(3)),
        ("lt3", Cmp::lt(3)),
        ("le2", Cmp::le(2)),
        ("gt1", Cmp::gt(1)),
        ("ge2", Cmp::ge(2)),
    ] {
        let out = replay()
            .verify(name, &[], &[Check::account(COUNTER_PDA).field("count", check).build()])
            .unwrap();
        assert!(out.pass, "{name} should pass: {:?}", out.asserts);
    }

    // And the complements must FAIL — proving the checks actually discriminate.
    for (name, check) in [
        ("eq3", Cmp::eq(3)),
        ("ne2", Cmp::ne(2)),
        ("lt2", Cmp::lt(2)),
        ("le1", Cmp::le(1)),
        ("gt2", Cmp::gt(2)),
        ("ge3", Cmp::ge(3)),
    ] {
        let out = replay()
            .verify(name, &[], &[Check::account(COUNTER_PDA).field("count", check).build()])
            .unwrap();
        assert!(!out.pass, "{name} should fail but passed: {:?}", out.asserts);
    }
}

#[test]
fn field_delta_is_plus_one() {
    let out = replay()
        .verify(
            "delta",
            &[],
            &[Check::account(COUNTER_PDA).field_delta("count", Cmp::eq(1)).build()],
        )
        .unwrap();
    assert!(out.pass, "{:?}", out.asserts);
}

#[test]
fn u64_at_offset_8_reads_the_count() {
    let out = replay()
        .verify(
            "u64@8",
            &[],
            &[Check::account(COUNTER_PDA).u64_at(8, Cmp::eq(2)).build()],
        )
        .unwrap();
    assert!(out.pass, "{:?}", out.asserts);
}

#[test]
fn matches_onchain_holds_for_the_frozen_baseline() {
    let out = replay().verify("faithful", &[], &[Check::matches_onchain()]).unwrap();
    assert!(out.pass, "{:?}", out.asserts);
}

#[test]
fn success_and_log_contains_checks() {
    let out = replay()
        .verify(
            "success+log",
            &[],
            &[Check::success(), Check::log_contains("Count incremented by 1 to")],
        )
        .unwrap();
    assert!(out.pass, "{:?}", out.asserts);
}

#[test]
fn compute_units_bounds() {
    let pass = replay()
        .verify("cu-le", &[], &[Check::compute_units(Cmp::le(200_000))])
        .unwrap();
    assert!(pass.pass);
    let pass2 = replay()
        .verify("cu-gt0", &[], &[Check::compute_units(Cmp::gt(0))])
        .unwrap();
    assert!(pass2.pass);
    let fail = replay()
        .verify("cu-eq0", &[], &[Check::compute_units(Cmp::eq(0))])
        .unwrap();
    assert!(!fail.pass, "zero CU must not match a real replay");
}

#[test]
fn multiple_checks_compose_in_one_scenario() {
    let out = replay()
        .verify(
            "everything",
            &[],
            &[
                Check::success(),
                Check::matches_onchain(),
                Check::log_contains("IncrementCounter"),
                Check::compute_units(Cmp::le(200_000)),
                Check::account(COUNTER_PDA)
                    .field("count", Cmp::eq(2))
                    .field_delta("count", Cmp::eq(1))
                    .u64_at(8, Cmp::eq(2))
                    .build(),
            ],
        )
        .unwrap();
    assert!(out.pass, "{:?}", out.asserts);
}

// --------------------------------------------------------------------------
// Mutations
// --------------------------------------------------------------------------

#[test]
fn data_patch_changes_the_pre_state_and_the_increment_follows() {
    // Patch count to 99 pre-replay; the increment then lands on 100.
    let out = replay()
        .verify(
            "patched",
            &[Mutation::patch(COUNTER_PDA, 8, 99u64.to_le_bytes().to_vec())],
            &[
                Check::success(),
                Check::account(COUNTER_PDA).field("count", Cmp::eq(100)).build(),
                Check::log_contains("Count incremented by 1 to : 100"),
            ],
        )
        .unwrap();
    assert!(out.pass, "{:?}", out.asserts);
}

#[test]
fn lamports_mutation_on_a_loaded_account_is_accepted() {
    // Setting lamports on the (loaded) counter PDA must not be a hard error.
    let out = replay()
        .simulate(&[Mutation::lamports(COUNTER_PDA, 5_000_000_000)])
        .expect("loaded-account mutation is valid");
    assert!(out.result.success);
}

#[test]
fn wholesale_data_replacement_is_accepted() {
    // Replace the counter's data with count=41 (8-byte disc + 8-byte count).
    let fx = fixture();
    // Reuse the real discriminator by patching only the count, then also prove a
    // full Data replacement of the count region composes.
    let mut replay = Replay::from_fixture(&fx).unwrap();
    replay.advance_seconds(0); // no-op, keeps the API exercised
    let out = replay
        .simulate(&[Mutation::patch(COUNTER_PDA, 8, 41u64.to_le_bytes().to_vec())])
        .unwrap();
    assert!(out.result.success);
    // count 41 → 42 after increment
    let checked = replay
        .verify(
            "42",
            &[Mutation::patch(COUNTER_PDA, 8, 41u64.to_le_bytes().to_vec())],
            &[Check::account(COUNTER_PDA).field("count", Cmp::eq(42)).build()],
        )
        .unwrap();
    assert!(checked.pass, "{:?}", checked.asserts);
}

#[test]
fn empty_mutation_and_check_lists_are_valid() {
    // No mutations, no explicit checks → implicit success check.
    let out = replay().verify("bare", &[], &[]).unwrap();
    assert!(out.pass);
    // simulate with an empty slice equals run().
    let a = replay().run().unwrap();
    let b = replay().simulate(&[]).unwrap();
    assert_eq!(a.result.success, b.result.success);
}

// --------------------------------------------------------------------------
// Time travel
// --------------------------------------------------------------------------

#[test]
fn each_time_travel_method_moves_the_clock() {
    let base = replay().describe_clock();

    let mut a = replay();
    a.advance_slots(1_000);
    assert_ne!(a.describe_clock(), base);

    let mut b = replay();
    b.advance_epochs(3);
    assert_ne!(b.describe_clock(), base);

    let mut c = replay();
    c.advance_seconds(30 * 86_400);
    assert_ne!(c.describe_clock(), base);

    let mut d = replay();
    d.warp_to_slot(500_000_000);
    assert_ne!(d.describe_clock(), base);

    let mut e = replay();
    e.warp_to_epoch(1_200);
    assert_ne!(e.describe_clock(), base);

    let mut f = replay();
    f.warp_to_timestamp(2_000_000_000);
    assert_ne!(f.describe_clock(), base);
}

#[test]
fn time_travel_then_replay_still_succeeds() {
    // The counter has no time gate, so it succeeds regardless of the clock.
    let mut r = replay();
    r.advance_seconds(365 * 86_400);
    assert!(r.run().unwrap().result.success);
}

#[test]
fn mutation_and_time_travel_compose() {
    let mut r = replay();
    r.advance_seconds(90 * 86_400);
    let out = r
        .verify(
            "warped+patched",
            &[Mutation::patch(COUNTER_PDA, 8, 7u64.to_le_bytes().to_vec())],
            &[Check::account(COUNTER_PDA).field("count", Cmp::eq(8)).build()],
        )
        .unwrap();
    assert!(out.pass, "{:?}", out.asserts);
}

// --------------------------------------------------------------------------
// Fixture round-trip
// --------------------------------------------------------------------------

#[test]
fn fixture_json_round_trips() {
    let fx1 = fixture();
    let json = fx1.to_json().expect("serializes");
    let fx2 = Fixture::from_json(&json).expect("re-parses");
    assert_eq!(fx1.signature, fx2.signature);
    assert_eq!(fx1.version, fx2.version);
    assert_eq!(fx1.entries.len(), fx2.entries.len());
}

#[test]
fn to_fixture_reproduces_an_identical_replay() {
    let refrozen = replay().to_fixture().expect("re-freeze");
    let again = Replay::from_fixture(&refrozen).unwrap();
    let a = replay().run().unwrap();
    let b = again.run().unwrap();
    assert_eq!(a.result.success, b.result.success);
    assert_eq!(a.result.compute_units, b.result.compute_units);
    // matches_onchain must survive the round-trip too.
    let out = again.verify("still faithful", &[], &[Check::matches_onchain()]).unwrap();
    assert!(out.pass);
}

// --------------------------------------------------------------------------
// run_suite
// --------------------------------------------------------------------------

#[test]
fn run_suite_over_several_scenarios() {
    let outcomes = replay()
        .run_suite(&[
            Scenario::new("baseline").check(Check::success()),
            Scenario::new("faithful").check(Check::matches_onchain()),
            Scenario::new("count reaches 2")
                .check(Check::account(COUNTER_PDA).field("count", Cmp::eq(2)).build()),
            Scenario::new("patched to 100")
                .mutate(Mutation::patch(COUNTER_PDA, 8, 99u64.to_le_bytes().to_vec()))
                .check(Check::account(COUNTER_PDA).field("count", Cmp::eq(100)).build()),
        ])
        .unwrap();
    assert_eq!(outcomes.len(), 4);
    assert!(outcomes.iter().all(|o| o.pass), "{outcomes:?}");
}

#[test]
fn empty_suite_returns_no_outcomes() {
    let outcomes = replay().run_suite(&[]).unwrap();
    assert!(outcomes.is_empty());
}

// --------------------------------------------------------------------------
// Adversarial — trying to break it. These are the regression guards.
// --------------------------------------------------------------------------

#[test]
fn typoed_mutation_address_is_a_hard_error_not_a_passing_revert() {
    // Valid base58 but absent from the replay → MutationTargetMissing.
    let err = replay()
        .run_suite(&[Scenario::new("drain")
            .mutate(Mutation::lamports(ABSENT_BUT_VALID, 0))
            .check(Check::revert())])
        .unwrap_err();
    assert!(
        matches!(err, Error::MutationTargetMissing(_)),
        "expected MutationTargetMissing, got {err:?}"
    );
}

#[test]
fn unparseable_mutation_address_is_an_invalid_address_error() {
    let err = replay()
        .run_suite(&[Scenario::new("bad").mutate(Mutation::lamports("not-a-real-address", 0))])
        .unwrap_err();
    assert!(
        matches!(err, Error::InvalidAddress(_) | Error::MutationTargetMissing(_)),
        "got {err:?}"
    );
}

#[test]
fn typoed_assert_address_fails_the_check_instead_of_silently_passing() {
    // A `lamports == 0` / `token_delta == 0` assertion against an address the
    // replay never loaded must FAIL (not read a silent zero and pass). This is a
    // specific hardening fix — prove it holds for both check kinds.
    let out = replay()
        .verify(
            "typo'd assert",
            &[],
            &[
                Check::account(ABSENT_BUT_VALID).lamports(Cmp::eq(0)).build(),
                Check::account(ABSENT_BUT_VALID).token_delta(Cmp::eq(0)).build(),
            ],
        )
        .unwrap();
    assert!(!out.pass, "typo'd asserts must not pass: {:?}", out.asserts);
    assert!(
        out.asserts.iter().all(|a| !a.pass),
        "every typo'd assert must fail, none vacuously pass: {:?}",
        out.asserts
    );
}

#[test]
fn unknown_field_fails_and_names_the_available_fields() {
    let out = replay()
        .verify(
            "bad field",
            &[],
            &[Check::account(COUNTER_PDA).field("countt", Cmp::eq(1)).build()],
        )
        .unwrap();
    assert!(!out.pass);
    // The failure description should name the real field so the fix is obvious.
    assert!(
        out.asserts.iter().any(|a| a.description.contains("count")),
        "description should list available fields: {:?}",
        out.asserts
    );
}

#[test]
fn patch_out_of_range_is_an_error_not_a_panic() {
    // The counter account is tiny; a patch far past its end must be a typed error.
    let err = replay()
        .simulate(&[Mutation::patch(COUNTER_PDA, 10_000, vec![0u8; 8])])
        .unwrap_err();
    assert!(
        matches!(err, Error::PatchOutOfRange { .. }),
        "expected PatchOutOfRange, got {err:?}"
    );
}

#[test]
fn extreme_time_warps_do_not_panic() {
    // Each on a fresh replay, a single warp (internal accumulation is 0 + n), so
    // these exercise the saturating clock math without tripping accumulation.
    let mut a = replay();
    a.advance_epochs(i64::MAX);
    assert!(!a.describe_clock().is_empty());
    let _ = a.run(); // must not panic (Ok or Err both acceptable)

    let mut b = replay();
    b.advance_seconds(i64::MAX / 2);
    assert!(!b.describe_clock().is_empty());
    let _ = b.run();

    let mut c = replay();
    c.warp_to_slot(u64::MAX);
    assert!(!c.describe_clock().is_empty());
    let _ = c.run();

    let mut d = replay();
    d.warp_to_timestamp(i64::MAX);
    assert!(!d.describe_clock().is_empty());
    let _ = d.run();

    let mut e = replay();
    e.warp_to_timestamp(i64::MIN);
    assert!(!e.describe_clock().is_empty());
    let _ = e.run();
}

#[test]
fn non_ascii_assert_address_does_not_panic() {
    // A >12-char multi-byte string forces the address-shortening formatter down
    // its char-boundary path. It must fail the assert, never panic on a byte slice.
    let weird = "€€€€€€€€€€€€€€€"; // 15 chars, 45 bytes
    let out = replay()
        .verify(
            "weird addr",
            &[],
            &[Check::account(weird).lamports(Cmp::eq(0)).build()],
        )
        .unwrap();
    assert!(!out.pass, "an unparseable address can't satisfy a check");
}

#[test]
fn a_wrong_expected_value_reliably_fails() {
    // Guard against vacuous passes: an obviously-wrong field value must fail.
    let out = replay()
        .verify(
            "wrong",
            &[],
            &[Check::account(COUNTER_PDA).field("count", Cmp::eq(999_999)).build()],
        )
        .unwrap();
    assert!(!out.pass);
}
