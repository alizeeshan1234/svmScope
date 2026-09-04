//! Verify the reconstruction engine: rebuild an account's state up to *now* by
//! replaying its whole write history forward, then compare to its ACTUAL current
//! on-chain state (which we can fetch for free). If they match, reconstruction
//! is correct — the same machinery then works at any historical slot.
//!
//!   RPC=<url> cargo run --example reconstruct_verify -- <address>

use svmscope::reconstruct::{reconstruct_account, RpcLedger};
use svmscope::Scope;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let rpc =
        std::env::var("RPC").unwrap_or_else(|_| "https://api.mainnet-beta.solana.com".to_string());
    let address = std::env::args().nth(1).expect("usage: … <address>");

    let scope = Scope::new(&rpc);
    let ledger = RpcLedger::new(&rpc);

    // Reconstruct as-of a slot past the tip, i.e. replay the entire history.
    let tip = scope.client().get_slot()?;
    println!("reconstructing {address} up to current tip (slot {tip})…");
    let recon = reconstruct_account(&scope, &ledger, &address, tip + 1, 80, 40)?;
    println!(
        "  writes replayed: {}   skipped: {}   exists: {}",
        recon.writes_replayed,
        recon.writes_skipped,
        recon.state.is_some()
    );

    // Ground truth: the account's real current state.
    let actual = scope.account_data(&address)?;

    match (recon.state, actual) {
        (Some(r), Some(a)) => {
            let data_match = r.data == a.data;
            println!(
                "  reconstructed data {} bytes · actual {} bytes",
                r.data.len(),
                a.data.len()
            );
            println!(
                "  DATA MATCH: {}{}",
                if data_match {
                    "✅ exact"
                } else {
                    "❌ differs"
                },
                if recon.writes_skipped > 0 {
                    "  (writes were skipped — expected to differ)"
                } else {
                    ""
                }
            );
            if !data_match {
                let n = r.data.len().min(a.data.len());
                let first_diff = (0..n).find(|&i| r.data[i] != a.data[i]);
                println!("  first differing byte offset: {first_diff:?}");
            }
        }
        (None, None) => println!("  both agree: account does not exist"),
        (r, a) => println!(
            "  existence mismatch: reconstructed={} actual={}",
            r.is_some(),
            a.is_some()
        ),
    }
    Ok(())
}
