//! svmscope library: decode, reconstruct, replay, and mutate Solana transactions.
//!
//! Shared by the CLI (`main.rs`) and the web server (`bin/server.rs`).

pub mod compute;
pub mod cpi_tree;
pub mod diffs;
pub mod replay;
pub mod state;
pub mod utils;

use serde::Serialize;
use serde_json::json;
use solana_client::rpc_client::RpcClient;
use solana_client::rpc_request::RpcRequest;

/// The full analysis of a transaction — the payload the CLI prints and the API serves.
#[derive(Serialize)]
pub struct Analysis {
    pub cpi_tree: Vec<cpi_tree::CpiEntry>,
    pub balance_change: Vec<diffs::BalanceChange>,
    pub compute: Vec<compute::CuUsage>,
    pub replay: replay::ReplayResult,
}

/// Fetch a transaction, decode it, and replay it — bundled into one `Analysis`.
pub fn analyze(client: &RpcClient, signature: &str) -> Result<Analysis, String> {
    let tx: serde_json::Value = client
        .send(
            RpcRequest::GetTransaction,
            json!([signature, { "encoding": "json", "maxSupportedTransactionVersion": 0 }]),
        )
        .map_err(|e| format!("RPC error: {e}"))?;

    if tx.is_null() {
        return Err(format!("transaction not found: {signature}"));
    }

    let account_keys = utils::resolve_account_keys(&tx);

    Ok(Analysis {
        cpi_tree: cpi_tree::build_cpi_tree(&tx),
        balance_change: diffs::account_diffs(&tx),
        compute: compute::cu_per_program(&tx),
        replay: replay::replay_transaction(client, signature, &account_keys),
    })
}

/// Replay a transaction after applying what-if mutations, returning just the result.
pub fn simulate(
    client: &RpcClient,
    signature: &str,
    mutations: &[replay::Mutation],
) -> Result<replay::ReplayResult, String> {
    let tx: serde_json::Value = client
        .send(
            RpcRequest::GetTransaction,
            json!([signature, { "encoding": "json", "maxSupportedTransactionVersion": 0 }]),
        )
        .map_err(|e| format!("RPC error: {e}"))?;

    if tx.is_null() {
        return Err(format!("transaction not found: {signature}"));
    }

    let account_keys = utils::resolve_account_keys(&tx);
    Ok(replay::mutate_and_replay(client, signature, &account_keys, mutations))
}
