//! Patch Lab: replay a transaction against the original program and a patched
//! one, and report the difference.
//!
//!   RPC=<url> cargo run --example patch_lab_demo -- <signature> <program_id>
//!
//! This demo uses a deliberately-broken ELF as the "patch" to show the A/B path
//! detecting a behavioral change — then confirms the original is restored.

use svmscope::Scope;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let rpc =
        std::env::var("RPC").unwrap_or_else(|_| "https://api.mainnet-beta.solana.com".to_string());
    let mut args = std::env::args().skip(1);
    let sig = args.next().expect("usage: … <signature> <program_id>");
    let program = args.next().expect("usage: … <signature> <program_id>");

    let scope = Scope::new(&rpc);
    let mut replay = scope.replay(&sig)?;

    // A stand-in "patched" binary — broken on purpose, to prove the comparison
    // detects a behavioral change. In real use this is your recompiled .so.
    let patched: Vec<u8> = vec![0u8; 64];

    let cmp = replay.compare_patch(&program, patched, &[])?;
    println!("patch comparison: {}", cmp.summary());
    println!(
        "  before:  success={:<5} cu={:<7} err={:?}",
        cmp.before.success, cmp.before.compute_units, cmp.before.error
    );
    println!(
        "  after:   success={:<5} cu={:<7} err={:?}",
        cmp.after.success, cmp.after.compute_units, cmp.after.error
    );
    println!("  changed: {}", cmp.changed());

    // Prove the original was restored: a fresh run must match `before`.
    let restored = replay.run()?;
    println!(
        "  restored run matches before: {}",
        restored.result.success == cmp.before.success
            && restored.result.compute_units == cmp.before.compute_units
    );

    Ok(())
}
