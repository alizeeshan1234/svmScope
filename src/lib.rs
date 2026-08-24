//! svmscope library: decode, reconstruct, replay, and mutate Solana transactions.
//!
//! Shared by the CLI (`main.rs`) and the web server (`bin/server.rs`).

pub mod api;
pub mod compute;
pub mod cpi_tree;
pub mod decode;
pub mod fixture;
pub mod report;
pub mod diffs;
pub mod replay;
pub mod state;
pub mod utils;
pub mod idl;

use serde::Serialize;
use serde_json::json;
use solana_address::Address;
use solana_client::rpc_client::RpcClient;
use solana_client::rpc_request::RpcRequest;
use std::str::FromStr;

/// Resolve a cluster name or explicit RPC URL to an endpoint, so one instance
/// serves every cluster. Precedence: explicit `rpc` URL > `cluster` name > `default`.
///
/// Clusters: `mainnet`, `devnet`, `testnet`, `localnet` (127.0.0.1:8899). A value
/// starting with `http` in either field is used verbatim.
pub fn resolve_rpc(cluster: Option<&str>, rpc: Option<&str>, default: &str) -> String {
    if let Some(u) = rpc {
        if u.starts_with("http") {
            return u.to_string();
        }
    }
    match cluster.map(|c| c.trim().to_ascii_lowercase()).as_deref() {
        None | Some("") => default.to_string(),
        Some("mainnet") | Some("mainnet-beta") | Some("m") => {
            "https://api.mainnet-beta.solana.com".into()
        }
        Some("devnet") | Some("d") => "https://api.devnet.solana.com".into(),
        Some("testnet") | Some("t") => "https://api.testnet.solana.com".into(),
        Some("localnet") | Some("local") | Some("localhost") | Some("l") => {
            "http://127.0.0.1:8899".into()
        }
        Some(other) if other.starts_with("http") => other.to_string(),
        Some(other) => {
            eprintln!("warn: unknown cluster '{other}', using default");
            default.to_string()
        }
    }
}

/// The full analysis of a transaction — the payload the CLI prints and the API serves.
#[derive(Serialize)]
pub struct Analysis {
    /// The resolved transaction signature (echoed back — useful when the input
    /// was an address that we resolved to its latest transaction).
    pub signature: String,
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

/// Accept a transaction signature OR an account/program address. A 32-byte value
/// parses as an address, so we resolve it to its most recent transaction; a
/// 64-byte signature is used as-is. Lets a user paste a program ID and see its
/// latest transaction.
fn resolve_signature(client: &RpcClient, input: &str) -> Result<String, String> {
    let input = input.trim();
    if Address::from_str(input).is_ok() {
        let resp: serde_json::Value = client
            .send(RpcRequest::GetSignaturesForAddress, json!([input, { "limit": 1 }]))
            .map_err(|e| format!("RPC error: {e}"))?;
        return resp
            .as_array()
            .and_then(|a| a.first())
            .and_then(|s| s["signature"].as_str())
            .map(String::from)
            .ok_or_else(|| format!("no transactions found for address {input}"));
    }
    Ok(input.to_string())
}

/// An account/program's on-chain overview — what an explorer's address page shows.
#[derive(Serialize)]
pub struct AccountOverview {
    pub address: String,
    pub exists: bool,
    pub owner: String,
    pub lamports: u64,
    pub executable: bool,
    pub data_len: usize,
    /// Program deployment details, when the address is an executable program.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub program: Option<ProgramInfo>,
    /// The program's name from its on-chain Anchor IDL, if published.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub idl_name: Option<String>,
    /// Decoded fields for a recognized data account (SPL or IDL).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub decoded: Option<decode::DecodedAccount>,
}

#[derive(Serialize)]
pub struct ProgramInfo {
    pub program_data: String,
    pub upgradeable: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub upgrade_authority: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_deployed_slot: Option<u64>,
}

/// Fetch an account's raw state: (owner, lamports, executable, data). None if it
/// doesn't exist.
fn get_account_raw(client: &RpcClient, address: &str) -> Option<(String, u64, bool, Vec<u8>)> {
    use base64::Engine;
    let resp: serde_json::Value = client
        .send(RpcRequest::GetAccountInfo, json!([address, { "encoding": "base64" }]))
        .ok()?;
    let v = &resp["value"];
    if v.is_null() {
        return None;
    }
    let owner = v["owner"].as_str()?.to_string();
    let lamports = v["lamports"].as_u64()?;
    let executable = v["executable"].as_bool().unwrap_or(false);
    let data = v["data"][0]
        .as_str()
        .and_then(|s| base64::engine::general_purpose::STANDARD.decode(s).ok())
        .unwrap_or_default();
    Some((owner, lamports, executable, data))
}

/// Program deployment details from the (upgradeable) loader accounts.
fn program_info(client: &RpcClient, program_data_bytes: &[u8], owner: &str) -> Option<ProgramInfo> {
    const UPGRADEABLE: &str = "BPFLoaderUpgradeab1e11111111111111111111111";
    const LOADER_V2: &str = "BPFLoader2111111111111111111111111111111111";

    if owner == UPGRADEABLE && program_data_bytes.len() >= 36 {
        // Program account: [0..4]=variant, [4..36]=programdata address.
        let pd_bytes: [u8; 32] = program_data_bytes[4..36].try_into().ok()?;
        let pd_addr = Address::from(pd_bytes).to_string();
        if let Some((_, _, _, pd)) = get_account_raw(client, &pd_addr) {
            // ProgramData: [0..4]=variant, [4..12]=slot, [12]=Option tag, [13..45]=authority.
            let slot = pd.get(4..12).map(|s| u64::from_le_bytes(s.try_into().unwrap()));
            let (upgradeable, authority) = if pd.len() >= 45 && pd[12] == 1 {
                let a: [u8; 32] = pd[13..45].try_into().unwrap();
                (true, Some(Address::from(a).to_string()))
            } else {
                (false, None)
            };
            return Some(ProgramInfo { program_data: pd_addr, upgradeable, upgrade_authority: authority, last_deployed_slot: slot });
        }
        return Some(ProgramInfo { program_data: pd_addr, upgradeable: true, upgrade_authority: None, last_deployed_slot: None });
    }
    if owner == LOADER_V2 {
        return Some(ProgramInfo { program_data: String::new(), upgradeable: false, upgrade_authority: None, last_deployed_slot: None });
    }
    None
}

/// An explorer-style overview of any account or program address.
pub fn account_overview(client: &RpcClient, address: &str) -> Result<AccountOverview, String> {
    let address = address.trim();
    if Address::from_str(address).is_err() {
        return Err(format!("not a valid address: {address}"));
    }
    let Some((owner, lamports, executable, data)) = get_account_raw(client, address) else {
        return Ok(AccountOverview {
            address: address.to_string(), exists: false, owner: String::new(),
            lamports: 0, executable: false, data_len: 0, program: None, idl_name: None, decoded: None,
        });
    };

    let mut ov = AccountOverview {
        address: address.to_string(), exists: true, owner: owner.clone(),
        lamports, executable, data_len: data.len(), program: None, idl_name: None, decoded: None,
    };

    if executable {
        ov.program = program_info(client, &data, &owner);
        ov.idl_name = Address::from_str(address).ok().and_then(|a| idl::fetch_idl_json(client, a)).and_then(|idl| {
            idl.get("metadata")
                .and_then(|m| m.get("name"))
                .or_else(|| idl.get("name"))
                .and_then(|n| n.as_str())
                .map(String::from)
        });
    } else {
        // Reuse the decoder for recognized data accounts (SPL / IDL).
        ov.decoded = decode::describe_accounts(client, &[address.to_string()])
            .into_iter()
            .next()
            .and_then(|a| a.decoded);
    }
    Ok(ov)
}

/// One entry in an address's recent transaction history.
#[derive(Serialize)]
pub struct SigInfo {
    pub signature: String,
    pub slot: Option<u64>,
    /// Whether the transaction failed on-chain.
    pub err: bool,
    /// Unix timestamp, when the RPC reports it.
    pub block_time: Option<i64>,
}

/// Recent transactions that touched an account or program — what an explorer
/// shows on an address page. Newest first.
pub fn recent_signatures(client: &RpcClient, address: &str, limit: u64) -> Result<Vec<SigInfo>, String> {
    let address = address.trim();
    if Address::from_str(address).is_err() {
        return Err(format!("not a valid address: {address}"));
    }
    let resp: serde_json::Value = client
        .send(RpcRequest::GetSignaturesForAddress, json!([address, { "limit": limit }]))
        .map_err(|e| format!("RPC error: {e}"))?;
    let arr = resp.as_array().ok_or("unexpected RPC response")?;
    Ok(arr
        .iter()
        .map(|s| SigInfo {
            signature: s["signature"].as_str().unwrap_or_default().to_string(),
            slot: s["slot"].as_u64(),
            err: !s["err"].is_null(),
            block_time: s["blockTime"].as_i64(),
        })
        .collect())
}

/// Fetch a transaction, decode it, and replay it — bundled into one `Analysis`.
/// `input` may be a signature or an account/program address (resolved to its
/// latest transaction).
pub fn analyze(client: &RpcClient, input: &str) -> Result<Analysis, String> {
    let signature = resolve_signature(client, input)?;
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
        signature,
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

/// Pre-flight simulate an **unsigned** transaction (base64 wire bytes) against
/// current on-chain state — "what will this do if I send it now?" This is the
/// primitive a wallet or bot calls before signing. Optional what-if `mutations`
/// let a caller preview the tx against edited state too.
pub fn simulate_preflight(
    client: &RpcClient,
    tx_b64: &str,
    mutations: &[replay::Mutation],
) -> Result<replay::ReplayResult, String> {
    use base64::Engine;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(tx_b64.trim())
        .map_err(|e| format!("bad base64 transaction: {e}"))?;
    let tx: solana_transaction::versioned::VersionedTransaction =
        bincode::deserialize(&bytes).map_err(|e| format!("could not deserialize transaction: {e}"))?;
    let ctx = replay::preflight_context(client, tx);
    Ok(ctx.run(mutations))
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
