//! Exact recursive reconstruction: rebuild an account to *now* via the
//! dependency-cone engine, and compare to actual on-chain state byte-for-byte.
//!
//!   RPC=<url> cargo run --example reconstruct_exact -- <address> [budget]

use svmscope::reconstruct::{Reconstructor, RpcLedger};
use svmscope::Scope;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let rpc =
        std::env::var("RPC").unwrap_or_else(|_| "https://api.mainnet-beta.solana.com".to_string());
    let mut args = std::env::args().skip(1);
    let address = args.next().expect("usage: … <address> [budget]");
    let budget: usize = args.next().and_then(|s| s.parse().ok()).unwrap_or(150);

    let scope = Scope::new(&rpc);
    let ledger = RpcLedger::new(&rpc);
    let tip = scope.client().get_slot()?;

    println!("exact-reconstructing {address} to slot {tip} (budget {budget} replays)…");
    let mut r = Reconstructor::new(&scope, &ledger, budget);
    let recon = r.reconstruct(&address, tip + 1)?;
    println!(
        "  replays: {}   exact-cone: {}   exists: {}",
        r.replays(),
        recon.exact,
        recon.state.is_some()
    );

    let actual = scope.account_data(&address)?;
    match (recon.state, actual) {
        (Some(rc), Some(ac)) => {
            let full = rc.data == ac.data;
            println!(
                "  data {} vs {} bytes · FULL MATCH: {}",
                rc.data.len(),
                ac.data.len(),
                if full { "✅" } else { "❌" }
            );
            // Spotlight the SPL token amount (offset 64..72), the coupled field
            // the naive engine got wrong.
            if rc.data.len() >= 72 && ac.data.len() >= 72 {
                let amt = |d: &[u8]| u64::from_le_bytes(d[64..72].try_into().unwrap());
                println!(
                    "  amount@64: reconstructed {} · actual {} · {}",
                    amt(&rc.data),
                    amt(&ac.data),
                    if amt(&rc.data) == amt(&ac.data) {
                        "✅ exact"
                    } else {
                        "❌ off"
                    }
                );
            }
            if !full {
                let n = rc.data.len().min(ac.data.len());
                let diffs: Vec<usize> = (0..n).filter(|&i| rc.data[i] != ac.data[i]).collect();
                println!("  differing offsets: {:?}", &diffs[..diffs.len().min(12)]);
            }
        }
        (a, b) => println!(
            "  existence: reconstructed={} actual={}",
            a.is_some(),
            b.is_some()
        ),
    }
    Ok(())
}
