//! The crate's front door: [`Scope`] (an RPC-backed client that caches what it
//! fetches) and [`Replay`] (a transaction's reconstructed world, fetched once,
//! replayable any number of times with zero further RPC).
//!
//! ```no_run
//! use svmscope::{Mutation, Scope};
//!
//! let scope = Scope::new("https://api.mainnet-beta.solana.com");
//! let mut replay = scope.replay("<signature>")?;   // all RPC happens here
//! replay.advance_seconds(30 * 86_400);             // +30 days
//! let out = replay.simulate(&[Mutation::lamports("<account>", 0)])?;
//! println!("success: {}", out.result.success);
//! # Ok::<(), svmscope::Error>(())
//! ```
use serde::Serialize;
use serde_json::json;
use solana_address::Address;
use solana_client::rpc_client::RpcClient;
use solana_client::rpc_config::RpcSendTransactionConfig;
use solana_client::rpc_request::RpcRequest;
use solana_transaction::versioned::VersionedTransaction;
use std::collections::HashMap;
use std::str::FromStr;
use std::sync::Mutex;
use std::thread;
use std::time::{Duration, Instant};

use crate::analyze::{
    build_overview, AccountDiff, AccountOverview, Analysis, Explanation, FieldDiff, ProgramInfo,
    SigInfo, SimulationReport,
};
use crate::check::{Check, Scenario};
use crate::error::{Error, Result};
use crate::fixture::Fixture;
use crate::replay::{
    FeatureToggle, Mutation, PreState, ReplayContext, ReplayResult, ScenarioOutcome, TimeTravel,
};
use crate::search::{search_threshold, Threshold};
use crate::{cpi_tree, decode, diffs, idl, ixname, utils, CapturedTransaction};

/// An RPC-backed client with caches. Everything svmscope fetches — transaction
/// JSON, program IDLs — is fetched once per `Scope` and reused, so
/// `analyze(sig)` followed by `replay(sig)` costs one transaction fetch, and
/// repeated simulations cost zero.
pub struct Scope {
    client: RpcClient,

    archive: Option<RpcClient>,

    /// getTransaction (json encoding) responses by signature.
    tx_cache: Mutex<HashMap<String, serde_json::Value>>,
    /// On-chain IDL by program id; `None` = checked, program publishes none.
    idl_cache: Mutex<HashMap<String, Option<serde_json::Value>>>,
}

fn status_is_confirmed(status: &serde_json::Value) -> bool {
    match status
        .get("confirmationStatus")
        .and_then(serde_json::Value::as_str)
    {
        Some("confirmed" | "finalized") => true,
        Some("processed") => false,
        None => status.get("confirmations").is_some_and(|confirmation| {
            confirmation.is_null() || confirmation.as_u64().is_some_and(|count| count > 1)
        }),
        Some(_) => false,
    }
}

/// A fetched account, as `(owner, lamports, executable, data)`.
type RawAccount = (String, u64, bool, Vec<u8>);

impl Scope {
    /// A scope talking to the given RPC endpoint.
    pub fn new(rpc_url: impl Into<String>) -> Scope {
        Scope::from_client(RpcClient::new(rpc_url.into()))
    }

    /// A scope over an existing client (custom commitment/timeout config).
    pub fn from_client(client: RpcClient) -> Scope {
        Scope {
            client,
            archive: None,
            tx_cache: Mutex::new(HashMap::new()),
            idl_cache: Mutex::new(HashMap::new()),
        }
    }

    /// Attach an archival RPC endpoint (e.g. Alchemy's Account Archive) so
    /// `replay_at_slot` can fetch account state as of a transaction's slot.
    pub fn with_archive(mut self, archive_url: impl Into<String>) -> Scope {
        self.archive = Some(RpcClient::new(archive_url.into()));
        self
    }

    /// The underlying RPC client — the escape hatch for anything svmscope
    /// doesn't wrap.
    pub fn client(&self) -> &RpcClient {
        &self.client
    }

    /// Accept a transaction signature OR an account/program address. A 32-byte
    /// value parses as an address and resolves to its most recent transaction;
    /// a 64-byte signature is used as-is.
    fn resolve_signature(&self, input: &str) -> Result<String> {
        let input = input.trim();
        if Address::from_str(input).is_ok() {
            let resp: serde_json::Value = self
                .client
                .send(
                    RpcRequest::GetSignaturesForAddress,
                    json!([input, { "limit": 1 }]),
                )
                .map_err(Error::rpc)?;
            return resp
                .as_array()
                .and_then(|a| a.first())
                .and_then(|s| s["signature"].as_str())
                .map(String::from)
                .ok_or_else(|| Error::NoSignatures(input.to_string()));
        }
        Ok(input.to_string())
    }

    /// The transaction's `getTransaction` JSON, fetched once and cached.
    fn fetch_transaction_json(&self, signature: &str) -> Result<Option<serde_json::Value>> {
        if let Some(tx) = self.tx_cache.lock().unwrap().get(signature) {
            return Ok(Some(tx.clone()));
        }

        let tx: serde_json::Value = self
            .client
            .send(
                RpcRequest::GetTransaction,
                json!([
                    signature,
                    {
                        "encoding": "json",
                        "commitment": "confirmed",
                        "maxSupportedTransactionVersion": 0
                    }
                ]),
            )
            .map_err(Error::rpc)?;

        // A recently confirmed transaction may not be indexed yet.
        if tx.is_null() {
            return Ok(None);
        }

        self.tx_cache
            .lock()
            .unwrap()
            .insert(signature.to_string(), tx.clone());

        Ok(Some(tx))
    }

    fn transaction_json(&self, signature: &str) -> Result<serde_json::Value> {
        self.fetch_transaction_json(signature)?
            .ok_or_else(|| Error::TransactionNotFound(signature.to_string()))
    }

    /// A program's on-chain IDL, fetched once and cached (`None` = has none).
    fn idl_for(&self, program: &str) -> Option<serde_json::Value> {
        let mut cache = self.idl_cache.lock().unwrap();
        cache
            .entry(program.to_string())
            .or_insert_with(|| {
                Address::from_str(program)
                    .ok()
                    .and_then(|a| idl::fetch_idl_json(&self.client, a))
            })
            .clone()
    }

    /// Decode a transaction: the full CPI tree with IDL-named instructions,
    /// balance and token diffs, per-program compute units, logs, and every
    /// touched account. `input` may be a signature or an address (resolved to
    /// its latest transaction). Decode only — replay via [`Scope::replay`].
    pub fn analyze(&self, input: &str) -> Result<Analysis> {
        let signature = self.resolve_signature(input)?;
        let tx = self.transaction_json(&signature)?;
        let account_keys = utils::resolve_account_keys(&tx);

        let mut cpi_tree = cpi_tree::build_cpi_tree(&tx);
        // Decode each instruction — name, arguments, and named accounts — from
        // native layouts (always) or the program's on-chain Anchor IDL (cached).
        {
            let mut idls = self.idl_cache.lock().unwrap();
            for e in &mut cpi_tree {
                let (name, args, accounts) = ixname::enrich(
                    &self.client,
                    &mut idls,
                    &e.program,
                    &e.data,
                    &e.account_indexes,
                    &account_keys,
                );
                e.name = name;
                e.args = args;
                e.accounts = accounts;
            }
        }
        Ok(Analysis {
            overview: build_overview(&tx, &cpi_tree, account_keys.len()),
            cpi_tree,
            balance_change: diffs::account_diffs(&tx),
            token_change: diffs::token_diffs(&tx),
            compute: crate::compute::cu_per_program(&tx),
            logs: tx["meta"]["logMessages"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|l| l.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default(),
            replay: None,
            accounts: decode::describe_accounts(&self.client, &account_keys),
            signature,
        })
    }

    /// Reconstruct the transaction's world for local replay — every touched
    /// account, every program ELF, the on-chain outcome, and the IDLs needed to
    /// name errors and fields. **All RPC happens here**; every run of the
    /// returned [`Replay`] is local and free.
    pub fn replay(&self, input: &str) -> Result<Replay> {
        let signature = self.resolve_signature(input)?;
        let tx = self.transaction_json(&signature)?;
        let account_keys = utils::resolve_account_keys(&tx);
        let pre = PreState::from_meta(&tx, &account_keys);
        let mut ctx = crate::replay::build_context(
            &self.client,
            &signature,
            &account_keys,
            tx["slot"].as_u64(),
            &pre,
        )?;
        self.preload_idls(&mut ctx);
        Ok(Replay {
            recorded: Some(OnchainRecord::from_tx_json(&tx)),
            ctx,
            time_travel: TimeTravel::default(),
            fidelity: Fidelity::Current,
        })
    }

    /// Replay a transaction as of its own slot, at the best fidelity the
    /// available data allows — see [`Replay::fidelity`] for what you actually got.
    /// Mutations compose on top: the returned [`Replay`]'s [`Replay::simulate`] /
    /// [`Replay::verify`] apply what-if changes to that same state, so you can
    /// mutate at a specific slot too.
    ///
    /// Two tiers, chosen automatically:
    /// - **Exact** — when an archival endpoint is set via [`Scope::with_archive`]
    ///   *and* it honors the historical `slot` parameter (e.g. Alchemy's Account
    ///   Archive). Accounts and program ELFs are loaded at the true slot.
    /// - **Reconstructed** — the free path, no archive required. Accounts load at
    ///   current state, then SOL and SPL-token balances are rewound to their
    ///   pre-transaction values from the transaction's own metadata, and the clock
    ///   is set to the transaction's slot. Faithful for balances and time; account
    ///   *data* (pool reserves, oracle prices) is still current — [`Fidelity`]
    ///   reports this honestly rather than pretending the replay is exact.
    pub fn replay_at_slot(&self, input: &str) -> Result<Replay> {
        let signature = self.resolve_signature(input)?;
        let tx = self.transaction_json(&signature)?;
        let slot = tx["slot"]
            .as_u64()
            .ok_or_else(|| Error::MalformedRpcResponse("transaction has no slot".into()))?;
        let account_keys = utils::resolve_account_keys(&tx);
        let pre = PreState::from_meta(&tx, &account_keys);

        // Exact tier: only when an archive is set AND actually honors the slot.
        let archive = self
            .archive
            .as_ref()
            .filter(|a| crate::replay::archive_honors_slot(a, slot));

        let (mut ctx, fidelity) = match archive {
            Some(archive) => {
                let ctx = crate::replay::build_context_at_slot(
                    archive,
                    &signature,
                    &account_keys,
                    slot,
                    tx["blockTime"].as_i64(),
                    &pre,
                )?;
                (ctx, Fidelity::Exact { slot })
            }
            None => {
                // Free reconstruction: current accounts + metadata balance rewind.
                let ctx = crate::replay::build_context(
                    &self.client,
                    &signature,
                    &account_keys,
                    Some(slot),
                    &pre,
                )?;
                (ctx, Fidelity::Reconstructed { slot })
            }
        };

        self.preload_idls(&mut ctx);

        let mut replay = Replay {
            recorded: Some(OnchainRecord::from_tx_json(&tx)),
            ctx,
            time_travel: TimeTravel::default(),
            fidelity: Fidelity::Current,
        };
        replay.set_fidelity(fidelity);
        // Anchor the clock to the transaction's slot/time for both tiers.
        replay.warp_to_slot(slot);
        if let Some(ts) = tx["blockTime"].as_i64() {
            replay.warp_to_timestamp(ts);
        }
        Ok(replay)
    }

    /// Reconstruct the world for an **unsigned / not-yet-sent** transaction
    /// (base64 wire bytes) — the pre-flight "what will this do if I send it
    /// now?" primitive. Current on-chain state IS its pre-state, so no drift.
    pub fn preflight(&self, tx_b64: &str) -> Result<Replay> {
        use base64::Engine;
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(tx_b64.trim())
            .map_err(|e| Error::TxDecode(format!("bad base64: {e}")))?;
        let tx: solana_transaction::versioned::VersionedTransaction =
            bincode::deserialize(&bytes).map_err(|e| Error::TxDecode(e.to_string()))?;
        self.preflight_tx(tx)
    }

    /// [`Scope::preflight`] for an already-deserialized transaction.
    pub fn preflight_tx(
        &self,
        tx: solana_transaction::versioned::VersionedTransaction,
    ) -> Result<Replay> {
        let mut ctx = crate::replay::preflight_context(&self.client, tx)?;
        self.preload_idls(&mut ctx);
        Ok(Replay {
            recorded: None,
            ctx,
            time_travel: TimeTravel::default(),
            fidelity: Fidelity::Current,
        })
    }

    /// Freeze a transaction's world into a portable, self-contained [`Fixture`]:
    /// capture once, then replay deterministically forever with no RPC.
    pub fn capture(&self, input: &str) -> Result<Fixture> {
        self.replay(input)?.to_fixture()
    }

    /// Load the IDLs a replay will want — for every loaded program and every
    /// distinct data-account owner — so error names, explanations, and
    /// named-field asserts all resolve without further RPC.
    fn preload_idls(&self, ctx: &mut ReplayContext) {
        for program in ctx.interesting_programs() {
            if let Some(idl) = self.idl_for(&program) {
                ctx.add_idl(program, idl);
            }
        }
    }

    /// An explorer-style overview of any account or program address.
    pub fn account(&self, address: &str) -> Result<AccountOverview> {
        let address = address.trim();
        if Address::from_str(address).is_err() {
            return Err(Error::InvalidAddress(address.to_string()));
        }
        let Some((owner, lamports, executable, data)) = self.account_raw(address)? else {
            return Ok(AccountOverview {
                address: address.to_string(),
                exists: false,
                owner: String::new(),
                lamports: 0,
                executable: false,
                data_len: 0,
                program: None,
                idl_name: None,
                decoded: None,
            });
        };

        let mut ov = AccountOverview {
            address: address.to_string(),
            exists: true,
            owner: owner.clone(),
            lamports,
            executable,
            data_len: data.len(),
            program: None,
            idl_name: None,
            decoded: None,
        };

        if executable {
            ov.program = self.program_info(&data, &owner);
            ov.idl_name = self.idl_for(address).and_then(|idl| {
                idl.get("metadata")
                    .and_then(|m| m.get("name"))
                    .or_else(|| idl.get("name"))
                    .and_then(|n| n.as_str())
                    .map(String::from)
            });
        } else {
            // Reuse the decoder for recognized data accounts (SPL / IDL).
            ov.decoded = decode::describe_accounts(&self.client, &[address.to_string()])
                .into_iter()
                .next()
                .and_then(|a| a.decoded);
        }
        Ok(ov)
    }

    /// Recent transactions that touched an account or program — what an
    /// explorer shows on an address page. Newest first.
    pub fn signatures(&self, address: &str, limit: usize) -> Result<Vec<SigInfo>> {
        let address = address.trim();
        if Address::from_str(address).is_err() {
            return Err(Error::InvalidAddress(address.to_string()));
        }
        let resp: serde_json::Value = self
            .client
            .send(
                RpcRequest::GetSignaturesForAddress,
                json!([address, { "limit": limit }]),
            )
            .map_err(Error::rpc)?;
        let arr = resp.as_array().ok_or_else(|| {
            Error::MalformedRpcResponse("getSignaturesForAddress: not an array".into())
        })?;
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

    /// Decode one account, optionally with a caller-supplied IDL.
    ///
    /// The on-chain IDL is the happy path, but plenty of programs never publish
    /// one — including your own during development. Passing the IDL JSON (from
    /// `target/idl/<program>.json`) gives full named-field decoding anyway.
    pub fn decode_account(
        &self,
        address: &str,
        user_idl: Option<&serde_json::Value>,
    ) -> Result<decode::AccountInfo> {
        let address = address.trim();
        if Address::from_str(address).is_err() {
            return Err(Error::InvalidAddress(address.to_string()));
        }
        let mut info = decode::describe_accounts(&self.client, &[address.to_string()])
            .into_iter()
            .next()
            .ok_or_else(|| Error::AccountNotFound(address.to_string()))?;

        // A supplied IDL wins: it's authoritative for this program, and it beats
        // the inferred layout we may have fallen back to.
        if let Some(idl) = user_idl {
            if let Ok(Some((_, _, _, bytes))) = self.account_raw(address) {
                if let Some(d) = idl::decode_with_idl(idl, &bytes) {
                    info.decoded = Some(d);
                }
            }
        }
        Ok(info)
    }

    /// The instructions a program exposes, from its on-chain IDL — the input to
    /// a transaction builder.
    pub fn program_instructions(&self, program_id: &str) -> Result<Vec<idl::IdlInstruction>> {
        let program_id = program_id.trim();
        if Address::from_str(program_id).is_err() {
            return Err(Error::InvalidAddress(program_id.to_string()));
        }
        let idl = self
            .idl_for(program_id)
            .ok_or_else(|| Error::NoIdl(program_id.to_string()))?;
        Ok(idl::instructions(&idl))
    }

    /// A program's complete on-chain IDL, when the program publishes one.
    pub fn program_idl(&self, program_id: &str) -> Result<Option<serde_json::Value>> {
        let program_id = program_id.trim();
        if Address::from_str(program_id).is_err() {
            return Err(Error::InvalidAddress(program_id.to_string()));
        }
        Ok(self.idl_for(program_id))
    }

    /// Fetch an account's raw state: (owner, lamports, executable, data).
    /// `None` if it doesn't exist.
    /// `Ok(None)` means the account genuinely does not exist; `Err` means the
    /// RPC call itself failed. Keeping these distinct stops a network outage from
    /// masquerading as "account not found".
    fn account_raw(&self, address: &str) -> Result<Option<RawAccount>> {
        use base64::Engine;
        let resp: serde_json::Value = self
            .client
            .send(
                RpcRequest::GetAccountInfo,
                json!([address, { "encoding": "base64" }]),
            )
            .map_err(Error::rpc)?;
        let v = &resp["value"];
        if v.is_null() {
            return Ok(None);
        }
        let owner = match v["owner"].as_str() {
            Some(o) => o.to_string(),
            None => return Ok(None),
        };
        let Some(lamports) = v["lamports"].as_u64() else {
            return Ok(None);
        };
        let executable = v["executable"].as_bool().unwrap_or(false);
        let data = v["data"][0]
            .as_str()
            .and_then(|s| base64::engine::general_purpose::STANDARD.decode(s).ok())
            .unwrap_or_default();
        Ok(Some((owner, lamports, executable, data)))
    }

    /// Program deployment details from the (upgradeable) loader accounts.
    fn program_info(&self, program_data_bytes: &[u8], owner: &str) -> Option<ProgramInfo> {
        const UPGRADEABLE: &str = "BPFLoaderUpgradeab1e11111111111111111111111";
        const LOADER_V2: &str = "BPFLoader2111111111111111111111111111111111";

        if owner == UPGRADEABLE && program_data_bytes.len() >= 36 {
            // Program account: [0..4]=variant, [4..36]=programdata address.
            let pd_bytes: [u8; 32] = program_data_bytes[4..36].try_into().ok()?;
            let pd_addr = Address::from(pd_bytes).to_string();
            if let Ok(Some((_, _, _, pd))) = self.account_raw(&pd_addr) {
                // ProgramData: [0..4]=variant, [4..12]=slot, [12]=Option tag, [13..45]=authority.
                let slot = pd
                    .get(4..12)
                    .and_then(|s| s.try_into().ok())
                    .map(u64::from_le_bytes);
                let (upgradeable, authority) = match pd.get(13..45) {
                    Some(a) if pd[12] == 1 => {
                        let a: [u8; 32] = a.try_into().ok()?;
                        (true, Some(Address::from(a).to_string()))
                    }
                    _ => (false, None),
                };
                return Some(ProgramInfo {
                    program_data: pd_addr,
                    upgradeable,
                    upgrade_authority: authority,
                    last_deployed_slot: slot,
                });
            }
            return Some(ProgramInfo {
                program_data: pd_addr,
                upgradeable: true,
                upgrade_authority: None,
                last_deployed_slot: None,
            });
        }
        if owner == LOADER_V2 {
            return Some(ProgramInfo {
                program_data: String::new(),
                upgradeable: false,
                upgrade_authority: None,
                last_deployed_slot: None,
            });
        }
        None
    }

    fn wait_for_transaction(&self, signature: &str) -> Result<serde_json::Value> {
        const TIMEOUT: Duration = Duration::from_secs(20);
        const POLL_INTERVAL: Duration = Duration::from_millis(100);

        let deadline = Instant::now() + TIMEOUT;
        let mut confirmed = false;

        loop {
            if Instant::now() >= deadline {
                return if confirmed {
                    Err(Error::TransactionMetadataUnavailable {
                        signature: signature.to_string(),
                    })
                } else {
                    Err(Error::ConfirmationTimeout {
                        signature: signature.to_string(),
                    })
                };
            }

            if !confirmed {
                let response: serde_json::Value = self
                    .client
                    .send(
                        RpcRequest::GetSignatureStatuses,
                        json!([
                            [signature],
                            {
                                "searchTransactionHistory": true,
                            }
                        ]),
                    )
                    .map_err(Error::rpc)?;

                let statuses = response
                    .get("value")
                    .and_then(serde_json::Value::as_array)
                    .ok_or_else(|| {
                        Error::MalformedRpcResponse(
                            "getSignatureStatuses: missing value array".into(),
                        )
                    })?;

                let status = statuses.first().ok_or_else(|| {
                    Error::MalformedRpcResponse("getSignatureStatuses: empty value array".into())
                })?;

                if !status.is_null() && status_is_confirmed(status) {
                    confirmed = true;
                }
            }

            if confirmed {
                if let Some(tx) = self.fetch_transaction_json(signature)? {
                    return Ok(tx);
                }
            }

            let remaining = deadline.saturating_duration_since(Instant::now());

            if !remaining.is_zero() {
                thread::sleep(POLL_INTERVAL.min(remaining));
            }
        }
    }

    /// Submit a signed transaction while retaining its pre-transaction world
    /// for replay, mutation, and time travel after it lands.
    pub fn send_and_capture(&self, tx: VersionedTransaction) -> Result<CapturedTransaction> {
        let mut replay = self.preflight_tx(tx.clone())?;
        let signature = self
            .client
            .send_transaction_with_config(
                &tx,
                RpcSendTransactionConfig {
                    // A reverting transaction must still land so svmscope can
                    // capture its program failure as data.
                    skip_preflight: true,
                    ..RpcSendTransactionConfig::default()
                },
            )
            .map_err(Error::rpc)?;

        let tx_json = self.wait_for_transaction(&signature.to_string())?;

        replay.set_recorded(OnchainRecord::from_tx_json(&tx_json));

        Ok(CapturedTransaction {
            signature: signature.to_string(),
            replay,
        })
    }
}

/// The transaction's actual on-chain outcome, kept alongside the replay so
/// local results can be compared against what really happened.
#[derive(Debug, Clone, PartialEq, Serialize, serde::Deserialize)]
pub struct OnchainRecord {
    /// Whether the transaction succeeded on-chain.
    pub success: bool,
    /// The on-chain error, as reported by the RPC (JSON-encoded), when it failed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Fee paid on-chain, in lamports.
    pub fee: u64,
    /// Compute units consumed on-chain, if reported.
    pub compute_units: Option<u64>,
    /// Slot the transaction landed in, if reported.
    pub slot: Option<u64>,
    /// Block time (unix seconds), when the RPC reports it.
    pub block_time: Option<i64>,
    /// The program logs the transaction produced on-chain.
    pub logs: Vec<String>,
}

impl OnchainRecord {
    pub(crate) fn from_tx_json(tx: &serde_json::Value) -> OnchainRecord {
        // `getTransaction` may legally return `meta: null`. Treating a null meta
        // as `err == null` would fabricate a success (and let `matches_onchain`
        // compare against it); instead record it as an explicit unknown-outcome
        // failure so nothing downstream reads a phantom success.
        let has_meta = tx["meta"].is_object();
        OnchainRecord {
            success: has_meta && tx["meta"]["err"].is_null(),
            error: if !has_meta {
                Some("transaction metadata unavailable".to_string())
            } else {
                (!tx["meta"]["err"].is_null()).then(|| tx["meta"]["err"].to_string())
            },
            fee: tx["meta"]["fee"].as_u64().unwrap_or(0),
            compute_units: tx["meta"]["computeUnitsConsumed"].as_u64(),
            slot: tx["slot"].as_u64(),
            block_time: tx["blockTime"].as_i64(),
            logs: tx["meta"]["logMessages"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|l| l.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default(),
        }
    }
}

/// How faithful a replay's starting state is to the transaction's real slot —
/// the honest label on every replay, so a convincing-but-drifted run is never
/// mistaken for an exact one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum Fidelity {
    /// Current-state replay ([`Scope::replay`]) — no historical anchoring.
    Current,
    /// Balances rewound to their pre-transaction values from the transaction's
    /// own metadata, clock set to the transaction's slot; account *data* is still
    /// current-state. Free — no archive needed.
    Reconstructed {
        /// The transaction's own slot, that the clock is anchored to.
        slot: u64,
    },
    /// Every account and program ELF loaded at the true slot from an archive.
    Exact {
        /// The transaction's own slot, that state was loaded at.
        slot: u64,
    },
}

impl Fidelity {
    /// A short human label: `current`, `reconstructed@<slot>`, `exact@<slot>`.
    pub fn label(&self) -> String {
        match self {
            Fidelity::Current => "current".to_string(),
            Fidelity::Reconstructed { slot } => format!("reconstructed@{slot}"),
            Fidelity::Exact { slot } => format!("exact@{slot}"),
        }
    }
}

/// Where an account's loaded bytes came from — the honest per-account source.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum Provenance {
    /// A frozen fixture — offline, content-addressed.
    Fixture,
    /// A historical archive, at the transaction's true slot.
    HistoricalArchive,
    /// Reconstructed from the transaction's own metadata (a pre-tx balance, or a
    /// since-closed account rebuilt from `preTokenBalances`).
    MetadataRewind,
    /// Loaded at current state from a normal RPC — may differ from the true slot.
    CurrentRpc,
}

/// One account's provenance within a replay.
#[derive(Debug, Clone, Serialize)]
pub struct AccountProvenance {
    /// The account address.
    pub address: String,
    /// Where its loaded bytes came from.
    pub source: Provenance,
    /// Whether it was loaded as an executable program (its ELF) rather than data.
    pub is_program: bool,
    /// A blake3 hash of the exact bytes loaded, for content addressing.
    pub hash: String,
}

/// An honest report of how faithful a replay's starting state is: the verdict,
/// where every account's bytes came from, which accounts may have drifted from
/// the transaction's true slot, and whether there is a recorded on-chain outcome
/// to check the replay against. Trust is a product feature — svmscope should
/// never silently hand back a convincing but historically inaccurate replay.
#[derive(Debug, Clone, Serialize)]
pub struct FidelityCertificate {
    /// The overall fidelity tier of this replay.
    pub fidelity: Fidelity,
    /// The (possibly warped) clock the replay runs at, human-readable.
    pub clock: String,
    /// Provenance and hash of every loaded account.
    pub accounts: Vec<AccountProvenance>,
    /// Addresses whose bytes are current-state in a historical replay, so their
    /// data may differ from the true slot. Empty for an exact or current replay.
    pub drifted: Vec<String>,
    /// Whether a recorded on-chain outcome exists to verify the replay against.
    pub verifiable: bool,
}

impl FidelityCertificate {
    /// A one-line human summary of the certificate.
    pub fn summary(&self) -> String {
        let n = self.accounts.len();
        let verify = if self.verifiable {
            "verifiable against mainnet"
        } else {
            "no recorded outcome"
        };
        match self.drifted.len() {
            0 => format!("{} · {n} accounts, none drifted · {verify}", self.fidelity.label()),
            d => format!(
                "{} · {n} accounts, {d} may have drifted · {verify}",
                self.fidelity.label()
            ),
        }
    }
}

/// A transaction's reconstructed world — fetched once via [`Scope::replay`],
/// then replayed locally any number of times. Every run builds a pristine SVM,
/// so runs are independent, repeatable, and free.
pub struct Replay {
    ctx: ReplayContext,
    /// What actually happened on-chain (`None` for pre-flight transactions and
    /// fixtures captured before outcomes were recorded).
    recorded: Option<OnchainRecord>,
    time_travel: TimeTravel,
    fidelity: Fidelity,
}

impl Replay {
    pub(crate) fn set_recorded(&mut self, recorded: OnchainRecord) {
        self.recorded = Some(recorded);
    }

    pub(crate) fn set_fidelity(&mut self, fidelity: Fidelity) {
        self.fidelity = fidelity;
    }

    /// How faithful this replay's starting state is to the transaction's slot.
    pub fn fidelity(&self) -> Fidelity {
        self.fidelity
    }

    /// An honest fidelity certificate for this replay: the verdict, per-account
    /// provenance and hashes, which accounts may have drifted from the true slot,
    /// and whether there is a recorded on-chain outcome to verify against.
    pub fn certificate(&self) -> FidelityCertificate {
        let accounts: Vec<AccountProvenance> = self
            .ctx
            .loaded_info()
            .into_iter()
            .map(|i| {
                let source = match self.fidelity {
                    Fidelity::Exact { .. } => Provenance::HistoricalArchive,
                    // A balance-only account (system-owned, no data) is faithfully
                    // rewound from metadata; program ELFs and program-owned data
                    // accounts are still current-state.
                    Fidelity::Reconstructed { .. }
                        if !i.is_program && i.owner_is_system && i.data_len == 0 =>
                    {
                        Provenance::MetadataRewind
                    }
                    Fidelity::Reconstructed { .. } => Provenance::CurrentRpc,
                    Fidelity::Current => Provenance::CurrentRpc,
                };
                AccountProvenance {
                    address: i.address,
                    source,
                    is_program: i.is_program,
                    hash: i.hash,
                }
            })
            .collect();

        // In a historical replay, any account still on current-state bytes is a
        // potential drift point. A plainly-current replay isn't "drifted" — it
        // never claimed to be historical.
        let drifted = if matches!(self.fidelity, Fidelity::Current) {
            Vec::new()
        } else {
            accounts
                .iter()
                .filter(|a| a.source == Provenance::CurrentRpc)
                .map(|a| a.address.clone())
                .collect()
        };

        let verifiable = self
            .recorded
            .as_ref()
            .is_some_and(|r| r.error.as_deref() != Some("transaction metadata unavailable"));

        FidelityCertificate {
            fidelity: self.fidelity,
            clock: self.ctx.describe_clock(),
            accounts,
            drifted,
            verifiable,
        }
    }

    /// Rebuild a replay from a frozen fixture — fully offline, no RPC. A v2
    /// fixture restores the recorded on-chain outcome and captured IDLs too.
    pub fn from_fixture(fx: &Fixture) -> Result<Replay> {
        Ok(Replay {
            ctx: ReplayContext::from_fixture(fx)?,
            recorded: fx.recorded.clone(),
            time_travel: TimeTravel::default(),
            fidelity: Fidelity::Current,
        })
    }

    /// What actually happened on-chain, when known.
    pub fn recorded(&self) -> Option<&OnchainRecord> {
        self.recorded.as_ref()
    }

    // --- time travel ---------------------------------------------------------

    /// Jump forward `n` slots (additive with other jumps).
    pub fn advance_slots(&mut self, n: i64) {
        // Saturating so repeated extreme jumps clamp instead of overflow-panicking
        // in debug builds; the clock application saturates too.
        self.time_travel.slots = Some(self.time_travel.slots.unwrap_or(0).saturating_add(n));
        self.apply_tt();
    }

    /// Jump forward `n` epochs (additive with other jumps).
    pub fn advance_epochs(&mut self, n: i64) {
        self.time_travel.epochs = Some(self.time_travel.epochs.unwrap_or(0).saturating_add(n));
        self.apply_tt();
    }

    /// Jump forward `n` seconds (additive with other jumps) — vesting cliffs,
    /// cooldowns, auction deadlines.
    pub fn advance_seconds(&mut self, n: i64) {
        self.time_travel.seconds = Some(self.time_travel.seconds.unwrap_or(0).saturating_add(n));
        self.apply_tt();
    }

    /// Set the clock's slot outright (wins over relative jumps).
    pub fn warp_to_slot(&mut self, slot: u64) {
        self.time_travel.at_slot = Some(slot);
        self.apply_tt();
    }

    /// Set the clock's epoch outright (wins over relative jumps).
    pub fn warp_to_epoch(&mut self, epoch: u64) {
        self.time_travel.at_epoch = Some(epoch);
        self.apply_tt();
    }

    /// Set the clock's unix timestamp outright (wins over relative jumps).
    pub fn warp_to_timestamp(&mut self, unix_timestamp: i64) {
        self.time_travel.at_unix_timestamp = Some(unix_timestamp);
        self.apply_tt();
    }

    /// Replace the whole clock warp at once (the JSON suite format's shape).
    pub fn set_time_travel(&mut self, tt: TimeTravel) {
        self.time_travel = tt;
        self.apply_tt();
    }

    fn apply_tt(&mut self) {
        self.ctx.set_time_travel(self.time_travel.clone());
    }

    /// A human description of the (possibly warped) clock replays run at,
    /// e.g. "slot 488,863,115 · epoch 1131 · 2026-09-26 14:03 UTC".
    pub fn describe_clock(&self) -> String {
        self.ctx.describe_clock()
    }

    // --- feature gates & IDLs ------------------------------------------------

    /// Flip one runtime feature gate for subsequent runs — replay a transaction
    /// as if a not-yet-live feature were active (or an active one weren't).
    pub fn set_feature(&mut self, id: Address, active: bool) {
        self.ctx.push_feature_toggle(FeatureToggle { id, active });
    }

    /// Replace all feature toggles at once (the JSON suite format's shape).
    pub fn set_features(&mut self, toggles: Vec<FeatureToggle>) {
        self.ctx.set_feature_toggles(toggles);
    }

    /// Register a program's IDL for named-field asserts and error explanations —
    /// for programs that publish nothing on-chain (e.g. your own, in development).
    pub fn add_idl(&mut self, program: impl Into<String>, idl: serde_json::Value) {
        self.ctx.add_idl(program.into(), idl);
    }

    // --- execution -----------------------------------------------------------

    /// Replay the transaction as-is. A reverting transaction is a successful
    /// observation (`result.success == false`), never an `Err`.
    pub fn run(&self) -> Result<Replayed> {
        self.simulate(&[])
    }

    /// Replay after applying what-if `mutations` to a fresh copy of the state.
    pub fn simulate(&self, mutations: &[Mutation]) -> Result<Replayed> {
        let warped = !self.time_travel.is_noop();
        let (mut result, raw_diffs) = self.ctx.run_with_diff(mutations)?;
        let explain = (!result.success)
            .then(|| explain_error(&result, self.ctx.idl_map()))
            .flatten();
        if result.error_name.is_none() {
            result.error_name = explain.as_ref().map(|e| e.title.clone());
        }
        Ok(Replayed {
            diffs: decode_diffs(raw_diffs, self.ctx.idl_map()),
            clock: warped.then(|| self.ctx.describe_clock()),
            explain,
            result,
        })
    }

    // --- counterfactual search -----------------------------------------------

    /// Binary-search a numeric knob for the value at which the outcome flips —
    /// "at what oracle price does this stop succeeding?", "what is the minimum
    /// balance that avoids the revert?". `mutate(v)` builds the mutation(s) that
    /// set the knob to candidate `v`; the search runs over the inclusive range
    /// `[lo, hi]` and returns the boundary (or `None` if the outcome is the same
    /// at both bounds). Every candidate is a fresh, independent replay, so the
    /// search never mutates shared state. Assumes a single crossing.
    ///
    /// ```no_run
    /// # use svmscope::{Mutation, Scope};
    /// # let replay = Scope::new("").replay("")?;
    /// // Lowest fee-payer balance at which the transaction still lands:
    /// let payer = "…".to_string();
    /// let boundary = replay.find_threshold(0, 5_000_000_000, |v| {
    ///     vec![Mutation::lamports(payer.clone(), v)]
    /// })?;
    /// # Ok::<(), svmscope::Error>(())
    /// ```
    pub fn find_threshold(
        &self,
        lo: u64,
        hi: u64,
        mutate: impl Fn(u64) -> Vec<Mutation>,
    ) -> Result<Option<Threshold>> {
        search_threshold(lo, hi, |v| Ok(self.simulate(&mutate(v))?.result.success))
    }

    /// Shrink a set of mutations to a minimal subset that still flips the
    /// outcome — "which of these changes actually caused the difference?".
    ///
    /// "Flips" means the subset's success differs from the un-mutated baseline.
    /// Runs a greedy delta-debugging pass: drop each mutation whose removal keeps
    /// the outcome flipped. Returns the minimal subset (input order preserved),
    /// or an empty vec if the full set doesn't change the baseline outcome at all.
    pub fn minimize_mutations(&self, mutations: &[Mutation]) -> Result<Vec<Mutation>> {
        let baseline = self.run()?.result.success;
        let target = self.simulate(mutations)?.result.success;
        if target == baseline {
            return Ok(Vec::new());
        }
        let mut keep = mutations.to_vec();
        let mut i = 0;
        while i < keep.len() {
            let mut trial = keep.clone();
            trial.remove(i);
            if self.simulate(&trial)?.result.success == target {
                keep = trial; // mutation i wasn't needed to keep the flip
            } else {
                i += 1; // it's load-bearing; keep it and move on
            }
        }
        Ok(keep)
    }

    /// Run a suite of scenarios, each against a fresh copy of the state.
    /// Mutations across the whole suite are validated before anything executes.
    pub fn run_suite(&self, scenarios: &[Scenario]) -> Result<Vec<ScenarioOutcome>> {
        crate::replay::run_suite(&self.ctx, self.recorded.as_ref(), scenarios)
    }

    /// Run one named scenario — mutations plus the checks that must hold —
    /// and report it. Sugar over [`Replay::run_suite`] for the single case.
    pub fn verify(
        &self,
        name: impl Into<String>,
        mutations: &[Mutation],
        checks: &[Check],
    ) -> Result<ScenarioOutcome> {
        let scenario = Scenario {
            name: name.into(),
            mutations: mutations.to_vec(),
            checks: checks.to_vec(),
        };
        let mut outcomes = self.run_suite(std::slice::from_ref(&scenario))?;
        Ok(outcomes.remove(0))
    }

    /// Freeze this world into a portable [`Fixture`] for offline CI replay.
    /// Captures the loaded IDLs and the recorded on-chain outcome, so field
    /// asserts and [`Check::matches_onchain`] work offline too.
    pub fn to_fixture(&self) -> Result<Fixture> {
        let mut fx = self.ctx.to_fixture()?;
        fx.recorded = self.recorded.clone();
        Ok(fx)
    }
}

/// The outcome of one local replay: the result itself, what changed, and — on
/// failure — a plain-language explanation.
#[derive(Debug, Serialize)]
pub struct Replayed {
    /// The replay's outcome (success, error, logs, CU).
    pub result: ReplayResult,
    /// Every account the transaction changed, before → after, with named
    /// fields where the layout (or an IDL) is known.
    pub diffs: Vec<AccountDiff>,
    /// Where the clock was warped to, when time travel was requested.
    pub clock: Option<String>,
    /// The failure in plain language, when the replay failed.
    pub explain: Option<Explanation>,
}

impl Replayed {
    /// The wire shape the HTTP API serves (`SimulationReport`) — same JSON as v0.1.
    pub fn into_report(self) -> SimulationReport {
        SimulationReport {
            replay: self.result,
            clock: self.clock,
            explain: self.explain,
            diffs: self.diffs,
        }
    }
}

/// Turn a program error into a human explanation using the loaded IDLs.
/// (Same logic the v0.1 library ran with live RPC — now resolved offline
/// against the IDLs preloaded into the replay context.)
fn explain_error(
    r: &ReplayResult,
    idls: &HashMap<String, serde_json::Value>,
) -> Option<Explanation> {
    let raw = r.error.as_ref()?;

    // Which program failed? The last "Program <id> failed" line names it.
    let program = r
        .logs
        .iter()
        .rev()
        .find_map(|l| {
            l.strip_prefix("Program ")
                .and_then(|s| s.split(" failed").next())
        })
        .map(|s| s.trim().to_string())
        .filter(|s| Address::from_str(s).is_ok());

    // Anchor prints the resolved error itself — prefer that, it's already human.
    if let Some(line) = r.logs.iter().rev().find(|l| l.contains("Error Message:")) {
        let detail = line
            .split("Error Message:")
            .nth(1)
            .unwrap_or("")
            .trim()
            .to_string();
        let title = line
            .split("Error Code:")
            .nth(1)
            .and_then(|s| s.split('.').next())
            .map(|s| s.trim().to_string())
            .unwrap_or_else(|| "Program error".into());
        return Some(Explanation {
            title,
            detail,
            program,
            raw: raw.clone(),
        });
    }

    // Otherwise resolve the custom code against the program's IDL.
    if let Some(code) = raw
        .split("Custom(")
        .nth(1)
        .and_then(|s| s.split(')').next())
        .and_then(|s| s.parse::<u64>().ok())
    {
        if let Some(e) = program
            .as_ref()
            .and_then(|p| idls.get(p))
            .and_then(|i| idl::error_for_code(i, code))
        {
            return Some(Explanation {
                title: e.name,
                detail: e.msg,
                program,
                raw: raw.clone(),
            });
        }
    }

    // Fall back to a friendly reading of the common runtime errors.
    let (title, detail) = if raw.contains("AccountNotFound") {
        ("Account not found", "An account the transaction needs doesn't exist (an account with zero lamports is treated as deleted).")
    } else if raw.contains("InsufficientFunds") {
        (
            "Insufficient funds",
            "An account didn't have enough lamports for the transfer plus rent.",
        )
    } else if raw.contains("InvalidAddressLookupTableIndex") {
        ("Lookup table index invalid", "The transaction referenced an address lookup table entry that isn't active at this slot.")
    } else {
        (
            "Transaction failed",
            "The program returned an error. See the logs below for the failing instruction.",
        )
    };
    Some(Explanation {
        title: title.into(),
        detail: detail.into(),
        program,
        raw: raw.clone(),
    })
}

/// Decode raw before/after bytes into named field changes, using built-in
/// layouts first and the preloaded IDLs second.
fn decode_diffs(
    raw: Vec<crate::replay::RawAccountDiff>,
    idls: &HashMap<String, serde_json::Value>,
) -> Vec<AccountDiff> {
    raw.into_iter()
        .map(|d| {
            let idl = idls.get(&d.owner);
            let decode_side = |bytes: &[u8]| -> Option<decode::DecodedAccount> {
                decode::decode_bytes(&d.owner, bytes)
                    .or_else(|| idl.and_then(|i| idl::decode_with_idl(i, bytes)))
            };
            let (before, after) = (decode_side(&d.data_before), decode_side(&d.data_after));
            let mut fields = Vec::new();
            if let (Some(b), Some(a)) = (&before, &after) {
                for (fb, fa) in b.fields.iter().zip(a.fields.iter()) {
                    if fb.value != fa.value {
                        fields.push(FieldDiff {
                            name: fa.name.clone(),
                            ty: fa.ty.clone(),
                            before: fb.value.clone(),
                            after: fa.value.clone(),
                        });
                    }
                }
            }
            let raw_data_changed = d.data_before != d.data_after && fields.is_empty();
            AccountDiff {
                address: d.address,
                owner: d.owner,
                lamports_before: d.lamports_before,
                lamports_after: d.lamports_after,
                fields,
                raw_data_changed,
            }
        })
        .collect()
}

#[cfg(test)]
mod wait_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn confirmed_success_is_landed() {
        let status = json!({
            "confirmationStatus": "confirmed",
            "err": null
        });

        assert!(status_is_confirmed(&status));
    }

    #[test]
    fn fidelity_labels_are_honest() {
        assert_eq!(Fidelity::Current.label(), "current");
        assert_eq!(
            Fidelity::Reconstructed { slot: 442384762 }.label(),
            "reconstructed@442384762"
        );
        assert_eq!(Fidelity::Exact { slot: 100 }.label(), "exact@100");
    }

    #[test]
    fn certificate_summary_discloses_drift_and_verifiability() {
        let drifted = FidelityCertificate {
            fidelity: Fidelity::Reconstructed { slot: 442384762 },
            clock: "slot 442384762".to_string(),
            accounts: Vec::new(),
            drifted: vec!["AccA".to_string(), "AccB".to_string()],
            verifiable: true,
        };
        let s = drifted.summary();
        assert!(s.contains("reconstructed@442384762"), "{s}");
        assert!(s.contains("2 may have drifted"), "{s}");
        assert!(s.contains("verifiable against mainnet"), "{s}");

        let clean = FidelityCertificate {
            fidelity: Fidelity::Exact { slot: 5 },
            clock: String::new(),
            accounts: Vec::new(),
            drifted: Vec::new(),
            verifiable: false,
        };
        let s = clean.summary();
        assert!(s.contains("none drifted"), "{s}");
        assert!(s.contains("no recorded outcome"), "{s}");
    }

    #[test]
    fn confirmed_program_failure_is_still_landed() {
        let status = json!({
            "confirmationStatus": "confirmed",
            "err": {
                "InstructionError": [0, {"Custom": 6001}]
            }
        });

        assert!(status_is_confirmed(&status));
    }

    #[test]
    fn finalized_is_landed() {
        let status = json!({
            "confirmationStatus": "finalized",
            "err": null
        });

        assert!(status_is_confirmed(&status));
    }

    #[test]
    fn processed_is_not_confirmed() {
        let status = json!({
            "confirmationStatus": "processed",
            "err": null
        });

        assert!(!status_is_confirmed(&status));
    }

    #[test]
    fn legacy_rooted_status_is_confirmed() {
        let status = json!({
            "confirmationStatus": null,
            "confirmations": null,
            "err": null
        });

        assert!(status_is_confirmed(&status));
    }
}
