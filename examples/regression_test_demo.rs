//! Freeze an incident and emit its permanent regression test.
//!
//!   RPC=<url> cargo run --example regression_test_demo -- <signature>

use svmscope::{Fixture, Replay, Scope};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let rpc =
        std::env::var("RPC").unwrap_or_else(|_| "https://api.mainnet-beta.solana.com".to_string());
    let sig = std::env::args().nth(1).expect("usage: … <signature>");

    let scope = Scope::new(&rpc);
    let replay = scope.replay(&sig)?;

    // Freeze the incident to a portable fixture.
    let json = replay.to_fixture()?.to_json()?;
    println!("frozen fixture: {} bytes", json.len());

    // Prove it replays fully offline (no RPC) and matches the live outcome.
    let offline = Replay::from_fixture(&Fixture::from_json(&json)?)?;
    let live = replay.run()?;
    let off = offline.run()?;
    println!(
        "live vs offline: success {}=={}  cu {}=={}",
        live.result.success, off.result.success, live.result.compute_units, off.result.compute_units
    );

    // Emit the permanent regression test.
    let name = format!("incident_{}", &sig[..8.min(sig.len())]);
    let src = replay.regression_test(&name, "incident.json")?;
    println!("\n--- generated regression test ---\n{src}");

    Ok(())
}
