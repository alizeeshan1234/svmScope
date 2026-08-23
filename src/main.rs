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


    // Mutations come only from the CLI (`--mutate <address>:<lamports>`).
    let mut mutations: Vec<replay::Mutation> = Vec::new();

    let mut i = 2;
    while i < args.len() {
        if args[i] == "--mutate" {
            // guard: make sure there's a next arg
            let spec = args.get(i + 1).expect("--mutate needs <address>:<lamports>");
            let (addr, lamports_str) = spec
                .split_once(':')
                .expect("mutation must look like <address>:<lamports>");
            let value: u64 = lamports_str
                .parse()
                .expect("lamports must be a number");
            mutations.push(replay::Mutation::Lamports {
                address: addr.to_string(),
                value,
            });
            i += 2; // skip both --mutate and its value
        } else {
            i += 1;
        }
    }

    // Only run the mutated replay if the user actually asked for mutations.
    if !mutations.is_empty() {
        replay::mutate_and_replay(&client, signature, &account_keys, &mutations);
    }

    Ok(())
}
