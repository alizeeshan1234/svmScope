//! Fidelity sweep: how many recent on-chain-successful transactions does the
//! free *reconstructed* tier reproduce as successes? Every miss is printed
//! with its error so the remaining drift classes stay visible.
//!
//! `cargo run --example fidelity_sweep -- [rpc-url] [per-program]`
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
    (
        "Token program",
        "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA",
    ),
];

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    let rpc = args
        .get(1)
        .cloned()
        .unwrap_or_else(|| "https://api.mainnet-beta.solana.com".to_string());
    let per: usize = args.get(2).and_then(|n| n.parse().ok()).unwrap_or(4);
    let scope = Scope::new(&rpc);
    let (mut total, mut reproduced) = (0, 0);
    for (label, program) in PROGRAMS {
        let sigs = match scope.signatures(program, per * 4) {
            Ok(s) => s,
            Err(e) => {
                println!("{label}: signatures failed: {e}");
                continue;
            }
        };
        let mut taken = 0;
        for s in sigs.iter().filter(|s| !s.err) {
            if taken >= per {
                break;
            }
            let replay = match scope.replay_at_slot(&s.signature) {
                Ok(r) => r,
                Err(e) => {
                    println!("{label} {}: fetch error: {e}", &s.signature[..12]);
                    continue;
                }
            };
            taken += 1;
            total += 1;
            let out = replay.run()?;
            if out.result.success {
                reproduced += 1;
                println!(
                    "{label} {}: reproduced ({} CU)",
                    &s.signature[..12],
                    out.result.compute_units
                );
            } else {
                println!(
                    "{label} {}: MISS {} — {}",
                    &s.signature[..12],
                    out.result.error_name.clone().unwrap_or_default(),
                    out.result.error.clone().unwrap_or_default()
                );
            }
        }
    }
    println!("\n{reproduced}/{total} on-chain successes reproduced by the reconstructed tier");
    Ok(())
}
