//! Live comparison of current-state replay vs. free reconstructed replay-at-slot.
//!
//!   RPC=<url> cargo run --example replay_at_slot_demo -- <signature>
//!
//! Prints the fidelity label and outcome of each, against the recorded on-chain
//! result — so you can see the reconstruction actually anchoring to the tx's slot.

use svmscope::Scope;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let rpc =
        std::env::var("RPC").unwrap_or_else(|_| "https://api.mainnet-beta.solana.com".to_string());
    let sig = std::env::args()
        .nth(1)
        .expect("usage: cargo run --example replay_at_slot_demo -- <signature>");

    let scope = Scope::new(&rpc);

    // Ground truth: what actually happened on-chain.
    let recon = scope.replay_at_slot(&sig)?;
    if let Some(rec) = recon.recorded() {
        println!(
            "on-chain (recorded)   success={:<5}  cu={:?}  slot={:?}",
            rec.success, rec.compute_units, rec.slot
        );
    }

    // 1. Plain current-state replay.
    let plain = scope.replay(&sig)?;
    let a = plain.run()?;
    println!(
        "replay()              fidelity={:<20} success={:<5} cu={}  err={:?}",
        plain.fidelity().label(),
        a.result.success,
        a.result.compute_units,
        a.result.error
    );

    // 2. Free reconstructed replay-at-slot (no archive).
    let b = recon.run()?;
    println!(
        "replay_at_slot()      fidelity={:<20} success={:<5} cu={}  err={:?}",
        recon.fidelity().label(),
        b.result.success,
        b.result.compute_units,
        b.result.error
    );
    println!("clock anchored to:    {}", recon.describe_clock());
    if !b.result.logs.is_empty() {
        println!("--- program logs ---");
        for l in b.result.logs.iter().take(12) {
            println!("  {l}");
        }
    }

    Ok(())
}
