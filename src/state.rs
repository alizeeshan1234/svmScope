//! Stage 2 — State reconstruction.
//!
//! To replay a transaction we first need the accounts it touched. This fetches
//! each account's current on-chain state via `getMultipleAccounts`.
//! (For recent transactions, current state ≈ the pre-transaction state.)

use solana_client::rpc_client::RpcClient;
use solana_client::rpc_request::RpcRequest;

/// Fetch and print the on-chain state of each account in the list.
///
/// TODO(you):
///   1. Call getMultipleAccounts with `account_keys` and {"encoding": "base64"}.
///      Same `client.send(...)` pattern you used for getTransaction — you'll need:
///          use serde_json::json;
///          use solana_client::rpc_request::RpcRequest;
///      then:
///          let resp: serde_json::Value = client.send(
///              RpcRequest::GetMultipleAccounts,
///              json!([account_keys, { "encoding": "base64" }]),
///          ).expect("getMultipleAccounts failed");
///
///   2. The accounts are in `resp["value"]` — an array PARALLEL to `account_keys`.
///      Each entry is either an account object or `null` (account doesn't exist).
///      An account object has:
///          ["owner"]       -> program that owns it (pubkey string)
///          ["lamports"]    -> balance (u64)
///          ["executable"]  -> bool (true = it's a program)
///          ["data"]        -> [ "<base64>", "base64" ]   (the account data, encoded)
///
///   3. Loop with the index so you can pair each result with account_keys[i].
///      For each account, print: address, owner, lamports, whether it's executable,
///      and the data size. (Getting the data string: resp["value"][i]["data"][0].)
///
/// Handle the `null` case (account doesn't exist) so it doesn't panic —
/// `if let Some(acc) = resp["value"][i].as_object()` or check `.is_null()`.
pub fn fetch_account_states(client: &RpcClient, account_keys: &[String]) {
    let resp: serde_json::Value = client.send(
        RpcRequest::GetMultipleAccounts,
        serde_json::json!([account_keys, { "encoding": "base64" }]),
    ).expect("getMultipleAccounts failed");

    let acconts = resp["value"].as_array().expect("value should be an array");

    for (i, acc) in acconts.iter().enumerate() {
        let address = &account_keys[i];
        if acc.is_null() {
            println!("{}: account does not exist", address);
        } else {
            let owner = acc["owner"].as_str().expect("owner should be str");
            let lamports = acc["lamports"].as_u64().expect("lamports should be u64");
            let executable = acc["executable"].as_bool().expect("executable should be bool");
            let data_size = acc["data"][0].as_str().expect("data[0] should be str").len();
            println!(
                "{}: owner={}, lamports={}, executable={}, data_size={}",
                address, owner, lamports, executable, data_size
            );
        }
    }
}
