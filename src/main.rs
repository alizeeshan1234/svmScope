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

fn main() {
    let args: Vec<String> = env::args().collect();
    let signature = &args[1];

    let client = RpcClient::new("https://api.mainnet-beta.solana.com".to_string());

    let tx: serde_json::Value = client
        .send(
            RpcRequest::GetTransaction,
            json!([signature, {
                "encoding": "json",
                "maxSupportedTransactionVersion": 0
            }]),
        )
        .expect("RPC call failed");

    cpi_tree::print_cpi_tree(&tx);

    println!("\n-- account balance changes --");
    diffs::print_account_diffs(&tx);

    println!("\n-- compute units per program --");
    compute::print_cu_per_program(&tx);
}
