//! Dependency watch: when a program your program depends on is upgraded,
//! replay your recent transactions against the new binary and say what
//! changed.
//!
//! The on-chain half is the `dependency_registry` Anchor program under
//! `programs/`: a protocol registers itself (proving it holds the program's
//! upgrade authority) and lists the programs it depends on, with a URL to
//! alert. This module is the engine half: read the registry, notice a
//! dependency's deploy slot moving, run the check, deliver the report.
//!
//! The check is differential. Every transaction in the corpus is replayed
//! twice on the same state, once with the dependency as it was loaded and
//! once with its freshly deployed bytes swapped in, so state drift affects
//! both runs alike and only the binary change shows. The replay's own
//! agreement with the on-chain outcome is reported alongside as the fidelity
//! of the baseline.

use std::str::FromStr;

use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use solana_address::Address;
use solana_client::rpc_request::RpcRequest;

use crate::error::{Error, Result};
use crate::replay::ReplayResult;
use crate::scope::Scope;

/// The `dependency_registry` program on devnet.
pub const DEVNET_REGISTRY: &str = "4nH59dWUJ5rgTZJTybPbfGY1sgBDwKgrKMBXpRtdxhhg";

/// A registered protocol, decoded from a `Protocol` account.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct RegistryProtocol {
    /// The `Protocol` account's address (PDA on `["protocol", program_id]`).
    pub address: String,
    /// Who may edit the entry.
    pub authority: String,
    /// The program being watched.
    pub program_id: String,
    /// Where reports are POSTed.
    pub alert_url: String,
    /// How many recent transactions to replay per check.
    pub corpus_size: u16,
    /// How many `Dependency` accounts point at this protocol.
    pub dependency_count: u8,
    /// Whether `register` checked the upgrade authority on the registry's
    /// own cluster. A program that lives elsewhere registers unproven and
    /// the engine verifies it on the cluster it lives on (`verified`).
    pub proven: bool,
    /// Set by the engine: whether `authority` is the upgrade authority of
    /// `program_id` on the cluster the checks run on. `None` until checked.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verified: Option<bool>,
    /// Set by the engine: the cluster the program was found on with the
    /// registered authority, e.g. `mainnet` or `devnet`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cluster: Option<String>,
}

/// One dependency of a protocol, decoded from a `Dependency` account.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct RegistryDependency {
    /// The `Dependency` account's address.
    pub address: String,
    /// The `Protocol` account it belongs to.
    pub protocol: String,
    /// The external program whose upgrades trigger a check.
    pub program_id: String,
    /// The last deploy slot a check was run for (written by the reporter).
    pub last_checked_slot: u64,
    /// Whether upgrades of this dependency produce alerts.
    pub alerts_enabled: bool,
}

/// Everything in a registry, joined.
#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct Registry {
    /// The registry program's id.
    pub program: String,
    /// Every registered protocol.
    pub protocols: Vec<RegistryProtocol>,
    /// Every dependency, across all protocols.
    pub dependencies: Vec<RegistryDependency>,
}

/// An upgradeable program's deploy facts.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct DeployInfo {
    /// The program.
    pub program_id: String,
    /// Its programdata account, which holds the binary.
    pub programdata: String,
    /// The slot of the deploy currently in force.
    pub last_deploy_slot: u64,
    /// Who may upgrade it; `None` once the program is frozen.
    pub upgrade_authority: Option<String>,
}

/// One transaction's before/after under the dependency check.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CheckedTransaction {
    /// The transaction.
    pub signature: String,
    /// The slot it landed in.
    pub slot: Option<u64>,
    /// What the chain recorded.
    pub onchain_success: bool,
    /// The replay with the dependency as loaded (its bytes at the
    /// transaction's slot when the history stream is attached, else current).
    pub before: Outcome,
    /// The replay with the dependency's newly deployed bytes.
    pub after: Outcome,
    /// `before` and `after` differ in success or error.
    pub changed: bool,
    /// `after.compute_units - before.compute_units`.
    pub compute_delta: i64,
    /// The check itself failed for this transaction (replay error), in which
    /// case `before`/`after` are empty and this says why.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// A replay outcome without its logs, which would make a corpus report huge.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Outcome {
    /// Whether the replay succeeded.
    pub success: bool,
    /// The failure, formatted, when `success` is false.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// The failure's IDL name, when resolved.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_name: Option<String>,
    /// Compute units consumed.
    pub compute_units: u64,
}

impl From<&ReplayResult> for Outcome {
    fn from(r: &ReplayResult) -> Self {
        Outcome {
            success: r.success,
            error: r.error.clone(),
            error_name: r.error_name.clone(),
            compute_units: r.compute_units,
        }
    }
}

/// The report one dependency check produces.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DependencyReport {
    /// The program whose transactions were replayed.
    pub program_id: String,
    /// The dependency whose new binary was swapped in.
    pub dependency: String,
    /// The dependency's current deploy, `None` if it is not upgradeable.
    pub dependency_deploy: Option<DeployInfo>,
    /// What "before" ran: `previous` (the snapshotted pre-deploy binary),
    /// `at_slot` (the binary in force at each transaction's slot), or
    /// `current` (no old binary was available; before and after are the same
    /// bytes, so only "still executes" can be concluded).
    pub baseline: &'static str,
    /// Why the corpus is not what the rule says, when it is not.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub corpus_note: Option<String>,
    /// Whether each baseline replay used the dependency's bytes at the
    /// transaction's own slot (`true`) or today's bytes (`false`).
    pub exact_baseline: bool,
    /// Unix seconds when the report was produced.
    pub generated_at: i64,
    /// One line, see [`DependencyReport::verdict`]; filled in on creation.
    #[serde(default)]
    pub verdict: String,
    /// The counts.
    pub summary: ReportSummary,
    /// Every transaction checked, oldest first.
    pub transactions: Vec<CheckedTransaction>,
}

/// What the "before" run of a check executes. See [`Scope::dependency_check`].
#[derive(Clone, Debug)]
pub enum Baseline {
    /// The dependency's binary as it was before the deploy.
    Previous(Vec<u8>),
    /// The dependency's binary before its current deploy, fetched from the
    /// account-changes stream; fails without the stream.
    BeforeDeploy,
    /// The binary in force at each transaction's own slot, from the history stream.
    AtSlot,
    /// No old binary; the current one on both sides.
    Current,
}

impl Baseline {
    /// The name that goes in the report.
    pub fn kind(&self) -> &'static str {
        match self {
            Baseline::Previous(_) => "previous",
            Baseline::BeforeDeploy => "before_deploy",
            Baseline::AtSlot => "at_slot",
            Baseline::Current => "current",
        }
    }
}

/// Counts over a report's transactions.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReportSummary {
    /// Transactions checked.
    pub total: usize,
    /// Same success/error before and after.
    pub unchanged: usize,
    /// Succeeded before, fails after.
    pub newly_failing: usize,
    /// Failed before, succeeds after.
    pub newly_passing: usize,
    /// Fails before and after, but with a different error.
    pub error_changed: usize,
    /// The check could not run for these.
    pub errored: usize,
    /// Baselines that agree with the on-chain outcome; the replay's fidelity.
    pub baseline_matches_chain: usize,
    /// Sum of the per-transaction compute deltas.
    pub compute_delta_total: i64,
}

impl DependencyReport {
    /// A one-line verdict.
    pub fn verdict(&self) -> String {
        let s = &self.summary;
        if s.total == 0 {
            return "no transactions to check".into();
        }
        if self.baseline == "current" {
            return format!(
                "{} of {} still execute against the current binary (no previous binary to compare)",
                s.total - s.errored,
                s.total
            );
        }
        let checked = s.total - s.errored;
        let mut verdict = if s.newly_failing == 0 && s.error_changed == 0 {
            format!(
                "{} of {} unchanged; nothing newly fails",
                s.unchanged, s.total
            )
        } else {
            format!(
                "{} newly failing, {} changed error, {} unchanged of {}",
                s.newly_failing, s.error_changed, s.unchanged, s.total
            )
        };
        // A baseline that disagrees with the chain means the state the
        // transactions ran on has drifted; before and after are still
        // comparable to each other, but neither describes what happened.
        let drifted = checked.saturating_sub(s.baseline_matches_chain);
        if checked > 0 && drifted * 2 >= checked {
            verdict.push_str(&format!(
                "; baseline disagrees with the chain for {drifted} of {checked}, so current state has drifted from when they ran"
            ));
        }
        verdict
    }
}

/// Whether `program` is the program of any instruction in `tx`, top-level or
/// inner (CPI). Account keys are the static list followed by the loaded
/// lookup-table addresses, writable then readonly, which is how
/// `programIdIndex` counts.
fn invokes_program(tx: &serde_json::Value, program: &str) -> bool {
    let keys = crate::utils::resolve_account_keys(tx);
    let is_it = |ix: &serde_json::Value| {
        ix["programIdIndex"]
            .as_u64()
            .and_then(|i| keys.get(i as usize))
            .map(|k| k == program)
            .unwrap_or(false)
    };
    let top = tx["transaction"]["message"]["instructions"]
        .as_array()
        .map(|a| a.iter().any(is_it))
        .unwrap_or(false);
    let inner = tx["meta"]["innerInstructions"]
        .as_array()
        .map(|groups| {
            groups.iter().any(|g| {
                g["instructions"]
                    .as_array()
                    .map(|a| a.iter().any(is_it))
                    .unwrap_or(false)
            })
        })
        .unwrap_or(false);
    top || inner
}

/// Anchor's account discriminator: the first 8 bytes of sha256("account:<Name>").
fn anchor_discriminator(name: &str) -> [u8; 8] {
    let hash = Sha256::digest(format!("account:{name}").as_bytes());
    let mut out = [0u8; 8];
    out.copy_from_slice(&hash[..8]);
    out
}

fn read_address(data: &[u8], at: usize) -> Option<String> {
    let bytes: [u8; 32] = data.get(at..at + 32)?.try_into().ok()?;
    Some(Address::from(bytes).to_string())
}

fn read_u64(data: &[u8], at: usize) -> Option<u64> {
    data.get(at..at + 8)?
        .try_into()
        .ok()
        .map(u64::from_le_bytes)
}

/// Allocated size of a `Protocol` account: discriminator, two keys, the URL
/// at its 128-byte limit plus length prefix, corpus_size, dependency_count,
/// proven, bump.
const PROTOCOL_LEN: usize = 8 + 32 + 32 + 4 + 128 + 2 + 1 + 1 + 1;
/// The first program version had no `proven` byte.
const PROTOCOL_V1_LEN: usize = PROTOCOL_LEN - 1;

/// `Protocol` layout: discriminator, authority, program_id, alert_url
/// (u32 length + bytes), corpus_size u16, dependency_count u8, proven bool,
/// bump u8. Entries from the first program version, which had no `proven`
/// byte, are one byte shorter and are skipped: their address no longer
/// matches the program's seeds, so nothing can edit or close them.
fn parse_protocol(address: &str, data: &[u8]) -> Option<RegistryProtocol> {
    let authority = read_address(data, 8)?;
    let program_id = read_address(data, 40)?;
    let len = u32::from_le_bytes(data.get(72..76)?.try_into().ok()?) as usize;
    let alert_url = String::from_utf8(data.get(76..76 + len)?.to_vec()).ok()?;
    let rest = 76 + len;
    let corpus_size = u16::from_le_bytes(data.get(rest..rest + 2)?.try_into().ok()?);
    let dependency_count = *data.get(rest + 2)?;
    // Accounts are allocated at their maximum size (the URL field is padded
    // to its limit), so the layout is told by the total length: the first
    // program version's accounts are one byte shorter than the current ones.
    if data.len() == PROTOCOL_V1_LEN {
        return None;
    }
    let proven = *data.get(rest + 3)? != 0;
    Some(RegistryProtocol {
        address: address.to_string(),
        authority,
        program_id,
        alert_url,
        corpus_size,
        dependency_count,
        proven,
        verified: None,
        cluster: None,
    })
}

/// `Dependency` layout: discriminator, protocol, program_id,
/// last_checked_slot u64, alerts_enabled bool, bump u8.
fn parse_dependency(address: &str, data: &[u8]) -> Option<RegistryDependency> {
    Some(RegistryDependency {
        address: address.to_string(),
        protocol: read_address(data, 8)?,
        program_id: read_address(data, 40)?,
        last_checked_slot: read_u64(data, 72)?,
        alerts_enabled: *data.get(80)? != 0,
    })
}

fn b64(s: &str) -> Vec<u8> {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD
        .decode(s)
        .unwrap_or_default()
}

impl Scope {
    /// Every account of `registry` whose discriminator is `name`'s, as
    /// (address, data).
    fn registry_accounts(&self, registry: &str, name: &str) -> Result<Vec<(String, Vec<u8>)>> {
        if Address::from_str(registry).is_err() {
            return Err(Error::InvalidAddress(registry.to_string()));
        }
        let disc = bs58::encode(anchor_discriminator(name)).into_string();
        let resp: serde_json::Value = self
            .client()
            .send(
                RpcRequest::GetProgramAccounts,
                json!([registry, {
                    "encoding": "base64",
                    "commitment": "confirmed",
                    "filters": [{ "memcmp": { "offset": 0, "bytes": disc } }]
                }]),
            )
            .map_err(Error::rpc)?;
        let arr = resp.as_array().ok_or_else(|| {
            Error::MalformedRpcResponse("getProgramAccounts: not an array".into())
        })?;
        Ok(arr
            .iter()
            .filter_map(|e| {
                let address = e["pubkey"].as_str()?.to_string();
                let data = b64(e["account"]["data"][0].as_str()?);
                Some((address, data))
            })
            .collect())
    }

    /// All protocols registered in `registry`.
    pub fn registry_protocols(&self, registry: &str) -> Result<Vec<RegistryProtocol>> {
        Ok(self
            .registry_accounts(registry, "Protocol")?
            .iter()
            .filter_map(|(a, d)| parse_protocol(a, d))
            .collect())
    }

    /// All dependencies registered in `registry`, across every protocol.
    pub fn registry_dependencies(&self, registry: &str) -> Result<Vec<RegistryDependency>> {
        Ok(self
            .registry_accounts(registry, "Dependency")?
            .iter()
            .filter_map(|(a, d)| parse_dependency(a, d))
            .collect())
    }

    /// The whole registry, joined.
    pub fn registry(&self, registry: &str) -> Result<Registry> {
        Ok(Registry {
            program: registry.to_string(),
            protocols: self.registry_protocols(registry)?,
            dependencies: self.registry_dependencies(registry)?,
        })
    }

    /// An upgradeable program's programdata address, the slot of its current
    /// deploy and its upgrade authority. `None` for programs owned by another
    /// loader (they cannot be upgraded, so they never trigger a check).
    pub fn deploy_info(&self, program_id: &str) -> Result<Option<DeployInfo>> {
        if Address::from_str(program_id).is_err() {
            return Err(Error::InvalidAddress(program_id.to_string()));
        }
        let account: serde_json::Value = self
            .client()
            .send(
                RpcRequest::GetAccountInfo,
                json!([program_id, { "encoding": "base64", "commitment": "confirmed" }]),
            )
            .map_err(Error::rpc)?;
        let Some(data) = account["value"]["data"][0].as_str().map(b64) else {
            return Err(Error::AccountNotFound(program_id.to_string()));
        };
        // Upgradeable loader program account: [0..4] = 2 (Program), [4..36] = programdata.
        if data.len() != 36 || data[..4] != [2, 0, 0, 0] {
            return Ok(None);
        }
        let Some(programdata) = read_address(&data, 4) else {
            return Ok(None);
        };
        // ProgramData header: [0..4] = 3, [4..12] slot, [12] option tag, [13..45] authority.
        let header: serde_json::Value = self
            .client()
            .send(
                RpcRequest::GetAccountInfo,
                json!([programdata, {
                    "encoding": "base64", "commitment": "confirmed",
                    "dataSlice": { "offset": 0, "length": 45 }
                }]),
            )
            .map_err(Error::rpc)?;
        let head = header["value"]["data"][0]
            .as_str()
            .map(b64)
            .unwrap_or_default();
        let Some(last_deploy_slot) = read_u64(&head, 4) else {
            return Ok(None);
        };
        let upgrade_authority = match head.get(12) {
            Some(1) => read_address(&head, 13),
            _ => None,
        };
        Ok(Some(DeployInfo {
            program_id: program_id.to_string(),
            programdata,
            last_deploy_slot,
            upgrade_authority,
        }))
    }

    /// Whether `authority` holds the upgrade authority of `program_id` on
    /// this scope's cluster. `Ok(false)` when the program is absent here, is
    /// not upgradeable, or belongs to someone else, so a registry on one
    /// cluster can vouch for programs on another only when the same key
    /// controls them there.
    pub fn holds_upgrade_authority(&self, program_id: &str, authority: &str) -> Result<bool> {
        match self.deploy_info(program_id) {
            Ok(Some(d)) => Ok(d.upgrade_authority.as_deref() == Some(authority)),
            Ok(None) => Ok(false),
            Err(Error::AccountNotFound(_)) => Ok(false),
            Err(e) => Err(e),
        }
    }

    /// A program's current binary, whichever loader owns it.
    pub fn program_elf(&self, program_id: &str) -> Result<Vec<u8>> {
        if Address::from_str(program_id).is_err() {
            return Err(Error::InvalidAddress(program_id.to_string()));
        }
        crate::replay::fetch_program_elf(self.client(), program_id)
            .ok_or_else(|| Error::AccountNotFound(format!("{program_id} (program binary)")))
    }

    /// Replay `program_id`'s recent transactions with `dependency`'s current
    /// binary swapped in and report what changed.
    ///
    /// `limit` caps the corpus. Only transactions that landed before the
    /// dependency's current deploy are eligible: those ran under the old
    /// binary, so their baseline is what the new one is compared against.
    ///
    /// The baseline decides what "before" means. [`Baseline::Previous`] is the
    /// dependency's binary before the deploy, snapshotted by a watcher that
    /// saw it; the check then runs fast against current state and is truly
    /// differential. [`Baseline::AtSlot`] replays each transaction at its own
    /// slot, which loads the binary in force then from the history stream
    /// (slow, mainnet only). [`Baseline::Current`] has no old binary at all:
    /// before and after run the same bytes, so the report can only show that
    /// the transactions still execute, and says so.
    pub fn dependency_check(
        &self,
        program_id: &str,
        dependency: &str,
        limit: usize,
        baseline: Baseline,
    ) -> Result<DependencyReport> {
        for a in [program_id, dependency] {
            if Address::from_str(a).is_err() {
                return Err(Error::InvalidAddress(a.to_string()));
            }
        }
        let deploy = self.deploy_info(dependency)?;
        let new_elf = self.program_elf(dependency)?;
        let exact = matches!(baseline, Baseline::AtSlot);
        let mut baseline_kind = baseline.kind();
        let previous_elf = match baseline {
            Baseline::Previous(elf) => Some(elf),
            Baseline::BeforeDeploy => {
                let Some(d) = deploy.as_ref() else {
                    return Err(Error::InvalidSpec(format!(
                        "{dependency} is not an upgradeable program, so it has no earlier binary"
                    )));
                };
                match self.program_elf_before(dependency, d.last_deploy_slot) {
                    Some((_, elf)) if elf != new_elf => Some(elf),
                    Some(_) => {
                        return Err(Error::InvalidSpec(format!(
                            "the stream returned the current binary for {dependency}; its earlier deploy is out of the stream's reach"
                        )))
                    }
                    None => {
                        return Err(Error::InvalidSpec(format!(
                            "no earlier binary for {dependency}: the engine has no account-changes stream, or the deploy before slot {} is beyond its reach",
                            d.last_deploy_slot
                        )))
                    }
                }
            }
            _ => None,
        };
        if previous_elf.is_some() && baseline_kind == "before_deploy" {
            baseline_kind = "previous";
        }

        // One page of signatures, newest first; keep those before the deploy.
        // Recent history is often mostly deploys and unrelated mentions, so
        // the page is well over the limit.
        let page = limit.max(1).saturating_mul(4).clamp(50, 1000);
        // Signatures for an address include every transaction that mentions
        // it, deploys of the program itself among them; a deploy invokes the
        // loader, not the program, and cannot be replayed as a program call.
        // A transaction that never reaches the dependency cannot be changed
        // by it either, so only those that call both are worth replaying.
        // Fetching each transaction in a tight loop trips public and paid
        // endpoints' rate limits alike; a rate-limited fetch is retried with
        // a growing pause rather than silently dropping the transaction.
        let fetch = |signature: &str| -> Option<serde_json::Value> {
            let mut wait = std::time::Duration::from_millis(250);
            for attempt in 0..5 {
                match self.transaction_json(signature) {
                    Ok(tx) => return Some(tx),
                    Err(_) if attempt < 4 => {
                        std::thread::sleep(wait);
                        wait *= 2;
                    }
                    Err(_) => return None,
                }
            }
            None
        };
        let recent: Vec<_> = self
            .signatures(program_id, page)?
            .into_iter()
            .filter(|s| {
                fetch(&s.signature)
                    .map(|tx| {
                        invokes_program(&tx, program_id)
                            && (dependency == program_id || invokes_program(&tx, dependency))
                    })
                    .unwrap_or(false)
            })
            .collect();
        let mut corpus: Vec<_> = recent
            .iter()
            .filter(|s| match (deploy.as_ref(), s.slot) {
                (Some(d), Some(slot)) => slot < d.last_deploy_slot,
                _ => true,
            })
            .take(limit)
            .cloned()
            .collect();
        // Nothing landed before the current deploy: every recent transaction
        // already ran under the new binary. Check them anyway and say so,
        // rather than answering with an empty report.
        let corpus_note = if recent.is_empty() {
            Some(format!(
                "none of {program_id}'s recent transactions call {dependency}"
            ))
        } else if corpus.is_empty() {
            corpus = recent.iter().take(limit).cloned().collect();
            Some(format!(
                "no transactions landed before the dependency's current deploy (slot {}); \
                 checked the newest ones, which already ran under the new binary",
                deploy.as_ref().map(|d| d.last_deploy_slot).unwrap_or(0)
            ))
        } else {
            None
        };
        // Oldest first reads better in a report.
        corpus.reverse();

        let mut transactions = Vec::with_capacity(corpus.len());
        let mut summary = ReportSummary::default();
        for sig in corpus {
            let onchain_success = !sig.err;
            let checked = (|| -> Result<CheckedTransaction> {
                let mut replay = if exact {
                    self.replay_at_slot(&sig.signature)?
                } else {
                    self.replay(&sig.signature)?
                };
                // With a snapshotted previous binary, "before" is that binary
                // on the same state; the loaded (current) one is restored by
                // compare_patch afterwards.
                let (before, after) = if let Some(old) = previous_elf.as_ref() {
                    let old_run = replay.compare_patch(dependency, old.clone(), &[])?;
                    let new_run = replay.compare_patch(dependency, new_elf.clone(), &[])?;
                    (Outcome::from(&old_run.after), Outcome::from(&new_run.after))
                } else {
                    let cmp = replay.compare_patch(dependency, new_elf.clone(), &[])?;
                    (Outcome::from(&cmp.before), Outcome::from(&cmp.after))
                };
                let changed = before.success != after.success || before.error != after.error;
                let compute_delta = after.compute_units as i64 - before.compute_units as i64;
                Ok(CheckedTransaction {
                    signature: sig.signature.clone(),
                    slot: sig.slot,
                    onchain_success,
                    before,
                    after,
                    changed,
                    compute_delta,
                    error: None,
                })
            })();
            let tx = match checked {
                Ok(tx) => tx,
                Err(e) => CheckedTransaction {
                    signature: sig.signature.clone(),
                    slot: sig.slot,
                    onchain_success,
                    before: Outcome::default(),
                    after: Outcome::default(),
                    changed: false,
                    compute_delta: 0,
                    error: Some(e.to_string()),
                },
            };
            summary.total += 1;
            if tx.error.is_some() {
                summary.errored += 1;
            } else {
                if tx.before.success == tx.onchain_success {
                    summary.baseline_matches_chain += 1;
                }
                match (tx.before.success, tx.after.success) {
                    (true, false) => summary.newly_failing += 1,
                    (false, true) => summary.newly_passing += 1,
                    (false, false) if tx.before.error != tx.after.error => {
                        summary.error_changed += 1
                    }
                    _ => summary.unchanged += 1,
                }
                summary.compute_delta_total += tx.compute_delta;
            }
            transactions.push(tx);
        }

        let mut report = DependencyReport {
            program_id: program_id.to_string(),
            dependency: dependency.to_string(),
            dependency_deploy: deploy,
            baseline: baseline_kind,
            corpus_note,
            exact_baseline: exact,
            generated_at: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0),
            verdict: String::new(),
            summary,
            transactions,
        };
        report.verdict = report.verdict();
        Ok(report)
    }
}

/// What a protocol's alert URL receives: the report, wrapped with who sent it.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AlertPayload {
    /// Always `"dependency_upgraded"`, so receivers can route on it.
    pub kind: &'static str,
    /// The `Protocol` account the alert is for.
    pub protocol: String,
    /// One line, see [`DependencyReport::verdict`].
    pub verdict: String,
    /// The full report.
    pub report: DependencyReport,
    /// Where the full report can be opened.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub report_url: Option<String>,
}

/// A reporter identity: reports are signed so a receiver can tell a real
/// alert from anything posted by someone who read the URL off chain.
pub struct Reporter {
    keypair: solana_keypair::Keypair,
}

impl Reporter {
    /// From a JSON byte-array keypair file or a base58 secret, as
    /// `SVMSCOPE_REPORTER_KEYPAIR` may hold either.
    pub fn from_env_value(value: &str) -> Option<Reporter> {
        let value = value.trim();
        if value.is_empty() {
            return None;
        }
        let keypair = if value.starts_with('[') {
            let bytes: Vec<u8> = serde_json::from_str(value).ok()?;
            solana_keypair::Keypair::try_from(bytes.as_slice()).ok()?
        } else if std::path::Path::new(value).exists() {
            solana_keypair::read_keypair_file(value).ok()?
        } else {
            solana_keypair::Keypair::from_base58_string(value)
        };
        Some(Reporter { keypair })
    }

    /// The reporter's public key, base58.
    pub fn address(&self) -> String {
        use solana_signer::Signer;
        self.keypair.pubkey().to_string()
    }

    /// Ed25519 signature over the exact body bytes, base58.
    pub fn sign(&self, body: &[u8]) -> String {
        use solana_signer::Signer;
        bs58::encode(self.keypair.sign_message(body).as_ref()).into_string()
    }
}

/// POST an alert to `url`. The body is the payload as JSON; the headers
/// `x-svmscope-reporter` and `x-svmscope-signature` carry the reporter's
/// address and its signature over the body when a reporter is configured.
/// Returns the HTTP status.
pub fn deliver_alert(
    url: &str,
    payload: &AlertPayload,
    reporter: Option<&Reporter>,
) -> Result<u16> {
    let body = serde_json::to_vec(payload).map_err(|e| Error::InvalidSpec(e.to_string()))?;
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(20))
        .build()
        .map_err(|e| Error::InvalidSpec(e.to_string()))?;
    let mut req = client
        .post(url)
        .header("content-type", "application/json")
        .header("user-agent", "svmscope-dependency-watch");
    if let Some(r) = reporter {
        req = req
            .header("x-svmscope-reporter", r.address())
            .header("x-svmscope-signature", r.sign(&body));
    }
    let resp = req
        .body(body)
        .send()
        .map_err(|e| Error::InvalidSpec(format!("alert delivery to {url} failed: {e}")))?;
    Ok(resp.status().as_u16())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discriminators_match_anchor() {
        // sha256("account:Protocol")[..8], as Anchor computes it.
        let d = anchor_discriminator("Protocol");
        let full = Sha256::digest(b"account:Protocol");
        assert_eq!(&d[..], &full[..8]);
        assert_ne!(
            anchor_discriminator("Protocol"),
            anchor_discriminator("Dependency")
        );
    }

    #[test]
    fn parses_protocol_layout() {
        let mut data = vec![0u8; 8];
        data.extend_from_slice(&[1u8; 32]); // authority
        data.extend_from_slice(&[2u8; 32]); // program_id
        let url = b"https://x.test/hook";
        data.extend_from_slice(&(url.len() as u32).to_le_bytes());
        data.extend_from_slice(url);
        data.extend_from_slice(&200u16.to_le_bytes());
        data.push(3); // dependency_count
        data.push(1); // proven
        data.push(254); // bump
                        // Borsh writes the fields right after the URL's actual bytes; the
                        // account is allocated for a 128-byte URL, so the rest is padding.
        data.resize(PROTOCOL_LEN, 0);
        let p = parse_protocol("addr", &data).unwrap();
        assert!(p.proven);
        // The first program version had no `proven` byte and one byte less
        // of allocation; such entries are skipped.
        let mut old = data.clone();
        old.remove(76 + "https://x.test/hook".len() + 3);
        assert_eq!(old.len(), PROTOCOL_V1_LEN);
        assert!(parse_protocol("addr", &old).is_none());
        assert_eq!(p.authority, Address::from([1u8; 32]).to_string());
        assert_eq!(p.program_id, Address::from([2u8; 32]).to_string());
        assert_eq!(p.alert_url, "https://x.test/hook");
        assert_eq!(p.corpus_size, 200);
        assert_eq!(p.dependency_count, 3);
        assert_eq!(p.verified, None);
    }

    #[test]
    fn parses_dependency_layout() {
        let mut data = vec![0u8; 8];
        data.extend_from_slice(&[5u8; 32]); // protocol
        data.extend_from_slice(&[6u8; 32]); // program_id
        data.extend_from_slice(&42u64.to_le_bytes());
        data.push(1); // alerts_enabled
        data.push(255); // bump
        let d = parse_dependency("addr", &data).unwrap();
        assert_eq!(d.protocol, Address::from([5u8; 32]).to_string());
        assert_eq!(d.program_id, Address::from([6u8; 32]).to_string());
        assert_eq!(d.last_checked_slot, 42);
        assert!(d.alerts_enabled);
        assert!(parse_dependency("addr", &data[..80]).is_none());
    }

    #[test]
    fn invokes_program_sees_top_level_and_inner_calls() {
        let tx = serde_json::json!({
            "transaction": { "message": {
                "accountKeys": ["payer", "prog", "loader", "other"],
                "instructions": [{ "programIdIndex": 2 }]
            }},
            "meta": {
                "loadedAddresses": { "writable": ["alt1"], "readonly": [] },
                "innerInstructions": [{ "instructions": [{ "programIdIndex": 4 }] }]
            }
        });
        assert!(!invokes_program(&tx, "prog"));
        assert!(invokes_program(&tx, "loader"));
        assert!(invokes_program(&tx, "alt1"));
    }

    #[test]
    fn verdict_reads_plainly() {
        let mut r = DependencyReport {
            program_id: "p".into(),
            dependency: "d".into(),
            dependency_deploy: None,
            baseline: "previous",
            corpus_note: None,
            exact_baseline: false,
            generated_at: 0,
            verdict: String::new(),
            summary: ReportSummary {
                total: 10,
                unchanged: 10,
                baseline_matches_chain: 10,
                ..Default::default()
            },
            transactions: vec![],
        };
        assert_eq!(r.verdict(), "10 of 10 unchanged; nothing newly fails");
        r.summary.unchanged = 7;
        r.summary.newly_failing = 3;
        assert_eq!(
            r.verdict(),
            "3 newly failing, 0 changed error, 7 unchanged of 10"
        );
    }

    #[test]
    fn reporter_signs_and_names_itself() {
        let kp = solana_keypair::Keypair::new();
        let json = serde_json::to_string(&kp.to_bytes().to_vec()).unwrap();
        let r = Reporter::from_env_value(&json).unwrap();
        use solana_signer::Signer;
        assert_eq!(r.address(), kp.pubkey().to_string());
        let sig = r.sign(b"hello");
        assert_eq!(bs58::decode(sig).into_vec().unwrap().len(), 64);
        assert!(Reporter::from_env_value("   ").is_none());
    }
}
