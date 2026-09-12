//! Fidelity sweep: the number behind "exact replay of any recent transaction,
//! free". Records the neighbourhood of a few busy programs for a while on the
//! public node, then replays transactions from inside the covered window at
//! their own slot, on the free tier, and compares each to the chain's own
//! outcome and compute units. Prints a per-program table and one line to
//! quote; optionally writes the raw rows as JSON.
//!
//!   RPC=<replay rpc> RECORD_RPC=<public rpc> RECORD_DIR=<dir> \
//!     cargo run --release --example fidelity_sweep -- \
//!       [--record <secs>] [--watch-txs <n>] [--per <n>] [--json <path>]
//!
//! `--record` runs the recorder first (default 0: sweep an existing store).
//! `--watch-txs` is how many recent transactions per program seed the watched
//! set (default 8); `--per` how many transactions per program to replay
//! (default 4). Anything the recorder never covered is skipped, not counted:
//! the sweep measures the promise, which starts where coverage starts.

use {
    solana_client::rpc_request::RpcRequest,
    std::{
        collections::{BTreeMap, BTreeSet},
        time::Instant,
    },
    svmscope::{
        records::{poll_once, LogStore, StateStore},
        Provenance, Scope,
    },
};

const PROGRAMS: &[(&str, &str)] = &[
    ("Jupiter v6", "JUP6LkbZbjS1jKKwapdHNy74zcZ3tLUZoi5QNyVTaV4"),
    (
        "Raydium AMM v4",
        "675kPX9MHTjS2zt1qfr1NYHuzeLXfQM9H24wFSUt1Mp8",
    ),
    (
        "Orca Whirlpool",
        "whirLbMiicVdio4qvUfM5KAg6Ct8VwpYzGff3uctyCc",
    ),
    (
        "Meteora DLMM",
        "LBUZKhRxPF3XUpBCjp4YzTKgLccjZhTSDM9YuVaPwxo",
    ),
    ("Pump AMM", "pAMMBay6oceH9fJKBRHGP5D4bD4sWpmSwMn52FMfXEA"),
];

/// Slots after coverage start that a target must clear, so the first polling
/// rounds have seen every watched account at least once.
const MARGIN_SLOTS: u64 = 30;

#[derive(Default)]
struct Opts {
    record_secs: u64,
    watch_txs: usize,
    per: usize,
    json: Option<String>,
}

fn parse_opts() -> Opts {
    let mut o = Opts {
        record_secs: 0,
        watch_txs: 8,
        per: 4,
        json: None,
    };
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        let mut val = || args.next().unwrap_or_default();
        match a.as_str() {
            "--record" => o.record_secs = val().parse().unwrap_or(0),
            "--watch-txs" => o.watch_txs = val().parse().unwrap_or(8),
            "--per" => o.per = val().parse().unwrap_or(4),
            "--json" => o.json = Some(val()),
            other => eprintln!("ignoring unknown argument {other}"),
        }
    }
    o
}

/// Every data account a transaction touched: its static keys plus the
/// lookup-table loaded ones, minus the programs it invoked and sysvars.
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
    if let Some(ixs) = msg["instructions"].as_array() {
        for ix in ixs {
            if let Some(i) = ix["programIdIndex"].as_u64() {
                if let Some(k) = keys.get(i as usize) {
                    programs.insert(k.clone());
                }
            }
        }
    }
    if let Some(inner) = tx["meta"]["innerInstructions"].as_array() {
        for group in inner {
            if let Some(ixs) = group["instructions"].as_array() {
                for ix in ixs {
                    if let Some(i) = ix["programIdIndex"].as_u64() {
                        if let Some(k) = keys.get(i as usize) {
                            programs.insert(k.clone());
                        }
                    }
                }
            }
        }
    }
    keys.retain(|k| !programs.contains(k) && !k.starts_with("Sysvar"));
    keys.sort();
    keys.dedup();
    keys
}

/// The address with the most data among `addresses`, if any exists.
fn largest_account(scope: &Scope, addresses: &[String]) -> Option<String> {
    use std::str::FromStr;
    let keys: Vec<solana_pubkey::Pubkey> = addresses
        .iter()
        .filter_map(|a| solana_pubkey::Pubkey::from_str(a).ok())
        .collect();
    let mut best: Option<(usize, String)> = None;
    for chunk in keys.chunks(100) {
        let accounts = scope.client().get_multiple_accounts(chunk).ok()?;
        for (key, acct) in chunk.iter().zip(accounts) {
            let Some(acct) = acct else { continue };
            if acct.executable {
                continue;
            }
            let len = acct.data.len();
            if best.as_ref().is_none_or(|(l, _)| len > *l) {
                best = Some((len, key.to_string()));
            }
        }
    }
    best.map(|(_, k)| k)
}

/// Record the neighbourhood of every seed program for `secs` seconds.
///
/// Returns, per program, candidate target signatures: transactions of the
/// program's hottest watched account (the pool or market its recent
/// transactions share) that landed while the recorder was running, fetched
/// two thirds of the way through so the window extends past them on both
/// sides.
fn record(
    scope: &Scope,
    record_rpc: &str,
    store: &LogStore,
    opts: &Opts,
) -> Vec<(&'static str, Vec<svmscope::SigInfo>)> {
    let mut watched = BTreeSet::new();
    let mut hottest: Vec<(&'static str, String)> = Vec::new();
    for (label, program) in PROGRAMS {
        let mut count: BTreeMap<String, usize> = BTreeMap::new();
        let sigs = match scope.signatures(program, opts.watch_txs * 2) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("{label}: signatures failed: {e}");
                continue;
            }
        };
        let mut seen = 0;
        for s in sigs.iter().filter(|s| !s.err) {
            if seen >= opts.watch_txs {
                break;
            }
            let keys = touched_accounts(scope, &s.signature);
            if keys.is_empty() {
                continue;
            }
            seen += 1;
            for k in &keys {
                *count.entry(k.clone()).or_default() += 1;
            }
            watched.extend(keys);
        }
        // The anchor: an account several of the program's transactions
        // share, but not all of them (mints, tip guards and fee accounts
        // are in every one and would give unrelated targets), preferring
        // the largest: pools and markets carry kilobytes, mints and fee
        // accounts almost nothing.
        let shared: Vec<String> = count
            .iter()
            .filter(|(_, n)| **n >= 2 && **n < seen.max(2))
            .map(|(k, _)| k.clone())
            .collect();
        let pick = largest_account(scope, &shared).or_else(|| {
            count
                .iter()
                .max_by_key(|(_, n)| **n)
                .map(|(k, _)| k.clone())
        });
        if let Some(hot) = pick {
            eprintln!(
                "{label}: anchor {hot} ({}/{seen} seed txs)",
                count.get(&hot).copied().unwrap_or(0)
            );
            hottest.push((label, hot));
        } else {
            eprintln!("{label}: no anchor ({seen} seed transactions fetched)");
        }
    }
    // The targets will be the anchors' next transactions, which touch the
    // anchors' neighbourhoods: watch what their recent transactions touched,
    // not only what the programs' did.
    for (label, hot) in &hottest {
        let sigs = scope
            .signatures(hot, opts.watch_txs * 2)
            .unwrap_or_default();
        let before = watched.len();
        for s in sigs.iter().filter(|s| !s.err).take(opts.watch_txs) {
            watched.extend(touched_accounts(scope, &s.signature));
        }
        eprintln!(
            "{label}: {} accounts from the anchor's neighbourhood",
            watched.len() - before
        );
    }
    for a in &watched {
        if let Err(e) = store.watch(a) {
            eprintln!("watch {a}: {e}");
        }
    }
    let list = store.watched().unwrap_or_default();
    eprintln!(
        "recording {} accounts for {}s on {record_rpc}",
        list.len(),
        opts.record_secs
    );
    let client = solana_client::rpc_client::RpcClient::new(record_rpc.to_string());
    let start = Instant::now();
    let mut versions = 0;
    let mut candidates: Vec<(&'static str, Vec<svmscope::SigInfo>)> = Vec::new();
    let pick_at = opts.record_secs * 2 / 3;
    while start.elapsed().as_secs() < opts.record_secs {
        match poll_once(&client, store, &list) {
            Ok(n) => versions += n,
            Err(e) => eprintln!("poll: {e}"),
        }
        if candidates.is_empty() && start.elapsed().as_secs() >= pick_at {
            let floor = store
                .covered_from()
                .ok()
                .flatten()
                .map(|f| f + MARGIN_SLOTS)
                .unwrap_or(u64::MAX);
            for (label, hot) in &hottest {
                let sigs = scope.signatures(hot, opts.per * 10).unwrap_or_default();
                let inside: Vec<svmscope::SigInfo> = sigs
                    .into_iter()
                    .filter(|s| s.slot.is_some_and(|slot| slot > floor))
                    .collect();
                eprintln!(
                    "{label}: {} candidate targets inside the window",
                    inside.len()
                );
                candidates.push((label, inside));
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(2000));
    }
    eprintln!(
        "recorded {versions} versions, {:.1} MB on disk",
        store.size_bytes() as f64 / 1e6
    );
    candidates
}

#[derive(Default, serde::Serialize)]
struct Tally {
    n: usize,
    /// Free-tier replay and chain agree on success/failure.
    outcome_match: usize,
    /// Free-tier compute units equal the chain's exactly.
    cu_exact: usize,
    /// The current-state tier also agreed on the outcome (the baseline).
    current_match: usize,
    /// Every data account came from a recording or the transaction's own
    /// metadata, and the outcome and compute units match: the promise, kept.
    fully_covered: usize,
    accounts: BTreeMap<String, usize>,
    seconds: Vec<f64>,
    cu_delta: Vec<i64>,
}

#[derive(serde::Serialize)]
struct Row {
    program: String,
    signature: String,
    slot: u64,
    onchain_success: bool,
    onchain_cu: Option<u64>,
    current_success: bool,
    free_success: bool,
    free_cu: u64,
    free_error: Option<String>,
    seconds: f64,
    accounts: BTreeMap<String, usize>,
}

fn provenance_label(p: &Provenance, is_program: bool) -> &'static str {
    if is_program {
        return "program";
    }
    match p {
        Provenance::Program {
            upgraded_since: Some(true),
        } => "program-upgraded",
        Provenance::Program { .. } => "program",
        Provenance::Unchanged { .. } => "unchanged",
        Provenance::Recorded { .. } => "recorded",
        Provenance::MetadataRewind => "metadata",
        Provenance::Reconstructed { exact: true, .. } => "reconstructed",
        Provenance::Reconstructed { exact: false, .. } => "reconstructed~",
        Provenance::HistoricalArchive => "archive",
        Provenance::CurrentRpc => "current",
        Provenance::Fixture => "fixture",
    }
}

fn median(v: &[f64]) -> f64 {
    if v.is_empty() {
        return 0.0;
    }
    let mut s = v.to_vec();
    s.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    s[s.len() / 2]
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let opts = parse_opts();
    let public = "https://api.mainnet-beta.solana.com".to_string();
    let rpc = std::env::var("RPC").unwrap_or_else(|_| public.clone());
    let record_rpc = std::env::var("RECORD_RPC").unwrap_or(public);
    let dir = std::env::var("RECORD_DIR").unwrap_or_else(|_| "target/records".to_string());
    let store = std::sync::Arc::new(LogStore::open(&dir)?);
    let scope = Scope::new(&rpc)
        .with_reconstruction_budget(8)
        .with_records(store.clone());

    let mut candidates = if opts.record_secs > 0 {
        record(&scope, &record_rpc, &store, &opts)
    } else {
        Vec::new()
    };
    let Some(from) = store.covered_from()? else {
        eprintln!("the store at {dir} has no coverage yet; run with --record <secs>");
        return Ok(());
    };
    let floor = from + MARGIN_SLOTS;
    eprintln!("coverage starts at slot {from}; targets must land after {floor}");
    if candidates.is_empty() {
        // Sweeping an existing store: the programs' newest signatures that
        // the window still covers.
        for (label, program) in PROGRAMS {
            match scope.signatures(program, 1000) {
                Ok(s) => candidates.push((label, s)),
                Err(e) => println!("{label}: signatures failed: {e}"),
            }
        }
    }

    let mut rows: Vec<Row> = Vec::new();
    let mut tallies: BTreeMap<String, Tally> = BTreeMap::new();
    let mut replayed: BTreeSet<String> = BTreeSet::new();
    for (label, sigs) in candidates {
        let mut taken = 0;
        for s in sigs {
            if taken >= opts.per {
                break;
            }
            let Some(slot) = s.slot else { continue };
            if slot <= floor || !store.covers(slot).unwrap_or(false) {
                continue;
            }
            // A route through several programs shows up under each; count
            // it once, under the first.
            if !replayed.insert(s.signature.clone()) {
                continue;
            }
            let short = &s.signature[..12];
            let started = Instant::now();
            let recon = match scope.replay_at_slot(&s.signature) {
                Ok(r) => r,
                Err(e) => {
                    println!("{label} {short}: fetch error: {e}");
                    continue;
                }
            };
            let Some(rec) = recon.recorded().cloned() else {
                continue;
            };
            let free = recon.run()?;
            let seconds = started.elapsed().as_secs_f64();
            let current = scope.replay(&s.signature).and_then(|r| r.run())?;
            taken += 1;

            let cert = recon.certificate();
            let mut accounts: BTreeMap<String, usize> = BTreeMap::new();
            for a in &cert.accounts {
                *accounts
                    .entry(provenance_label(&a.source, a.is_program).to_string())
                    .or_default() += 1;
            }
            let outcome = free.result.success == rec.success;
            let cu_exact = rec.compute_units == Some(free.result.compute_units);
            let t = tallies.entry(label.to_string()).or_default();
            t.n += 1;
            t.outcome_match += usize::from(outcome);
            t.cu_exact += usize::from(cu_exact);
            let no_current = accounts.get("current").copied().unwrap_or(0) == 0
                && accounts.get("reconstructed~").copied().unwrap_or(0) == 0;
            t.fully_covered += usize::from(outcome && cu_exact && no_current);
            t.current_match += usize::from(current.result.success == rec.success);
            t.seconds.push(seconds);
            if let Some(cu) = rec.compute_units {
                t.cu_delta
                    .push(free.result.compute_units as i64 - cu as i64);
            }
            for (k, v) in &accounts {
                *t.accounts.entry(k.clone()).or_default() += v;
            }
            let verdict = match (outcome, cu_exact) {
                (true, true) => "exact".to_string(),
                (true, false) => format!(
                    "outcome ok, cu {} vs {:?}",
                    free.result.compute_units, rec.compute_units
                ),
                (false, _) => format!(
                    "MISS chain={} free={} {}",
                    rec.success,
                    free.result.success,
                    free.result.error_name.clone().unwrap_or_default()
                ),
            };
            let summary: Vec<String> = accounts.iter().map(|(k, v)| format!("{v} {k}")).collect();
            println!(
                "{label} {short} @{slot}: {verdict} · {:.1}s · {}",
                seconds,
                summary.join(", ")
            );
            rows.push(Row {
                program: label.to_string(),
                signature: s.signature.clone(),
                slot,
                onchain_success: rec.success,
                onchain_cu: rec.compute_units,
                current_success: current.result.success,
                free_success: free.result.success,
                free_cu: free.result.compute_units,
                free_error: free.result.error.clone(),
                seconds,
                accounts,
            });
        }
        if taken == 0 {
            println!("{label}: no transaction inside the covered window");
        }
    }

    println!();
    println!("| program | txs | outcome match (free) | outcome match (current) | CU exact | fully covered | median s | accounts recorded / metadata / current |");
    println!("|---|---|---|---|---|---|---|---|");
    let (mut n, mut om, mut cm, mut ce, mut fc) = (0, 0, 0, 0, 0);
    let mut all_seconds = Vec::new();
    for (label, t) in &tallies {
        n += t.n;
        om += t.outcome_match;
        cm += t.current_match;
        ce += t.cu_exact;
        fc += t.fully_covered;
        all_seconds.extend(t.seconds.iter().copied());
        let get = |k: &str| t.accounts.get(k).copied().unwrap_or(0);
        println!(
            "| {label} | {} | {} | {} | {} | {} | {:.1} | {} / {} / {} |",
            t.n,
            t.outcome_match,
            t.current_match,
            t.cu_exact,
            t.fully_covered,
            median(&t.seconds),
            get("recorded"),
            get("metadata"),
            get("current")
        );
    }
    if n > 0 {
        println!();
        println!(
            "{om}/{n} outcomes reproduced at their own slot on the free tier ({cm}/{n} at current state), {ce}/{n} with exact compute units, {fc}/{n} with every account covered, median {:.1} s per replay; store {:.1} MB, coverage from slot {from}",
            median(&all_seconds),
            store.size_bytes() as f64 / 1e6
        );
    }
    if let Some(path) = opts.json {
        std::fs::write(&path, serde_json::to_string_pretty(&rows)?)?;
        eprintln!("wrote {} rows to {path}", rows.len());
    }
    Ok(())
}
