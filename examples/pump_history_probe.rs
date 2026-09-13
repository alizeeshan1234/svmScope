//! Experimental A/B replay using four reserve fields recovered from Pump events.
//! Input: output of scripts/probe_pump_history.py; select an event by its index.
//! Uses current bytes for every other field/program. NEVER certifies exactness.
//!
//! RPC=<ordinary rpc> cargo run --release --example pump_history_probe -- REPORT.json INDEX
use base64::Engine;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use solana_pubkey::Pubkey;
use std::{str::FromStr, time::Instant};
use svmscope::{FixtureEntry, Mutation, Scope};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let path = args.next().ok_or("expected report path")?;
    let index: usize = args.next().ok_or("expected event index")?.parse()?;
    let report: Value = serde_json::from_str(&std::fs::read_to_string(path)?)?;
    let event = report["events"]
        .get(index)
        .ok_or("event index out of range")?;
    let sig = event["signature"].as_str().ok_or("signature missing")?;
    // Use only the first event in the transaction for this curve. A later
    // event describes an intermediate state, not a transaction preimage.
    if report["events"].as_array().ok_or("events missing")?[..index]
        .iter()
        .any(|e| e["signature"] == event["signature"] && e["mint"] == event["mint"])
    {
        return Err("choose the first event for this mint in this transaction".into());
    }
    let mint = Pubkey::from_str(event["mint"].as_str().ok_or("mint missing")?)?;
    let program = Pubkey::from_str("6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P")?;
    let (curve, _) = Pubkey::find_program_address(&[b"bonding-curve", mint.as_ref()], &program);
    let curve = curve.to_string();
    let rpc =
        std::env::var("RPC").unwrap_or_else(|_| "https://api.mainnet-beta.solana.com".to_string());
    let scope = Scope::new(rpc).with_reconstruction_budget(0);
    let started = Instant::now();
    eprintln!("loading historical transaction {sig}; current programs and remaining state");
    let mut replay = scope.replay(sig)?;
    replay.warp_to_slot(report["slot"].as_u64().ok_or("slot missing")?);
    replay.warp_to_timestamp(report["block_time"].as_i64().ok_or("block time missing")?);
    let fx = replay.to_fixture()?;
    let mut data = fx
        .entries
        .iter()
        .find_map(|entry| match entry {
            FixtureEntry::Data {
                address,
                owner,
                data_b64,
                ..
            } if address == &curve && owner == &program.to_string() => {
                base64::engine::general_purpose::STANDARD
                    .decode(data_b64)
                    .ok()
            }
            _ => None,
        })
        .ok_or("curve absent from loaded world")?;
    if data.len() < 49 {
        return Err("curve too short for legacy reserve layout".into());
    }
    if data[..8] != Sha256::digest(b"account:BondingCurve")[..8] {
        return Err("account discriminator does not identify a BondingCurve".into());
    }
    for (offset, field) in [
        (8, "virtual_token"),
        (16, "virtual_sol"),
        (24, "real_token"),
        (32, "real_sol"),
    ] {
        let value = event["candidate_pre"][field]
            .as_u64()
            .ok_or("reserve missing")?;
        data[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
    }
    let baseline = replay.run()?.result;
    let patched = replay
        .simulate(&[Mutation::data(curve.clone(), data)])?
        .result;
    println!(
        "{}",
        serde_json::to_string_pretty(&json!({
            "status": "experimental_partial_fields_only",
            "signature": sig,
            "slot": report["slot"],
            "curve": curve,
            "patched_fields": ["virtual_token_reserves", "virtual_sol_reserves", "real_token_reserves", "real_sol_reserves"],
            "onchain": {"success": replay.recorded().map(|r| r.success), "compute_units": replay.recorded().and_then(|r| r.compute_units)},
            "baseline": baseline,
            "event_reserves": patched,
            "elapsed_seconds": started.elapsed().as_secs_f64(),
            "warning": "Other bytes and programs are current. A matching outcome does not establish exact state or arbitrary-slot support."
        }))?
    );
    Ok(())
}
