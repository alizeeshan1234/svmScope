//! Online, validator-gated regression tests for the full build → sign → submit →
//! land → capture → replay → mutate → time-travel → freeze pipeline against the
//! real deployed `svmscope_crate` Anchor program.
//!
//! Gated behind `#[ignore]`; run with a validator that has the program loaded:
//!
//! ```bash
//! solana-test-validator --reset \
//!   --bpf-program 41NQKxFZ2eEAFPSKyP2BEvw2bkxwXjC8Wjrxcy6njF8u \
//!   ../target/deploy/svmscope_crate.so --quiet &
//! cargo test --test localnet -- --ignored --test-threads=1
//! ```

use std::str::FromStr;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use solana_address::Address;
use solana_keypair::Keypair;
use solana_signer::Signer;
use svmscope::{Check, Cmp, Mutation, ProgramClient, Replay, Scope};

const RPC_URL: &str = "http://127.0.0.1:8899";
const PROGRAM_ID: &str = "41NQKxFZ2eEAFPSKyP2BEvw2bkxwXjC8Wjrxcy6njF8u";
const IDL_JSON: &str = include_str!("../../target/idl/svmscope_crate.json");

const DAY: i64 = 86_400;
const VESTING_AMOUNT: u64 = 50_000_000;
const START_TS_OFFSET: usize = 8 + 32 + 32 + 8;
const CLIFF_TS_OFFSET: usize = START_TS_OFFSET + 8;

fn scope() -> Scope {
    Scope::new(RPC_URL)
}

fn idl() -> serde_json::Value {
    serde_json::from_str(IDL_JSON).expect("program IDL parses")
}

fn wait_for_balance(scope: &Scope, address: &Address, minimum: u64) {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if let Ok(balance) = scope.client().get_balance(address) {
            if balance >= minimum {
                return;
            }
        }
        if Instant::now() >= deadline {
            panic!("timed out waiting for airdrop to {address}");
        }
        thread::sleep(Duration::from_millis(100));
    }
}

fn funded(scope: &Scope, lamports: u64) -> Keypair {
    let kp = Keypair::new();
    scope
        .client()
        .request_airdrop(&kp.pubkey(), lamports)
        .expect("airdrop request");
    wait_for_balance(scope, &kp.pubkey(), lamports);
    kp
}

fn program(scope: &Scope) -> ProgramClient<'_> {
    scope.program_with_idl(Address::from_str(PROGRAM_ID).unwrap(), idl())
}

/// A successful transaction: build it from the IDL, submit it, and confirm the
/// captured replay reproduces the on-chain success locally.
#[test]
#[ignore = "requires solana-test-validator with the program loaded"]
fn counter_increment_captures_and_replays() {
    let scope = scope();
    let program = program(&scope);
    let payer = funded(&scope, 1_000_000_000);
    let (counter, _) = Address::find_program_address(&[b"counter"], &program.program_id());

    if scope.client().get_account(&counter).is_err() {
        let init = program
            .method("initialize_counter")
            .unwrap()
            .payer(&payer)
            .account_signer("signer", &payer)
            .account("counter", counter)
            .send_and_capture()
            .expect("initialize lands");
        assert!(init.replay.recorded().is_some_and(|r| r.success));
    }

    let captured = program
        .method("increment_counter")
        .unwrap()
        .payer(&payer)
        .account_signer("signer", &payer)
        .account("counter", counter)
        .send_and_capture()
        .expect("increment lands");

    // The on-chain outcome was captured...
    assert!(
        captured.replay.recorded().is_some_and(|r| r.success),
        "recorded on-chain outcome should be success"
    );
    // ...and the local replay reproduces it from the captured pre-state.
    let replayed = captured.replay.run().expect("replay runs");
    assert!(replayed.result.success, "local replay: {:?}", replayed.result.error);
    // The counter account changed in the replay diffs.
    assert!(
        replayed.diffs.iter().any(|d| d.address == counter.to_string()),
        "counter should appear in replay diffs"
    );
    // Non-empty signature.
    assert!(!captured.signature.is_empty());
}

/// A deliberate program revert (claim before the cliff) must still LAND, be
/// captured as data, be explained from the IDL, and then be unlocked two
/// different ways — a state mutation and a clock warp — plus frozen for offline.
#[test]
#[ignore = "requires solana-test-validator with the program loaded"]
fn vesting_revert_explain_mutate_timetravel_and_freeze() {
    let scope = scope();
    let program = program(&scope);
    let creator = funded(&scope, 2_000_000_000);
    let beneficiary = funded(&scope, 100_000_000);
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs() as i64;
    let schedule_id = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos() as u64;
    let (schedule, _) = Address::find_program_address(
        &[b"vesting", beneficiary.pubkey().as_ref(), &schedule_id.to_le_bytes()],
        &program.program_id(),
    );

    // Create the escrow.
    let created = program
        .method("create_vesting")
        .unwrap()
        .payer(&creator)
        .account_signer("creator", &creator)
        .account("beneficiary", beneficiary.pubkey())
        .account("schedule", schedule)
        .arg("schedule_id", schedule_id)
        .arg("amount", VESTING_AMOUNT)
        .arg("start_ts", now)
        .arg("cliff_ts", now + 7 * DAY)
        .arg("end_ts", now + 30 * DAY)
        .send_and_capture()
        .expect("create_vesting lands");
    assert!(created.replay.recorded().is_some_and(|r| r.success));

    // Claim before the cliff — an on-chain revert, captured (not an Err).
    let mut pre_cliff = program
        .method("claim_vested")
        .unwrap()
        .payer(&beneficiary)
        .account_signer("beneficiary", &beneficiary)
        .account("schedule", schedule)
        .arg("schedule_id", schedule_id)
        .send_and_capture()
        .expect("a reverting claim still lands as a captured transaction");
    assert!(
        !pre_cliff.replay.recorded().expect("metadata").success,
        "pre-cliff claim should have reverted on-chain"
    );

    // The IDL explains the custom error by name.
    let baseline = pre_cliff.replay.run().unwrap();
    assert!(!baseline.result.success);
    let explain = baseline.explain.as_ref().expect("IDL explains the revert");
    assert_eq!(explain.title, "CliffNotReached");

    // Unlock #1 — mutate the schedule's timestamps into the past.
    let mutated = pre_cliff
        .replay
        .verify(
            "past timestamps unlock the claim",
            &[
                Mutation::patch(schedule.to_string(), START_TS_OFFSET, (now - 10 * DAY).to_le_bytes().to_vec()),
                Mutation::patch(schedule.to_string(), CLIFF_TS_OFFSET, (now - 9 * DAY).to_le_bytes().to_vec()),
            ],
            &[
                Check::success(),
                Check::log_contains("Beneficiary claimed"),
                Check::account(schedule.to_string()).field("claimed_amount", Cmp::gt(0)).build(),
            ],
        )
        .unwrap();
    assert!(mutated.pass, "mutation asserts: {:?}", mutated.asserts);

    // Unlock #2 — leave data untouched, warp the clock past the end.
    pre_cliff.replay.advance_seconds(31 * DAY);
    let future = pre_cliff
        .replay
        .verify(
            "warping past vesting end unlocks the full amount",
            &[],
            &[
                Check::success(),
                Check::account(schedule.to_string()).field("claimed_amount", Cmp::eq(VESTING_AMOUNT)).build(),
            ],
        )
        .unwrap();
    assert!(future.pass, "future asserts: {:?}", future.asserts);

    // Freeze and replay fully offline — fidelity to the recorded revert, then a
    // fresh warp with no validator. The recorded outcome is a revert, and
    // `matches_onchain` alone holds for it (it carries its own outcome
    // expectation, so it isn't sabotaged by the implicit "expect success").
    let fixture = pre_cliff.replay.to_fixture().unwrap();
    let offline = Replay::from_fixture(&fixture).unwrap();
    assert!(offline
        .verify("offline matches the landed revert", &[], &[Check::matches_onchain()])
        .unwrap()
        .pass);

    let mut offline_future = Replay::from_fixture(&fixture).unwrap();
    offline_future.advance_seconds(31 * DAY);
    assert!(offline_future
        .verify("offline warp unlocks the claim", &[], &[Check::success()])
        .unwrap()
        .pass);
}

/// The IDL builder against a program id that isn't deployed: the instruction
/// fails inside the runtime, but it still lands with a signature, metadata, and a
/// locally replayable failure — proving failures are data, not infrastructure errors.
#[test]
#[ignore = "requires solana-test-validator"]
fn missing_program_call_lands_as_a_captured_failure() {
    let scope = scope();
    let payer = funded(&scope, 1_000_000_000);
    let missing = Keypair::new().pubkey();
    let idl = serde_json::json!({
        "instructions": [{
            "name": "alwaysFails",
            "discriminator": [1, 2, 3, 4, 5, 6, 7, 8],
            "accounts": [{ "name": "authority", "writable": true, "signer": true }],
            "args": [{ "name": "value", "type": "u64" }]
        }]
    });
    let captured = scope
        .program_with_idl(missing, idl)
        .method("alwaysFails")
        .unwrap()
        .payer(&payer)
        .account_signer("authority", &payer)
        .arg("value", 42_u64)
        .send_and_capture()
        .expect("even a runtime failure returns a captured transaction");

    assert!(!captured.signature.is_empty());
    assert!(!captured.replay.recorded().expect("metadata").success);
    assert!(!captured.replay.run().unwrap().result.success);
}
