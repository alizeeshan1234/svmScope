//! Complete svmscope reference consumer.
//!
//! The counter is the smallest successful-transaction example. The vesting
//! flow is the realistic example: create an escrow, deliberately land a claim
//! before its cliff, explain the revert, mutate its state, time-travel the exact
//! same transaction until it succeeds, and freeze it for deterministic CI.

use std::str::FromStr;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use solana_address::Address;
use solana_keypair::Keypair;
use solana_signer::Signer;
use svmscope::{Check, Cmp, Mutation, ProgramClient, Replay, Scope};

const DEFAULT_RPC_URL: &str = "http://127.0.0.1:8899";
const PROGRAM_ID: &str = "41NQKxFZ2eEAFPSKyP2BEvw2bkxwXjC8Wjrxcy6njF8u";
const IDL_JSON: &str = include_str!("../../target/idl/svmscope_crate.json");

const DAY: i64 = 86_400;
const VESTING_AMOUNT: u64 = 50_000_000;

// VestingSchedule layout: 8-byte discriminator, creator, beneficiary,
// schedule_id, then start_ts and cliff_ts.
const START_TS_OFFSET: usize = 8 + 32 + 32 + 8;
const CLIFF_TS_OFFSET: usize = START_TS_OFFSET + 8;

fn wait_for_balance(
    scope: &Scope,
    address: &Address,
    minimum: u64,
) -> Result<(), Box<dyn std::error::Error>> {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let balance = scope.client().get_balance(address)?;
        if balance >= minimum {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(format!("timed out waiting for the airdrop to {address}").into());
        }
        thread::sleep(Duration::from_millis(100));
    }
}

fn funded_keypair(scope: &Scope, lamports: u64) -> Result<Keypair, Box<dyn std::error::Error>> {
    let keypair = Keypair::new();
    scope
        .client()
        .request_airdrop(&keypair.pubkey(), lamports)?;
    wait_for_balance(scope, &keypair.pubkey(), lamports)?;
    Ok(keypair)
}

fn counter_demo(
    scope: &Scope,
    program: &ProgramClient<'_>,
) -> Result<(), Box<dyn std::error::Error>> {
    println!("\n== counter: successful transaction capture ==");
    let payer = funded_keypair(scope, 1_000_000_000)?;
    let (counter, _) = Address::find_program_address(&[b"counter"], &program.program_id());

    if scope.client().get_account(&counter).is_err() {
        let initialized = program
            .method("initialize_counter")?
            .payer(&payer)
            .account_signer("signer", &payer)
            .account("counter", counter)
            .send_and_capture()?;
        assert!(initialized
            .replay
            .recorded()
            .is_some_and(|record| record.success));
        println!("initialize: {}", initialized.signature);
    }

    let captured = program
        .method("increment_counter")?
        .payer(&payer)
        .account_signer("signer", &payer)
        .account("counter", counter)
        .send_and_capture()?;
    assert!(captured
        .replay
        .recorded()
        .is_some_and(|record| record.success));
    assert!(captured.replay.run()?.result.success);
    println!("increment:  {}", captured.signature);
    println!("replay:     passed");
    Ok(())
}

fn vesting_demo(
    scope: &Scope,
    program: &ProgramClient<'_>,
) -> Result<(), Box<dyn std::error::Error>> {
    println!("\n== vesting: revert, mutation, time travel, fixture ==");
    let creator = funded_keypair(scope, 2_000_000_000)?;
    let beneficiary = funded_keypair(scope, 100_000_000)?;
    let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs() as i64;
    let schedule_id = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos() as u64;
    let start_ts = now;
    let cliff_ts = now + 7 * DAY;
    let end_ts = now + 30 * DAY;
    let schedule_id_bytes = schedule_id.to_le_bytes();
    let (schedule, _) = Address::find_program_address(
        &[
            b"vesting",
            beneficiary.pubkey().as_ref(),
            &schedule_id_bytes,
        ],
        &program.program_id(),
    );

    let created = program
        .method("create_vesting")?
        .payer(&creator)
        .account_signer("creator", &creator)
        .account("beneficiary", beneficiary.pubkey())
        .account("schedule", schedule)
        .arg("schedule_id", schedule_id)
        .arg("amount", VESTING_AMOUNT)
        .arg("start_ts", start_ts)
        .arg("cliff_ts", cliff_ts)
        .arg("end_ts", end_ts)
        .send_and_capture()?;
    assert!(created
        .replay
        .recorded()
        .is_some_and(|record| record.success));
    assert!(created.replay.run()?.result.success);
    println!("create:     {}", created.signature);

    // `skip_preflight` lets this expected program failure land. svmscope returns
    // Ok(CapturedTransaction); failure is an observed outcome, not infrastructure
    // failure.
    let mut pre_cliff = program
        .method("claim_vested")?
        .payer(&beneficiary)
        .account_signer("beneficiary", &beneficiary)
        .account("schedule", schedule)
        .arg("schedule_id", schedule_id)
        .send_and_capture()?;
    let recorded = pre_cliff
        .replay
        .recorded()
        .expect("landed claim should have transaction metadata");
    assert!(
        !recorded.success,
        "the pre-cliff claim unexpectedly succeeded"
    );
    println!(
        "claim:      {} (expected on-chain revert)",
        pre_cliff.signature
    );

    let baseline = pre_cliff.replay.run()?;
    assert!(!baseline.result.success);
    let explanation = baseline
        .explain
        .as_ref()
        .expect("the supplied IDL should explain Anchor error 6003");
    assert_eq!(explanation.title, "CliffNotReached");
    println!("explained:  {} — {}", explanation.title, explanation.detail);

    // Mutation path: make the captured schedule ten days old and move its cliff
    // nine days into the past. The same claim now succeeds without changing the
    // validator or waiting in real life.
    let mutated = pre_cliff.replay.verify(
        "moving schedule timestamps into the past unlocks a claim",
        &[
            Mutation::patch(
                schedule.to_string(),
                START_TS_OFFSET,
                (now - 10 * DAY).to_le_bytes().to_vec(),
            ),
            Mutation::patch(
                schedule.to_string(),
                CLIFF_TS_OFFSET,
                (now - 9 * DAY).to_le_bytes().to_vec(),
            ),
        ],
        &[
            Check::success(),
            Check::log_contains("Beneficiary claimed"),
            Check::account(schedule.to_string())
                .field("claimed_amount", Cmp::gt(0))
                .build(),
        ],
    )?;
    assert!(mutated.pass, "mutation assertions: {:?}", mutated.asserts);
    println!("mutation:   passed (past timestamps unlock the claim)");

    // Time-travel path: keep account data untouched and move only Clock beyond
    // the end of the schedule. The full amount becomes vested.
    pre_cliff.replay.advance_seconds(31 * DAY);
    let future = pre_cliff.replay.verify(
        "the same claim succeeds after the vesting end",
        &[],
        &[
            Check::success(),
            Check::account(schedule.to_string())
                .field("claimed_amount", Cmp::eq(VESTING_AMOUNT))
                .build(),
        ],
    )?;
    assert!(future.pass, "future assertions: {:?}", future.asserts);
    println!(
        "time travel: passed ({})",
        pre_cliff.replay.describe_clock()
    );

    // Fixtures retain the original recorded revert and IDL. Offline CI can
    // first prove fidelity, then apply its own clock warp with no validator/RPC.
    let fixture = pre_cliff.replay.to_fixture()?;
    let offline = Replay::from_fixture(&fixture)?;
    let matches = offline.verify(
        "offline baseline matches the landed pre-cliff revert",
        &[],
        &[Check::matches_onchain()],
    )?;
    assert!(matches.pass);

    let mut offline_future = Replay::from_fixture(&fixture)?;
    offline_future.advance_seconds(31 * DAY);
    let offline_future = offline_future.verify(
        "offline time travel unlocks the claim",
        &[],
        &[Check::success()],
    )?;
    assert!(offline_future.pass);
    println!(
        "fixture:    passed offline ({} accounts/programs)",
        fixture.entries.len()
    );
    Ok(())
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let rpc_url = std::env::var("SVMSCOPE_RPC_URL").unwrap_or_else(|_| DEFAULT_RPC_URL.into());
    let scope = Scope::new(rpc_url);
    let program_id = Address::from_str(PROGRAM_ID)?;
    let idl: serde_json::Value = serde_json::from_str(IDL_JSON)?;
    let program = scope.program_with_idl(program_id, idl);

    counter_demo(&scope, &program)?;
    vesting_demo(&scope, &program)?;

    println!("\nAll svmscope reference flows passed.");
    Ok(())
}
