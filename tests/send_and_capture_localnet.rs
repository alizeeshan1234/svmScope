use std::thread;
use std::time::{Duration, Instant};

use serde_json::json;
use solana_address::Address;
use solana_keypair::Keypair;
use solana_message::{Message, VersionedMessage};
use solana_signer::Signer;
use solana_system_interface::instruction;
use solana_transaction::versioned::VersionedTransaction;
use svmscope::Scope;

const RPC_URL: &str = "http://127.0.0.1:8899";
const AIRDROP_LAMPORTS: u64 = 1_000_000_000;
const TRANSFER_LAMPORTS: u64 = 2_000_000;
const FOLLOWUP_LAMPORTS: u64 = 1_000_000;

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
            return Err(format!("timed out waiting for airdrop; balance is {balance}").into());
        }
        thread::sleep(Duration::from_millis(100));
    }
}

#[test]
#[ignore = "requires solana-test-validator on port 8899"]
fn sends_captures_and_replays_a_local_transaction() -> Result<(), Box<dyn std::error::Error>> {
    let scope = Scope::new(RPC_URL);
    let payer = Keypair::new();
    let recipient = Keypair::new();

    scope
        .client()
        .request_airdrop(&payer.pubkey(), AIRDROP_LAMPORTS)?;
    wait_for_balance(&scope, &payer.pubkey(), AIRDROP_LAMPORTS)?;

    let blockhash = scope.client().get_latest_blockhash()?;
    let transfer = instruction::transfer(&payer.pubkey(), &recipient.pubkey(), TRANSFER_LAMPORTS);
    let message = Message::new_with_blockhash(&[transfer], Some(&payer.pubkey()), &blockhash);
    let transaction = VersionedTransaction::try_new(VersionedMessage::Legacy(message), &[&payer])?;

    let captured = scope.send_and_capture(transaction)?;
    assert!(!captured.signature.is_empty());

    let recorded = captured
        .replay
        .recorded()
        .expect("submitted transaction should have a recorded outcome");
    assert!(
        recorded.success,
        "validator transaction failed: {:?}",
        recorded.error
    );

    let replayed = captured.replay.run()?;
    assert!(
        replayed.result.success,
        "local replay failed: {:?}",
        replayed.result.error
    );
    assert!(
        replayed.diffs.iter().any(|diff| {
            diff.address == payer.pubkey().to_string()
                && diff.lamports_before.saturating_sub(diff.lamports_after) >= TRANSFER_LAMPORTS
        }),
        "payer transfer was not present in replay diffs: {:?}",
        replayed.diffs
    );

    // Immediately spend from the account the first confirmed transaction just
    // created. Account capture must query confirmed state too; querying the RPC
    // default (finalized) races and captures this payer as missing.
    let second_recipient = Keypair::new();
    let blockhash = scope.client().get_latest_blockhash()?;
    let followup = instruction::transfer(
        &recipient.pubkey(),
        &second_recipient.pubkey(),
        FOLLOWUP_LAMPORTS,
    );
    let message = Message::new_with_blockhash(&[followup], Some(&recipient.pubkey()), &blockhash);
    let transaction =
        VersionedTransaction::try_new(VersionedMessage::Legacy(message), &[&recipient])?;
    let followup = scope.send_and_capture(transaction)?;
    let followup_recorded = followup
        .replay
        .recorded()
        .expect("follow-up transaction should have metadata");
    assert!(
        followup_recorded.success,
        "follow-up transaction failed on localnet: {:?}",
        followup_recorded.error
    );
    assert!(
        followup.replay.run()?.result.success,
        "confirmed account state was not captured for the follow-up transaction"
    );

    Ok(())
}

#[test]
#[ignore = "requires solana-test-validator on port 8899"]
fn idl_builder_returns_a_landed_program_failure() -> Result<(), Box<dyn std::error::Error>> {
    let scope = Scope::new(RPC_URL);
    let payer = Keypair::new();
    scope
        .client()
        .request_airdrop(&payer.pubkey(), AIRDROP_LAMPORTS)?;
    wait_for_balance(&scope, &payer.pubkey(), AIRDROP_LAMPORTS)?;

    // A missing program makes the instruction fail inside the runtime. That is
    // deliberate here: it proves program failures still produce a signature,
    // confirmation metadata, and a locally replayable outcome.
    let missing_program = Keypair::new().pubkey();
    let idl = json!({
        "instructions": [{
            "name": "alwaysFails",
            "discriminator": [1, 2, 3, 4, 5, 6, 7, 8],
            "accounts": [{
                "name": "authority",
                "writable": true,
                "signer": true
            }],
            "args": [{ "name": "value", "type": "u64" }]
        }]
    });
    let captured = scope
        .program_with_idl(missing_program, idl)
        .method("alwaysFails")?
        .payer(&payer)
        .account_signer("authority", &payer)
        .arg("value", 42_u64)
        .send_and_capture()?;

    assert!(!captured.signature.is_empty());
    assert!(
        !captured
            .replay
            .recorded()
            .expect("landed failure should have metadata")
            .success
    );
    assert!(!captured.replay.run()?.result.success);

    Ok(())
}
