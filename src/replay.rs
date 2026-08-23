//! Stage 3 — Deterministic replay.
//!
//! Loads the transaction's accounts AND programs into an in-process SVM
//! (LiteSVM) so the transaction can be re-executed locally.

use base64::Engine;
use litesvm::types::TransactionResult;
use litesvm::LiteSVM;
use serde_json::json;
use solana_account::Account;
use solana_address::Address;
use solana_client::rpc_client::RpcClient;
use solana_client::rpc_request::RpcRequest;
use solana_transaction::versioned::VersionedTransaction;
use std::ops::Add;
use std::str::FromStr;

const NATIVE_LOADER: &str = "NativeLoader1111111111111111111111111111111";
const BPF_LOADER_2: &str = "BPFLoader2111111111111111111111111111111111";
const BPF_LOADER_UPGRADEABLE: &str = "BPFLoaderUpgradeab1e11111111111111111111111";

fn b64_decode(s: &str) -> Vec<u8> {
    base64::engine::general_purpose::STANDARD
        .decode(s)
        .expect("bad base64")
}

/// Fetch a single account's raw data via getAccountInfo.
fn fetch_account_data(client: &RpcClient, address: &str) -> Option<Vec<u8>> {
    let resp: serde_json::Value = client
        .send(
            RpcRequest::GetAccountInfo,
            json!([address, { "encoding": "base64" }]),
        )
        .ok()?;
    let data_b64 = resp["value"]["data"][0].as_str()?;
    Some(b64_decode(data_b64))
}

/// Build a LiteSVM instance with the transaction's accounts and programs loaded.
pub fn build_svm(client: &RpcClient, account_keys: &[String]) -> LiteSVM {
    let resp: serde_json::Value = client
        .send(
            RpcRequest::GetMultipleAccounts,
            json!([account_keys, { "encoding": "base64" }]),
        )
        .expect("getMultipleAccounts failed");

    let accounts = resp["value"]
        .as_array()
        .expect("value should be an array");

    // Disable sigverify + blockhash check: we're replaying an already-signed
    // transaction, so its original blockhash won't be valid in a fresh SVM.
    let mut svm = LiteSVM::new()
        .with_sigverify(false)
        .with_blockhash_check(false);
    let mut loaded = 0;
    let mut programs = 0;

    for (i, acc) in accounts.iter().enumerate() {
        if acc.is_null() {
            continue; // account doesn't exist (created during the tx, etc.)
        }

        let address = Address::from_str(&account_keys[i]).expect("bad account address");
        let owner = acc["owner"].as_str().unwrap();
        let executable = acc["executable"].as_bool().unwrap();

        if executable {
            // Resolve the program's ELF bytecode based on which loader owns it.
            let elf: Option<Vec<u8>> = if owner == NATIVE_LOADER {
                None // native programs are built into LiteSVM
            } else if owner == BPF_LOADER_2 {
                // Non-upgradeable: the ELF is the account data itself.
                Some(b64_decode(acc["data"][0].as_str().unwrap()))
            } else if owner == BPF_LOADER_UPGRADEABLE {
                // Upgradeable: the program account is a pointer.
                // bytes 4..36 = the programdata account address; ELF starts at
                // offset 45 inside that programdata account.
                let prog = b64_decode(acc["data"][0].as_str().unwrap());
                if prog.len() >= 36 {
                    let pd_bytes: [u8; 32] = prog[4..36].try_into().unwrap();
                    let pd_addr = Address::from(pd_bytes);
                    fetch_account_data(client, &pd_addr.to_string())
                        .filter(|d| d.len() > 45)
                        .map(|d| d[45..].to_vec())
                } else {
                    None
                }
            } else {
                None // unknown loader
            };

            if let Some(elf) = elf {
                match svm.add_program(address, &elf) {
                    Ok(()) => programs += 1,
                    Err(e) => eprintln!("warn: skipped program {}: {:?}", account_keys[i], e),
                }
            }
            continue;
        }

        // Data account: load it verbatim.
        let owner_addr = Address::from_str(owner).expect("bad owner address");
        let lamports = acc["lamports"].as_u64().unwrap();
        let data = b64_decode(acc["data"][0].as_str().unwrap());

        let account = Account {
            lamports,
            data,
            owner: owner_addr,
            executable: false,
            rent_epoch: 0,
        };
        svm.set_account(address, account).expect("set_account failed");
        loaded += 1;
    }

    println!("loaded {loaded} data accounts and {programs} programs into the SVM");
    svm
}

/// Fetch the raw transaction (base64 wire bytes) and deserialize it.
fn fetch_transaction(client: &RpcClient, signature: &str) -> VersionedTransaction {
    let resp: serde_json::Value = client
        .send(
            RpcRequest::GetTransaction,
            json!([signature, { "encoding": "base64", "maxSupportedTransactionVersion": 0 }]),
        )
        .expect("getTransaction failed");
    let tx_b64 = resp["transaction"][0]
        .as_str()
        .expect("no base64 transaction in response");
    let bytes = b64_decode(tx_b64);
    bincode::deserialize::<VersionedTransaction>(&bytes).expect("failed to deserialize transaction")
}

/// A change to apply to an account before replaying.
pub enum Mutation {
    Lamports { address: String, value: u64 },
    Data { address: String, bytes: Vec<u8> },
}

/// Shared setup: fetch the transaction, load the Address Lookup Table accounts it
/// references (so the SVM can resolve a versioned tx), and build the SVM.
fn setup(
    client: &RpcClient,
    signature: &str,
    account_keys: &[String],
) -> (LiteSVM, VersionedTransaction) {
    let tx = fetch_transaction(client, signature);

    let mut all_keys = account_keys.to_vec();
    if let Some(lookups) = tx.message.address_table_lookups() {
        for l in lookups {
            all_keys.push(l.account_key.to_string());
        }
    }

    let svm = build_svm(client, &all_keys);
    (svm, tx)
}

/// Print a replay result under the given label.
fn report(label: &str, result: TransactionResult) {
    match result {
        Ok(meta) => {
            println!("{label}: success ✅  (compute units: {})", meta.compute_units_consumed);
            for log in &meta.logs {
                println!("  {log}");
            }
        }
        Err(failed) => {
            println!("{label}: failed ❌  error: {:?}", failed.err);
            for log in &failed.meta.logs {
                println!("  {log}");
            }
        }
    }
}

/// Replay the transaction against the reconstructed state and report the result.
pub fn replay_transaction(client: &RpcClient, signature: &str, account_keys: &[String]) {
    let (mut svm, tx) = setup(client, signature, account_keys);
    let result = svm.send_transaction(tx);
    report("REPLAY", result);
}

/// Replay the transaction after applying the given mutations, and report the result.
pub fn mutate_and_replay(
    client: &RpcClient,
    signature: &str,
    account_keys: &[String],
    mutations: &[Mutation],
) {
    let (mut svm, tx) = setup(client, signature, account_keys);

    for m in mutations {
        match m {
            Mutation::Lamports { address, value } => {
                let addr = Address::from_str(address).unwrap();
                let mut account = svm.get_account(&addr).unwrap();
                account.lamports = *value;
                svm.set_account(addr, account).unwrap();
            }
            Mutation::Data { address, bytes } => {
                let addr = Address::from_str(address).unwrap();
                let mut account = svm.get_account(&addr).unwrap();
                account.data = bytes.clone();
                svm.set_account(addr, account).unwrap();
            }
        }
    }

    let result = svm.send_transaction(tx);
    report("MUTATED REPLAY", result);
}