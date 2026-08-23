//! svmscope — transaction autopsy for Solana.
//!
//! Fetches a transaction by signature and prints its CPI call tree,
//! account balance changes, and compute units per program.

mod compute;
mod cpi_tree;
mod diffs;
mod replay;
mod state;
mod utils;

use serde_json::json;
use solana_client::rpc_client::RpcClient;
use solana_client::rpc_request::RpcRequest;
use std::env;
use std::error::Error;

fn main() -> Result<(), Box<dyn Error>> {
    let args: Vec<String> = env::args().collect();

    // Missing argument -> clean usage message instead of an index-out-of-bounds panic.
    let signature = args
        .get(1)
        .ok_or("usage: svmscope <transaction-signature>")?;

    let client = RpcClient::new("https://api.mainnet-beta.solana.com".to_string());

    // A network/RPC failure (bad signature format, node down, ...) becomes an
    // error that `?` returns out of main and prints cleanly.
    let tx: serde_json::Value = client.send(
        RpcRequest::GetTransaction,
        json!([signature, {
            "encoding": "json",
            "maxSupportedTransactionVersion": 0
        }]),
    )?;

    // A well-formed but nonexistent signature comes back as JSON null.
    if tx.is_null() {
        return Err(format!("transaction not found: {signature}").into());
    }

    cpi_tree::print_cpi_tree(&tx);

    println!("\n-- account balance changes --");
    diffs::print_account_diffs(&tx);

    println!("\n-- compute units per program --");
    compute::print_cu_per_program(&tx);

    // Stage 2: reconstruct the state the transaction ran against.
    let account_keys = utils::resolve_account_keys(&tx);
    println!("\n-- account states --");
    state::fetch_account_states(&client, &account_keys);

    // Stage 3: reconstruct state and replay the transaction locally.
    println!("\n-- replay --");
    replay::replay_transaction(&client, signature, &account_keys);

    let mutations = vec![
        replay::Mutation {
            address: account_keys[0].clone(),
            new_lamports: 0
        }
    ];
    replay::mutate_and_replay(&client, signature, &account_keys, &mutations);

    Ok(())
}
