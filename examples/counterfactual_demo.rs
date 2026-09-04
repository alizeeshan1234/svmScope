//! Counterfactual search: find the smallest change that flips a replay.
//!
//!   RPC=<url> cargo run --example counterfactual_demo -- <signature>
//!
//! Finds the minimum fee-payer balance at which the transaction still lands, by
//! binary-searching the balance rather than making anyone guess it.

use svmscope::{Mutation, Scope};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let rpc =
        std::env::var("RPC").unwrap_or_else(|_| "https://api.mainnet-beta.solana.com".to_string());
    let sig = std::env::args()
        .nth(1)
        .expect("usage: cargo run --example counterfactual_demo -- <signature>");

    let scope = Scope::new(&rpc);

    // The fee payer is the transaction's first account.
    let analysis = scope.analyze(&sig)?;
    let payer = analysis
        .balance_change
        .first()
        .expect("transaction has no accounts")
        .address
        .clone();

    let replay = scope.replay(&sig)?;
    let base = replay.run()?;
    println!("fee payer:  {payer}");
    println!("baseline:   success={}", base.result.success);
    if !base.result.success {
        println!("(baseline already reverts — balance isn't the question here)");
        return Ok(());
    }

    // Binary-search the fee payer's lamports for the flip point.
    let hi = 100_000_000u64; // 0.1 SOL — comfortably above any fee
    println!("\nsearching fee-payer balance in [0, {hi}] for the flip…");
    match replay.find_threshold(0, hi, |v| vec![Mutation::lamports(payer.clone(), v)])? {
        Some(th) => {
            println!(
                "→ lands with ≥ {} lamports; reverts below it   ({} replays)",
                th.flips_at, th.evaluations
            );
            println!(
                "  at {} it {}   ·   at 0 it {}",
                hi,
                if th.high_success {
                    "succeeds"
                } else {
                    "reverts"
                },
                if th.low_success {
                    "succeeds"
                } else {
                    "reverts"
                },
            );
        }
        None => println!("→ no flip in range — the fee-payer balance isn't the gating factor"),
    }

    Ok(())
}
