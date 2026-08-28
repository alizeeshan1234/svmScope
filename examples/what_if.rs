//! What-if testing: mutate state, warp the clock, and assert on the outcome —
//! against a real transaction's reconstructed world.
//!
//! ```sh
//! cargo run --example what_if -- <signature> <account> [rpc_url]
//! ```

use svmscope::{Check, Cmp, Mutation, Scope};

fn main() -> svmscope::Result<()> {
    let mut args = std::env::args().skip(1);
    let sig = args
        .next()
        .expect("usage: what_if <signature> <account> [rpc_url]");
    let account = args
        .next()
        .expect("usage: what_if <signature> <account> [rpc_url]");
    let rpc = args
        .next()
        .unwrap_or_else(|| "https://api.mainnet-beta.solana.com".into());

    // One fetch pass; every run below is local and free.
    let scope = Scope::new(rpc);
    let mut replay = scope.replay(&sig)?;

    // Baseline: does the replay behave like the chain did?
    let baseline = replay.verify("baseline", &[], &[Check::matches_onchain()])?;
    println!("baseline matches on-chain: {}", baseline.pass);

    // What if this account were drained? (Declarative checks, mollusk-style.)
    let drained = replay.verify(
        "drained account",
        &[Mutation::lamports(&*account, 0)],
        &[
            Check::any_outcome(), // just observe — or assert Check::revert()
            Check::account(&*account).lamports(Cmp::eq(0)).build(),
        ],
    )?;
    println!("drained scenario: pass={}", drained.pass);
    for a in &drained.asserts {
        println!("  {} {}", if a.pass { "✓" } else { "✗" }, a.description);
    }

    // What about 30 days from now? Time is just the Clock sysvar — warp it.
    replay.advance_seconds(30 * 86_400);
    println!(
        "+30 days ({}): success={}",
        replay.describe_clock(),
        replay.run()?.result.success
    );
    Ok(())
}
