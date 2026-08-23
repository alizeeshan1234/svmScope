//! svmscope CLI — decode, replay, and mutate a Solana transaction.
//!
//! `--json` emits the whole analysis as JSON; `--mutate <addr>:<lamports>` applies
//! a what-if mutation before replaying.

use serde_json::json;
use solana_client::rpc_client::RpcClient;
use solana_client::rpc_request::RpcRequest;
use std::env;
use std::error::Error;

use svmscope::{analyze, api, compute, cpi_tree, diffs, replay, simulate_suite, state, utils};

fn main() -> Result<(), Box<dyn Error>> {
    let args: Vec<String> = env::args().collect();

    let signature = args
        .get(1)
        .ok_or("usage: svmscope <transaction-signature> [--json] [--mutate <addr>:<lamports>]\n       svmscope freeze <transaction-signature> [-o fixture.json]\n       svmscope test <scenarios.json>")?;

    let client = RpcClient::new("https://api.mainnet-beta.solana.com".to_string());

    // Test-runner mode: `svmscope test <scenarios.json>` runs a scenario suite
    // and exits non-zero if any assertion fails — drop it straight into CI.
    if signature == "test" {
        let path = args.get(2).ok_or("usage: svmscope test <scenarios.json>")?;
        return run_tests(&client, path);
    }

    // Freeze mode: `svmscope freeze <sig> [-o fixture.json]` captures a
    // self-contained fixture for deterministic, offline replay.
    if signature == "freeze" {
        let sig = args.get(2).ok_or("usage: svmscope freeze <signature> [-o fixture.json]")?;
        let out = args.iter().position(|a| a == "-o").and_then(|i| args.get(i + 1));
        eprintln!("freezing {sig}…");
        let fx = svmscope::capture_fixture(&client, sig)?;
        let json = fx.to_json()?;
        match out {
            Some(path) => {
                std::fs::write(path, &json).map_err(|e| format!("write {path}: {e}"))?;
                eprintln!("wrote {path} ({}, {} bytes)", fx.summary(), json.len());
            }
            None => println!("{json}"),
        }
        return Ok(());
    }

    let json_mode = args.iter().any(|a| a == "--json");

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

/// Run a scenario suite from a JSON file and print a test-runner report.
/// Exits the process with code 1 if any scenario fails its assertion.
///
/// Prefers a frozen `fixture` (deterministic, offline) over a live `signature`.
fn run_tests(client: &RpcClient, path: &str) -> Result<(), Box<dyn Error>> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("cannot read {path}: {e}"))?;
    let req: api::SuiteRequest = serde_json::from_str(&text).map_err(|e| format!("bad scenario file: {e}"))?;

    let scenarios = req
        .scenarios
        .into_iter()
        .map(|s| s.into_spec())
        .collect::<Result<Vec<_>, _>>()?;
    let n = scenarios.len();

    // Fixture path is resolved relative to the suite file's directory.
    let outcomes = if let Some(fx_ref) = &req.fixture {
        let fx_path = std::path::Path::new(path)
            .parent()
            .map(|d| d.join(fx_ref))
            .unwrap_or_else(|| std::path::PathBuf::from(fx_ref));
        let fx_text = std::fs::read_to_string(&fx_path)
            .map_err(|e| format!("cannot read fixture {}: {e}", fx_path.display()))?;
        let fx = svmscope::fixture::Fixture::from_json(&fx_text)?;
        println!(
            "svmscope test — fixture {} ({}, {n} scenarios) [deterministic, offline]\n",
            fx.signature,
            fx.summary()
        );
        svmscope::run_fixture_suite(&fx, scenarios)?
    } else if let Some(sig) = &req.signature {
        println!("svmscope test — {sig} ({n} scenarios) [live RPC — may drift]\n");
        simulate_suite(client, sig, scenarios)?
    } else {
        return Err("suite must specify either \"fixture\" or \"signature\"".into());
    };

    let mut passed = 0;
    for o in &outcomes {
        let got = if o.actual.success {
            "succeeded".to_string()
        } else {
            format!("reverted ({})", o.actual.error.as_deref().unwrap_or("error"))
        };
        let mark = if o.pass { passed += 1; "PASS" } else { "FAIL" };
        println!("  {mark}  {}  (expect: {}; got: {})", o.name, o.expect, got);
        for a in &o.asserts {
            let am = if a.pass { "✓" } else { "✗" };
            println!("          {am} assert {}", a.description);
        }
    }

    let total = outcomes.len();
    println!("\n{passed}/{total} passed");
    if passed != total {
        std::process::exit(1);
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
