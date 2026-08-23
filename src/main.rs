//! svmscope CLI — decode, replay, and mutate a Solana transaction.
//!
//! `--json` emits the whole analysis as JSON; `--mutate <addr>:<lamports>` applies
//! a what-if mutation before replaying.

use serde_json::json;
use solana_client::rpc_client::RpcClient;
use solana_client::rpc_request::RpcRequest;
use std::env;
use std::error::Error;

use svmscope::{analyze, compute, cpi_tree, diffs, replay, state, utils};

fn main() -> Result<(), Box<dyn Error>> {
    let args: Vec<String> = env::args().collect();

    let signature = args
        .get(1)
        .ok_or("usage: svmscope <transaction-signature> [--json] [--mutate <addr>:<lamports>]")?;

    let json_mode = args.iter().any(|a| a == "--json");
    let client = RpcClient::new("https://api.mainnet-beta.solana.com".to_string());

    // JSON mode: emit the whole analysis as one JSON blob and stop.
    if json_mode {
        let analysis = analyze(&client, signature)?;
        println!("{}", serde_json::to_string_pretty(&analysis)?);
        return Ok(());
    }

    // --- human-readable output ---
    let tx: serde_json::Value = client.send(
        RpcRequest::GetTransaction,
        json!([signature, { "encoding": "json", "maxSupportedTransactionVersion": 0 }]),
    )?;
    if tx.is_null() {
        return Err(format!("transaction not found: {signature}").into());
    }

    let account_keys = utils::resolve_account_keys(&tx);

    for e in &cpi_tree::build_cpi_tree(&tx) {
        if e.stack_height == 1 {
            println!("#{}  {}", e.index, e.program);
        } else {
            let indent = "    ".repeat((e.stack_height - 2) as usize);
            println!("{}└─ [{}] {}", indent, e.stack_height, e.program);
        }
    }

    println!("\n-- account balance changes --");
    for c in &diffs::account_diffs(&tx) {
        println!("{:<44} {:+} lamports", c.address, c.delta);
    }

    println!("\n-- compute units per program --");
    for c in &compute::cu_per_program(&tx) {
        println!("{:<44} {} CU", c.program, c.cu);
    }

    println!("\n-- account states --");
    state::fetch_account_states(&client, &account_keys);

    println!("\n-- replay --");
    let baseline = replay::replay_transaction(&client, signature, &account_keys);
    print_replay("REPLAY", &baseline);

    // Mutations come only from the CLI (`--mutate <address>:<lamports>`).
    let mut mutations: Vec<replay::Mutation> = Vec::new();
    let mut i = 2;
    while i < args.len() {
        if args[i] == "--mutate" {
            let spec = args.get(i + 1).expect("--mutate needs <address>:<lamports>");
            let (addr, lamports_str) = spec
                .split_once(':')
                .expect("mutation must look like <address>:<lamports>");
            let value: u64 = lamports_str.parse().expect("lamports must be a number");
            mutations.push(replay::Mutation::Lamports {
                address: addr.to_string(),
                value,
            });
            i += 2;
        } else {
            i += 1;
        }
    }

    if !mutations.is_empty() {
        let mutated = replay::mutate_and_replay(&client, signature, &account_keys, &mutations);
        print_replay("MUTATED REPLAY", &mutated);
    }

    Ok(())
}

/// Display a replay result (the CLI's job — the module returns data, `main` prints it).
fn print_replay(label: &str, r: &replay::ReplayResult) {
    if r.success {
        println!("{label}: success ✅  (compute units: {})", r.compute_units);
    } else {
        println!(
            "{label}: failed ❌  error: {}",
            r.error.as_deref().unwrap_or("unknown")
        );
    }
    for log in &r.logs {
        println!("  {log}");
    }
}
