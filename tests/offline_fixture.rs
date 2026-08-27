//! Integration test: the crate exactly as a consumer uses it, fully offline.
//!
//! The committed fixture froze a real devnet transaction (an Anchor counter
//! program's `increment_counter`) with its accounts, program ELF, on-chain
//! IDL, and recorded outcome. Everything below runs with zero RPC.

use svmscope::{Check, Cmp, Fixture, Mutation, Replay, Scenario};

const FIXTURE: &str = include_str!("fixtures/counter_increment.json");
const COUNTER_PDA: &str = "FLFUsWEUjARKvDrFf9WtF7k1tCxHqtCpfJU2krL6z7vY";

fn replay() -> Replay {
    Replay::from_fixture(&Fixture::from_json(FIXTURE).expect("fixture parses"))
        .expect("fixture loads")
}

#[test]
fn frozen_replay_matches_the_onchain_outcome() {
    let out = replay()
        .verify("faithful", &[], &[Check::matches_onchain()])
        .unwrap();
    assert!(out.pass, "{out:?}");
}

#[test]
fn named_field_asserts_resolve_offline_via_the_captured_idl() {
    // The fixture froze the counter at 1 (state at capture time, not at the
    // tx's original slot — replays run against reconstructed current state),
    // so the increment lands on 2. The delta is the robust assertion.
    let out = replay()
        .verify(
            "count increments",
            &[],
            &[Check::account(COUNTER_PDA)
                .field("count", Cmp::eq(2))
                .field_delta("count", Cmp::eq(1))
                .build()],
        )
        .unwrap();
    assert!(out.pass, "{out:?}");
}

#[test]
fn mutations_and_the_check_dsl_compose() {
    // Patch count to 99 pre-replay; the increment lands on 100.
    let out = replay()
        .verify(
            "patched counter reaches 100",
            &[Mutation::patch(COUNTER_PDA, 8, 99u64.to_le_bytes().to_vec())],
            &[
                Check::success(),
                Check::log_contains("incremented by 1 to : 100"),
                Check::account(COUNTER_PDA).field("count", Cmp::eq(100)).build(),
                Check::compute_units(Cmp::le(200_000)),
            ],
        )
        .unwrap();
    assert!(out.pass, "{out:?}");
}

#[test]
fn time_travel_warps_the_clock() {
    let mut replay = replay();
    let before = replay.describe_clock();
    replay.advance_seconds(30 * 86_400);
    let after = replay.describe_clock();
    assert_ne!(before, after);
    // The counter has no time gate — it still succeeds in the future.
    assert!(replay.run().unwrap().result.success);
}

#[test]
fn a_typoed_mutation_address_is_a_hard_error_not_a_passing_revert() {
    let typo = "7NypoTypoTypoTypoTypoTypoTypoTypoTypoTypoTyp";
    let err = replay()
        .run_suite(&[Scenario::new("drain")
            .mutate(Mutation::lamports(typo, 0))
            .check(Check::revert())])
        .unwrap_err();
    assert!(matches!(err, svmscope::Error::InvalidAddress(_) | svmscope::Error::MutationTargetMissing(_)), "{err}");
}

#[test]
fn unknown_fields_error_with_the_available_names()  {
    let out = replay()
        .verify(
            "bad field name",
            &[],
            &[Check::account(COUNTER_PDA).field("countt", Cmp::eq(1)).build()],
        )
        .unwrap();
    // A failed field resolution is a failed assertion whose description names
    // the available fields, so the fix is right in the test output.
    assert!(!out.pass);
    assert!(out.asserts[0].description.contains("count"), "{out:?}");
}
