//! Build the recorder's seed list: the accounts the busiest programs' recent
//! transactions share. A pool, market, vault or authority shows up in many
//! of a program's transactions; a user's wallet or token account in one. So
//! the accounts that appear in at least `MIN_SHARED` of a program's last
//! `PER_PROGRAM` transactions are the ones worth recording from day one.
//!
//!   RPC=<url> cargo run --release --example seed_list -- [-o seeds/mainnet.txt] [--max <n>]
//!
//! Writes one address per line, grouped under a `# <program name>` comment,
//! most-shared first, capped at `--max` (default 1000). Programs whose ids
//! yield no signatures are skipped and named on stderr.

use {
    solana_client::rpc_request::RpcRequest,
    std::collections::{BTreeMap, BTreeSet},
    svmscope::Scope,
};

/// The programs whose neighbourhoods to seed. Any id that returns no
/// signatures is skipped, so a wrong or retired id costs nothing.
const PROGRAMS: &[(&str, &str)] = &[
    ("Jupiter v6", "JUP6LkbZbjS1jKKwapdHNy74zcZ3tLUZoi5QNyVTaV4"),
    (
        "Raydium AMM v4",
        "675kPX9MHTjS2zt1qfr1NYHuzeLXfQM9H24wFSUt1Mp8",
    ),
    (
        "Raydium CLMM",
        "CAMMCzo5YL8w4VFF8KVHrK22GGUsp5VTaW7grrKgrWqK",
    ),
    (
        "Raydium CPMM",
        "CPMMoo8L3F4NbTegBCKVNunggL7H1ZpdTHKxQB5qKP1C",
    ),
    (
        "Orca Whirlpool",
        "whirLbMiicVdio4qvUfM5KAg6Ct8VwpYzGff3uctyCc",
    ),
    (
        "Meteora DLMM",
        "LBUZKhRxPF3XUpBCjp4YzTKgLccjZhTSDM9YuVaPwxo",
    ),
    (
        "Meteora DAMM v2",
        "cpamdpZCGKUy5JxQXB4dcpGPiikHawvSWAd6mEn1sGG",
    ),
    ("Pump AMM", "pAMMBay6oceH9fJKBRHGP5D4bD4sWpmSwMn52FMfXEA"),
    ("pump.fun", "6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P"),
    ("Phoenix", "PhoeNiXZ8ByJGLkxNfZRnkUfjvmuYqLR89jjFHGqdXY"),
    ("OpenBook v2", "opnb2LAfJYbRMAHHvqjCwQxanZn7ReEHp1k81EohpZb"),
    ("Kamino Lend", "KLend2g3cP87fffoy8q1mQqGKjrxjC8boSyAYavgmjD"),
    ("Drift", "dRiftyHA39MWdi3vFsF8f2KBHTDGm5vV2bbMdA5A1WZ"),
];

const PER_PROGRAM: usize = 40;
const MIN_SHARED: usize = 2;

/// Every account a transaction touched, minus the programs it invoked and
/// sysvars: what the recorder would need to make it exact.
fn touched_accounts(scope: &Scope, signature: &str) -> Vec<String> {
    let mut tx = serde_json::Value::Null;
    for attempt in 0..3 {
        match scope.client().send(
            RpcRequest::GetTransaction,
            serde_json::json!([signature, { "encoding": "json", "maxSupportedTransactionVersion": 0 }]),
        ) {
            Ok(v) => {
                tx = v;
                break;
            }
            Err(e) if attempt == 2 => eprintln!("getTransaction {}: {e}", &signature[..12]),
            Err(_) => std::thread::sleep(std::time::Duration::from_millis(500 << attempt)),
        }
    }
    if tx.is_null() {
        return Vec::new();
    }
    let msg = &tx["transaction"]["message"];
    let mut keys: Vec<String> = msg["accountKeys"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|k| k.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    for side in ["writable", "readonly"] {
        if let Some(extra) = tx["meta"]["loadedAddresses"][side].as_array() {
            keys.extend(extra.iter().filter_map(|k| k.as_str().map(String::from)));
        }
    }
    let mut programs = BTreeSet::new();
    let mut all_ixs: Vec<serde_json::Value> =
        msg["instructions"].as_array().cloned().unwrap_or_default();
    for group in tx["meta"]["innerInstructions"]
        .as_array()
        .cloned()
        .unwrap_or_default()
    {
        all_ixs.extend(
            group["instructions"]
                .as_array()
                .cloned()
                .unwrap_or_default(),
        );
    }
    for ix in &all_ixs {
        if let Some(i) = ix["programIdIndex"].as_u64() {
            if let Some(k) = keys.get(i as usize) {
                programs.insert(k.clone());
            }
        }
    }
    keys.retain(|k| !programs.contains(k) && !k.starts_with("Sysvar"));
    keys.sort();
    keys.dedup();
    keys
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let out = args
        .iter()
        .position(|a| a == "-o")
        .and_then(|i| args.get(i + 1).cloned())
        .unwrap_or_else(|| "seeds/mainnet.txt".to_string());
    let max: usize = args
        .iter()
        .position(|a| a == "--max")
        .and_then(|i| args.get(i + 1))
        .and_then(|v| v.parse().ok())
        .unwrap_or(1000);
    let rpc =
        std::env::var("RPC").unwrap_or_else(|_| "https://api.mainnet-beta.solana.com".to_string());
    let scope = Scope::new(&rpc);

    let mut sections: Vec<(String, Vec<(String, usize)>)> = Vec::new();
    let mut seen_global: BTreeSet<String> = BTreeSet::new();
    for (label, program) in PROGRAMS {
        let sigs = match scope.signatures(program, PER_PROGRAM * 2) {
            Ok(s) if !s.is_empty() => s,
            Ok(_) => {
                eprintln!("{label}: no signatures, skipped");
                continue;
            }
            Err(e) => {
                eprintln!("{label}: {e}, skipped");
                continue;
            }
        };
        let mut count: BTreeMap<String, usize> = BTreeMap::new();
        let mut fetched = 0;
        for s in sigs.iter().filter(|s| !s.err) {
            if fetched >= PER_PROGRAM {
                break;
            }
            let keys = touched_accounts(&scope, &s.signature);
            if keys.is_empty() {
                continue;
            }
            fetched += 1;
            for k in keys {
                *count.entry(k).or_default() += 1;
            }
        }
        let mut shared: Vec<(String, usize)> = count
            .into_iter()
            .filter(|(k, n)| *n >= MIN_SHARED && !seen_global.contains(k))
            .collect();
        shared.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        for (k, _) in &shared {
            seen_global.insert(k.clone());
        }
        eprintln!(
            "{label}: {fetched} transactions, {} shared accounts",
            shared.len()
        );
        sections.push((label.to_string(), shared));
    }

    // Cap: take the most-shared accounts across programs, round-robin so no
    // single busy program crowds the others out.
    let mut budget = max;
    let mut text = String::from(
        "# svmscope recorder seeds: accounts the busiest programs' recent transactions share.\n\
         # Generated by `cargo run --example seed_list`; one address per line.\n",
    );
    let mut cursors = vec![0usize; sections.len()];
    let mut picked: Vec<BTreeSet<String>> = vec![BTreeSet::new(); sections.len()];
    while budget > 0 {
        let mut progressed = false;
        for (i, (_, shared)) in sections.iter().enumerate() {
            if budget == 0 {
                break;
            }
            if let Some((k, _)) = shared.get(cursors[i]) {
                picked[i].insert(k.clone());
                cursors[i] += 1;
                budget -= 1;
                progressed = true;
            }
        }
        if !progressed {
            break;
        }
    }
    let mut total = 0;
    for (i, (label, shared)) in sections.iter().enumerate() {
        let chosen: Vec<&(String, usize)> = shared
            .iter()
            .filter(|(k, _)| picked[i].contains(k))
            .collect();
        if chosen.is_empty() {
            continue;
        }
        text.push_str(&format!("\n# {label} ({} accounts)\n", chosen.len()));
        for (k, n) in chosen {
            text.push_str(&format!("{k}  # in {n} of the last {PER_PROGRAM}\n"));
            total += 1;
        }
    }
    if let Some(dir) = std::path::Path::new(&out).parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(&out, text)?;
    println!("wrote {total} seed accounts to {out}");
    Ok(())
}
