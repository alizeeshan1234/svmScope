//! Hermetic CI: freeze a real transaction's world once, then regression-test
//! it forever — offline, deterministic, no RPC key in CI.
//!
//! ```sh
//! cargo run --example fixture_ci -- <signature> [rpc_url]
//! ```

use svmscope::{Check, Replay, Scenario, Scope};

fn main() -> svmscope::Result<()> {
    let mut args = std::env::args().skip(1);
    let sig = args.next().expect("usage: fixture_ci <signature> [rpc_url]");
    let rpc = args
        .next()
        .unwrap_or_else(|| "https://api.mainnet-beta.solana.com".into());

    // Capture once (live). The fixture carries the accounts, the program ELFs,
    // the IDLs, and the recorded on-chain outcome.
    let fixture = Scope::new(rpc).capture(&sig)?;
    let json = fixture.to_json()?;
    println!("captured: v{} — {}", fixture.version, fixture.summary());
    // In real use: fs::write("fixtures/my_tx.json", &json)? and commit it.

    // From here on: NO network. This is what runs in CI.
    let frozen = svmscope::Fixture::from_json(&json)?;
    let replay = Replay::from_fixture(&frozen)?;
    let outcomes = replay.run_suite(&[
        Scenario::new("replays exactly like mainnet").check(Check::matches_onchain()),
        Scenario::new("still succeeds").check(Check::success()),
    ])?;

    for o in &outcomes {
        println!("{} {}", if o.pass { "PASS" } else { "FAIL" }, o.name);
    }
    assert!(outcomes.iter().all(|o| o.pass), "regression!");
    Ok(())
}
