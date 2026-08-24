//! svmscope library: decode, reconstruct, replay, and mutate Solana transactions.
//!
//! Shared by the CLI (`main.rs`) and the web server (`bin/server.rs`).

pub mod api;
pub mod compute;
pub mod cpi_tree;
pub mod decode;
pub mod fixture;
pub mod diffs;
pub mod replay;
pub mod state;
pub mod utils;
pub mod idl;

use serde::Serialize;
use serde_json::json;
use solana_client::rpc_client::RpcClient;
use solana_client::rpc_request::RpcRequest;

/// The full analysis of a transaction — the payload the CLI prints and the API serves.
#[derive(Serialize)]
pub struct Analysis {
    pub overview: Overview,
    pub cpi_tree: Vec<cpi_tree::CpiEntry>,
    pub balance_change: Vec<diffs::BalanceChange>,
    pub token_change: Vec<diffs::TokenChange>,
    pub compute: Vec<compute::CuUsage>,
    /// Local replay is opt-in (it's the slow, drift-prone part) — `analyze` leaves
    /// this `None` and the client runs it on demand via `run_replay` / `/replay`.
    pub replay: Option<replay::ReplayResult>,
    /// The transaction's accounts, with decoded fields where the layout is known.
    /// Drives the what-if editor: pick an account, edit named fields.
    pub accounts: Vec<decode::AccountInfo>,
}

/// Headline facts about the transaction as it actually ran on-chain.
#[derive(Serialize)]
pub struct Overview {
    /// The transaction succeeded on-chain (meta.err is null).
    pub success: bool,
    /// Fee paid, in lamports.
    pub fee: u64,
    /// Slot the transaction landed in, if reported.
    pub slot: Option<u64>,
    /// On-chain compute units consumed, if reported.
    pub compute_units: Option<u64>,
    /// Top-level programs invoked, in order (the instruction programs).
    pub top_programs: Vec<String>,
}

fn build_overview(tx: &serde_json::Value, cpi_tree: &[cpi_tree::CpiEntry]) -> Overview {
    let top_programs = cpi_tree
        .iter()
        .filter(|e| e.stack_height == 1)
        .map(|e| e.program.clone())
        .collect();
    Overview {
        success: tx["meta"]["err"].is_null(),
        fee: tx["meta"]["fee"].as_u64().unwrap_or(0),
        slot: tx["slot"].as_u64(),
        compute_units: tx["meta"]["computeUnitsConsumed"].as_u64(),
        top_programs,
    }
}

/// Fetch a transaction, decode it, and replay it — bundled into one `Analysis`.
pub fn analyze(client: &RpcClient, signature: &str) -> Result<Analysis, String> {
    let tx: serde_json::Value = client
        .send(
            RpcRequest::GetTransaction,
            json!([signature, { "encoding": "json", "maxSupportedTransactionVersion": 0 }]),
        )
        .map_err(|e| format!("RPC error: {e}"))?;

    if tx.is_null() {
        return Err(format!("transaction not found: {signature}"));
    }

    let account_keys = utils::resolve_account_keys(&tx);

    let cpi_tree = cpi_tree::build_cpi_tree(&tx);
    Ok(Analysis {
        overview: build_overview(&tx, &cpi_tree),
        cpi_tree,
        balance_change: diffs::account_diffs(&tx),
        token_change: diffs::token_diffs(&tx),
        compute: compute::cu_per_program(&tx),
        replay: None, // run on demand — see run_replay
        accounts: decode::describe_accounts(client, &account_keys),
    })
}

/// Replay a transaction against current on-chain state — the opt-in step that
/// `analyze` no longer runs automatically.
pub fn run_replay(client: &RpcClient, signature: &str) -> Result<replay::ReplayResult, String> {
    let tx: serde_json::Value = client
        .send(
            RpcRequest::GetTransaction,
            json!([signature, { "encoding": "json", "maxSupportedTransactionVersion": 0 }]),
        )
        .map_err(|e| format!("RPC error: {e}"))?;
    if tx.is_null() {
        return Err(format!("transaction not found: {signature}"));
    }
    let account_keys = utils::resolve_account_keys(&tx);
    let pre = replay::PreState::from_meta(&tx, &account_keys);
    Ok(replay::replay_transaction(client, signature, &account_keys, tx["slot"].as_u64(), &pre))
}

/// Replay a transaction after applying what-if mutations, returning just the result.
pub fn simulate(
    client: &RpcClient,
    signature: &str,
    mutations: &[replay::Mutation],
) -> Result<replay::ReplayResult, String> {
    let tx: serde_json::Value = client
        .send(
            RpcRequest::GetTransaction,
            json!([signature, { "encoding": "json", "maxSupportedTransactionVersion": 0 }]),
        )
        .map_err(|e| format!("RPC error: {e}"))?;

    if tx.is_null() {
        return Err(format!("transaction not found: {signature}"));
    }

    let account_keys = utils::resolve_account_keys(&tx);
    let pre = replay::PreState::from_meta(&tx, &account_keys);
    Ok(replay::mutate_and_replay(client, signature, &account_keys, mutations, tx["slot"].as_u64(), &pre))
}

/// Run a suite of test scenarios against one transaction. State is fetched once
/// and every scenario replays against a fresh copy — the core of the tester.
pub fn simulate_suite(
    client: &RpcClient,
    signature: &str,
    scenarios: Vec<replay::ScenarioSpec>,
) -> Result<Vec<replay::ScenarioOutcome>, String> {
    let tx: serde_json::Value = client
        .send(
            RpcRequest::GetTransaction,
            json!([signature, { "encoding": "json", "maxSupportedTransactionVersion": 0 }]),
        )
        .map_err(|e| format!("RPC error: {e}"))?;
    if tx.is_null() {
        return Err(format!("transaction not found: {signature}"));
    }
    let account_keys = utils::resolve_account_keys(&tx);
    let pre = replay::PreState::from_meta(&tx, &account_keys);
    let ctx = replay::build_context(client, signature, &account_keys, tx["slot"].as_u64(), &pre);
    Ok(replay::run_suite(&ctx, &scenarios))
}

/// Capture a transaction's full state into a portable, self-contained fixture.
/// Freeze once, then replay deterministically forever with no RPC.
pub fn capture_fixture(client: &RpcClient, signature: &str) -> Result<fixture::Fixture, String> {
    let tx: serde_json::Value = client
        .send(
            RpcRequest::GetTransaction,
            json!([signature, { "encoding": "json", "maxSupportedTransactionVersion": 0 }]),
        )
        .map_err(|e| format!("RPC error: {e}"))?;
    if tx.is_null() {
        return Err(format!("transaction not found: {signature}"));
    }
    let slot = tx["slot"].as_u64();
    let account_keys = utils::resolve_account_keys(&tx);
    let pre = replay::PreState::from_meta(&tx, &account_keys);
    let ctx = replay::build_context(client, signature, &account_keys, slot, &pre);
    Ok(ctx.to_fixture(signature, slot))
}

/// Run a scenario suite against a frozen fixture — deterministic, offline, CI-safe.
pub fn run_fixture_suite(
    fx: &fixture::Fixture,
    scenarios: Vec<replay::ScenarioSpec>,
) -> Result<Vec<replay::ScenarioOutcome>, String> {
    let ctx = replay::ReplayContext::from_fixture(fx)?;
    Ok(replay::run_suite(&ctx, &scenarios))
}
