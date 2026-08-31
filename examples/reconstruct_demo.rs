//! Show an account's on-chain write history before a slot — the ordered list of
//! transactions the reconstruction engine will replay to rebuild the account's
//! state at that slot. Works against any RPC with history (Old Faithful later).
//!
//!   RPC=<url> cargo run --example reconstruct_demo -- <address> <before_slot>

use svmscope::reconstruct::{write_history, RpcLedger};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let rpc =
        std::env::var("RPC").unwrap_or_else(|_| "https://api.mainnet-beta.solana.com".to_string());
    let mut args = std::env::args().skip(1);
    let address = args.next().expect("usage: … <address> <before_slot>");
    let before_slot: u64 = args
        .next()
        .expect("usage: … <address> <before_slot>")
        .parse()?;

    let ledger = RpcLedger::new(&rpc);
    let hist = write_history(&ledger, &address, before_slot, 12, 5)?;

    println!(
        "account {address}\nwrites before slot {before_slot} (oldest-first, {} found):",
        hist.len()
    );
    for w in &hist {
        println!("  slot {:>12}  {}", w.slot, w.signature);
    }
    if let Some(last) = hist.last() {
        println!(
            "\n→ reconstruction would start from a known anchor and replay these forward;\n  the last write ({}) at slot {} sets the account's state going into {before_slot}.",
            &last.signature[..16.min(last.signature.len())],
            last.slot
        );
    }
    Ok(())
}
