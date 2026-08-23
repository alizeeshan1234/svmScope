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
use std::str::FromStr;

const NATIVE_LOADER: &str = "NativeLoader1111111111111111111111111111111";
const BPF_LOADER_2: &str = "BPFLoader2111111111111111111111111111111111";
const BPF_LOADER_UPGRADEABLE: &str = "BPFLoaderUpgradeab1e11111111111111111111111";

#[derive(serde::Serialize)]
pub struct ReplayResult {
    pub success: bool,
    pub error: Option<String>,
    pub logs: Vec<String>,
    pub compute_units: u64
}

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

    eprintln!("loaded {loaded} data accounts and {programs} programs into the SVM");
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
///
/// `Data` replaces an account's bytes wholesale; `DataPatch` overwrites a slice
/// at an offset (how you'd flip a token balance or an oracle price in place).
#[allow(dead_code)]
pub enum Mutation {
    Lamports { address: String, value: u64 },
    Data { address: String, bytes: Vec<u8> },
    DataPatch { address: String, offset: usize, bytes: Vec<u8> },
}

/// Apply one mutation to the SVM, or explain why it can't be applied.
fn apply_mutation(svm: &mut LiteSVM, m: &Mutation) -> Result<(), String> {
    let (address, addr) = match m {
        Mutation::Lamports { address, .. }
        | Mutation::Data { address, .. }
        | Mutation::DataPatch { address, .. } => (
            address,
            Address::from_str(address).map_err(|_| format!("invalid address: {address}"))?,
        ),
    };
    let mut account = svm
        .get_account(&addr)
        .ok_or_else(|| format!("mutation target not loaded in SVM: {address}"))?;

    match m {
        Mutation::Lamports { value, .. } => account.lamports = *value,
        Mutation::Data { bytes, .. } => account.data = bytes.clone(),
        Mutation::DataPatch { offset, bytes, .. } => {
            let end = offset + bytes.len();
            if end > account.data.len() {
                return Err(format!(
                    "patch out of range for {address}: offset {offset} + {} bytes > account size {}",
                    bytes.len(),
                    account.data.len()
                ));
            }
            account.data[*offset..end].copy_from_slice(bytes);
        }
    }
    svm.set_account(addr, account)
        .map_err(|e| format!("set_account failed for {address}: {e:?}"))
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

/// Convert a raw transaction result into a `ReplayResult`.
///
/// This does NOT print — it just extracts the data. The caller (CLI or a UI)
/// decides how to display it.
fn to_replay_result(result: TransactionResult) -> ReplayResult {
    match result {
        Ok(meta) => ReplayResult {
            success: true,
            error: None,
            logs: meta.logs,
            compute_units: meta.compute_units_consumed,
        },
        Err(failed) => ReplayResult {
            success: false,
            error: Some(format!("{:?}", failed.err)),
            logs: failed.meta.logs,
            compute_units: failed.meta.compute_units_consumed,
        },
    }
}

/// Replay the transaction against the reconstructed state.
pub fn replay_transaction(client: &RpcClient, signature: &str, account_keys: &[String]) -> ReplayResult {
    let (mut svm, tx) = setup(client, signature, account_keys);
    let result = svm.send_transaction(tx);
    to_replay_result(result)
}

/// Replay the transaction after applying the given mutations.
pub fn mutate_and_replay(
    client: &RpcClient,
    signature: &str,
    account_keys: &[String],
    mutations: &[Mutation],
) -> ReplayResult {
    let (mut svm, tx) = setup(client, signature, account_keys);

    for m in mutations {
        if let Err(e) = apply_mutation(&mut svm, m) {
            return ReplayResult {
                success: false,
                error: Some(e),
                logs: vec![],
                compute_units: 0,
            };
        }
    }

    let result = svm.send_transaction(tx);
    to_replay_result(result)
}