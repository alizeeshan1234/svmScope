//! svmscope — transaction autopsy for Solana.
//!
//! Fetches a transaction by signature and prints its CPI call tree,
//! account balance changes, and compute units per program.

mod compute;
mod cpi_tree;
mod diffs;
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

    Ok(())
}
