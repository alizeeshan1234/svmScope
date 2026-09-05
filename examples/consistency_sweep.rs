//! Consistency sweep: for recent transactions of several programs, fetch the
//! world ONCE and compare the whole-transaction replay (`Replay::run`, which
//! sends) against the debugger's last prefix (`Replay::trace`, which
//! simulates the same full transaction read-only). Any disagreement on the
//! same state is an engine bug, not drift.
//!
//! `cargo run --example consistency_sweep -- [rpc-url] [per-program]`
use svmscope::Scope;

const PROGRAMS: &[(&str, &str)] = &[
    ("Jupiter v6", "JUP6LkbZbjS1jKKwapdHNy74zcZ3tLUZoi5QNyVTaV4"),
    (
        "Raydium AMM v4",
        "675kPX9MHTjS2zt1qfr1NYHuzeLXfQM9H24wFSUt1Mp8",
    ),
    (
        "Orca Whirlpool",
        "whirLbMiicVdio4qvUfM5KAg6Ct8VwpYzGff3uctyCc",
    ),
    (
        "Meteora DLMM",
        "LBUZKhRxPF3XUpBCjp4YzTKgLccjZhTSDM9YuVaPwxo",
    ),
    ("Pump AMM", "pAMMBay6oceH9fJKBRHGP5D4bD4sWpmSwMn52FMfXEA"),
];

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    let rpc = args
        .get(1)
        .cloned()
        .unwrap_or_else(|| "https://api.mainnet-beta.solana.com".to_string());
    let per: usize = args.get(2).and_then(|n| n.parse().ok()).unwrap_or(4);
    let scope = Scope::new(&rpc);
    let (mut total, mut agree, mut disagree, mut errors) = (0, 0, 0, 0);
    for (label, program) in PROGRAMS {
        let sigs = match scope.signatures(program, per * 3) {
            Ok(s) => s,
            Err(e) => {
                println!("{label}: signatures failed: {e}");
                continue;
            }
        };
        let mut taken = 0;
        for s in sigs {
            if taken >= per {
                break;
            }
            let replay = match scope.replay(&s.signature) {
                Ok(r) => r,
                Err(e) => {
                    errors += 1;
                    println!("{label} {}: fetch error: {e}", &s.signature[..12]);
                    continue;
                }
            };
            taken += 1;
            total += 1;
            let run = replay.run()?.result;
            let trace = replay.trace(&[])?;
            let same = run.success == trace.result.success
                && run.error == trace.result.error
                && run.compute_units == trace.result.compute_units;
            if same {
                agree += 1;
                println!(
                    "{label} {}: agree ({}, {} CU)",
                    &s.signature[..12],
                    if run.success { "ok" } else { "fail" },
                    run.compute_units
                );
            } else {
                disagree += 1;
                println!(
                    "{label} {}: DISAGREE run={:?}/{:?}/{} trace={:?}/{:?}/{} failed_step={:?}",
                    &s.signature[..12],
                    run.success,
                    run.error,
                    run.compute_units,
                    trace.result.success,
                    trace.result.error,
                    trace.result.compute_units,
                    trace.failed_step
                );
            }
        }
    }
    println!("\n{total} compared: {agree} agree, {disagree} disagree, {errors} fetch errors");
    Ok(())
}
