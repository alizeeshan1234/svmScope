//! svmscope — transaction autopsy for Solana (v0.1)
//!
//! v0.1 goal: take a saved `getTransaction` JSON and print
//!   1. the CPI call tree   <- YOUR first brick (src/cpi_tree.rs)
//!   2. compute units per program   (later)
//!   3. account balance diffs        (later)

mod cpi_tree;
mod diffs;
mod compute;

use clap::Parser;
use std::env;

/// Command-line arguments.
#[derive(Parser)]
#[command(name = "svmscope", about = "Transaction autopsy for Solana")]
struct Args {
    /// Path to a saved getTransaction JSON file (e.g. sample_tx.json)
    tx_file: String,
}

use solana_client::rpc_client::RpcClient;
use solana_client::rpc_request::RpcRequest;
use serde_json::json;

fn main() {
    // let args = Args::parse();

    // // Read the file and parse it into a generic JSON value.
    // // (Later you can swap this for typed `solana-transaction-status` structs.)
    // let raw = fs::read_to_string(&args.tx_file)
    //     .unwrap_or_else(|e| panic!("could not read {}: {e}", args.tx_file));
    // let tx: serde_json::Value =
    //     serde_json::from_str(&raw).expect("file is not valid JSON");

    // println!("== svmscope :: {} ==\n", args.tx_file);

    let args: Vec<String> = env::args().collect();
    let tx_signatre = &args[1] as &str;

    let client = RpcClient::new("https://api.mainnet-beta.solana.com".to_string());

    let tx: serde_json::Value = client.send(RpcRequest::GetTransaction, json!([tx_signatre, {
        "encoding": "json",
        "maxSupportedTransactionVersion": 0
    }])).expect("Rpc call failed");

    // ---- YOUR FIRST BRICK ----
    cpi_tree::print_cpi_tree(&tx);

    println!("\n-- account balance changes --");
    diffs::print_account_diffs(&tx);

    println!("\n-- compute units per program --");
    compute::print_cu_per_program(&tx);
}
