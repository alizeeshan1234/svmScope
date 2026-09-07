//! Stage 3 — Deterministic replay.
//!
//! Loads the transaction's accounts AND programs into an in-process SVM
//! (LiteSVM) so the transaction can be re-executed locally.

use crate::error::{Error, Result};
use crate::fixture::{Fixture, FixtureEntry};
use base64::Engine;
use litesvm::types::TransactionResult;
use litesvm::LiteSVM;
use serde_json::json;
use solana_account::Account;
use solana_address::Address;
use solana_client::rpc_client::RpcClient;
use solana_client::rpc_request::RpcRequest;
use solana_clock::Clock;
use solana_transaction::versioned::VersionedTransaction;
use std::collections::HashMap;
use std::str::FromStr;

const NATIVE_LOADER: &str = "NativeLoader1111111111111111111111111111111";
const BPF_LOADER_2: &str = "BPFLoader2111111111111111111111111111111111";
const BPF_LOADER_UPGRADEABLE: &str = "BPFLoaderUpgradeab1e11111111111111111111111";

/// The outcome of one local replay run.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, Default)]
pub struct ReplayResult {
    /// Whether the transaction executed without error.
    pub success: bool,
    /// The failure, formatted, when `success` is false.
    pub error: Option<String>,
    /// The failure resolved to its human name via the program's IDL, e.g.
    /// "SlippageToleranceExceeded" for a bare `Custom(6001)`. Filled in by the
    /// library layer (which has RPC access); `None` when unresolved.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_name: Option<String>,
    /// The program logs the replay produced.
    pub logs: Vec<String>,
    /// Compute units the replay consumed.
    pub compute_units: u64,
}

fn b64_decode(s: &str) -> Vec<u8> {
    base64::engine::general_purpose::STANDARD
        .decode(s)
        .unwrap_or_default()
}

/// Process-wide cache of program ELFs, keyed by programdata address and the
/// slot it was last upgraded at. Program binaries are the largest thing a replay
/// downloads (Jupiter alone is over a megabyte) and they change only on upgrade,
/// so one 45-byte header fetch per program replaces the full download whenever
/// the upgrade slot is unchanged.
type ElfCache = HashMap<(String, u64), std::sync::Arc<Vec<u8>>>;
static ELF_CACHE: std::sync::LazyLock<std::sync::Mutex<ElfCache>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(HashMap::new()));
const ELF_CACHE_MAX_ENTRIES: usize = 256;

/// The ELF inside an upgradeable program's programdata account, served from
/// [`ELF_CACHE`] when the account's upgrade slot has not changed.
fn fetch_programdata_elf_cached(client: &RpcClient, pd_addr: &str) -> Option<Vec<u8>> {
    // Header only: [0..4] variant, [4..12] last-upgrade slot, [12..45] authority.
    let header: serde_json::Value = client
        .send(
            RpcRequest::GetAccountInfo,
            json!([pd_addr, { "encoding": "base64", "commitment": "confirmed", "dataSlice": { "offset": 0, "length": 45 } }]),
        )
        .ok()?;
    let head = b64_decode(header["value"]["data"][0].as_str()?);
    let slot = head
        .get(4..12)
        .and_then(|b| b.try_into().ok())
        .map(u64::from_le_bytes)?;
    let key = (pd_addr.to_string(), slot);
    if let Ok(cache) = ELF_CACHE.lock() {
        if let Some(elf) = cache.get(&key) {
            return Some(elf.as_ref().clone());
        }
    }
    let elf = fetch_account_data(client, pd_addr)
        .filter(|d| d.len() > 45)
        .map(|d| d[45..].to_vec())?;
    if let Ok(mut cache) = ELF_CACHE.lock() {
        if cache.len() >= ELF_CACHE_MAX_ENTRIES {
            cache.clear();
        }
        cache.insert(key, std::sync::Arc::new(elf.clone()));
    }
    Some(elf)
}

/// A program's ELF given its program id, whichever loader owns it: the
/// programdata account for the upgradeable loader (served from [`ELF_CACHE`]),
/// or the program account itself for the legacy loaders.
pub(crate) fn fetch_program_elf(client: &RpcClient, program: &str) -> Option<Vec<u8>> {
    let data = fetch_account_data(client, program)?;
    if data.starts_with(b"\x7fELF") {
        return Some(data);
    }
    // Upgradeable loader program account: [0..4]=variant (2 = Program), [4..36]=programdata.
    if data.len() == 36 && data[..4] == [2, 0, 0, 0] {
        let pd: [u8; 32] = data[4..36].try_into().ok()?;
        return fetch_programdata_elf_cached(client, &Address::from(pd).to_string());
    }
    None
}

/// Fetch a single account's raw data via getAccountInfo.
fn fetch_account_data(client: &RpcClient, address: &str) -> Option<Vec<u8>> {
    let resp: serde_json::Value = client
        .send(
            RpcRequest::GetAccountInfo,
            json!([address, { "encoding": "base64", "commitment": "confirmed" }]),
        )
        .ok()?;
    let data_b64 = resp["value"]["data"][0].as_str()?;
    Some(b64_decode(data_b64))
}

fn fetch_account_at_slot(
    archive: &RpcClient,
    address: &str,
    slot: u64,
) -> Option<serde_json::Value> {
    let resp: serde_json::Value = archive
        .send(
            RpcRequest::GetAccountInfo,
            json!([address, { "encoding": "base64", "slot": slot }]),
        )
        .ok()?;
    let v = &resp["value"];
    if v.is_null() {
        None
    } else {
        Some(v.clone())
    }
}

/// How far behind the transaction's slot the archive probe asks. A non-archive
/// answers every query at its current tip, so probing at the transaction's own
/// slot is fooled whenever that slot *is* the tip (a transaction from the latest
/// finalized slot, or anything on a fresh localnet). Probing ~10k slots
/// (about an hour) earlier leaves no tip a non-archive could answer from.
const ARCHIVE_PROBE_LAG: u64 = 10_000;

/// Verify the endpoint actually honors historical `slot` queries before we
/// trust anything it returns. A non-archival RPC (Helius, a public node)
/// silently *ignores* the `slot` param and answers at the current tip — which
/// would make a "replay at slot" quietly wrong. We probe an always-present
/// account (the SPL Token program) at a slot well before the transaction's and
/// confirm the response was evaluated no later than that.
pub(crate) fn archive_honors_slot(archive: &RpcClient, slot: u64) -> bool {
    let probe = slot.saturating_sub(ARCHIVE_PROBE_LAG);
    let resp: serde_json::Value = match archive.send(
        RpcRequest::GetAccountInfo,
        json!([SPL_TOKEN_PROGRAM, { "encoding": "base64", "slot": probe }]),
    ) {
        Ok(v) => v,
        Err(_) => return false,
    };
    // An archive evaluates the query at (≤) the requested slot; a non-archive
    // ignores `slot` and answers at the current tip, which is past `probe`
    // by construction (the transaction itself landed after it).
    match resp["context"]["slot"].as_u64() {
        Some(ctx_slot) => ctx_slot <= probe,
        None => false,
    }
}

fn fetch_account_data_at_slot(archive: &RpcClient, address: &str, slot: u64) -> Option<Vec<u8>> {
    Some(b64_decode(
        fetch_account_at_slot(archive, address, slot)?["data"][0].as_str()?,
    ))
}

fn fetch_loaded_at_slot(
    archive: &RpcClient,
    account_keys: &[String],
    slot: u64,
) -> Result<LoadedAccounts> {
    let mut out: Vec<(Address, Loaded)> = Vec::new();
    let mut existing: HashMap<String, ()> = HashMap::new();

    for key in account_keys {
        let Some(acc) = fetch_account_at_slot(archive, key, slot) else {
            continue;
        };
        existing.insert(key.clone(), ());
        let Ok(address) = Address::from_str(key) else {
            continue;
        };
        let owner = acc["owner"].as_str().unwrap_or_default();
        let executable = acc["executable"].as_bool().unwrap_or(false);

        if executable {
            let elf: Option<Vec<u8>> = if owner == NATIVE_LOADER {
                None
            } else if owner == BPF_LOADER_2 {
                Some(b64_decode(acc["data"][0].as_str().unwrap_or_default()))
            } else if owner == BPF_LOADER_UPGRADEABLE {
                let prog = b64_decode(acc["data"][0].as_str().unwrap_or_default());
                if prog.len() >= 36 {
                    let pd_bytes: [u8; 32] = prog[4..36].try_into().unwrap();
                    let pd_addr = Address::from(pd_bytes);
                    // programdata AT THE SAME SLOT → the ELF that was live then
                    fetch_account_data_at_slot(archive, &pd_addr.to_string(), slot)
                        .filter(|d| d.len() > 45)
                        .map(|d| d[45..].to_vec())
                } else {
                    None
                }
            } else {
                None
            };
            if let Some(elf) = elf {
                out.push((address, Loaded::Program(elf)));
            }
            continue;
        }

        let Ok(owner_addr) = Address::from_str(owner) else {
            continue;
        };
        out.push((
            address,
            Loaded::Data(Account {
                lamports: acc["lamports"].as_u64().unwrap_or(0),
                data: b64_decode(acc["data"][0].as_str().unwrap_or_default()),
                owner: owner_addr,
                executable: false,
                rent_epoch: 0,
            }),
        ));
    }
    Ok((out, existing))
}

/// A ready-to-load account: either raw data, or a program's ELF bytecode.
#[derive(Clone)]
enum Loaded {
    Data(Account),
    Program(Vec<u8>),
}

/// The loadables plus the set of addresses that actually exist on-chain.
type LoadedAccounts = (Vec<(Address, Loaded)>, HashMap<String, ()>);

/// Fetch each account's on-chain state and resolve program ELFs into loadables.
///
/// This does all the network I/O; building a fresh SVM from the result is then
/// free, which is what lets a whole scenario suite run without re-fetching.
/// Returns the loadables plus the set of addresses that actually exist on-chain
/// (non-null) — including programs whose ELF we couldn't resolve. The caller uses
/// that set to tell a genuinely-closed account (safe to reconstruct) apart from a
/// program that merely failed to load (must NOT be reconstructed as data).
fn fetch_loaded(client: &RpcClient, account_keys: &[String]) -> Result<LoadedAccounts> {
    let resp: serde_json::Value = client
        .send(
            RpcRequest::GetMultipleAccounts,
            json!([
                account_keys,
                { "encoding": "base64", "commitment": "confirmed" }
            ]),
        )
        .map_err(Error::rpc)?;

    let accounts = resp["value"]
        .as_array()
        .ok_or_else(|| Error::MalformedRpcResponse("getMultipleAccounts: no value array".into()))?;
    let mut out: Vec<(Address, Loaded)> = Vec::new();
    let mut existing: HashMap<String, ()> = HashMap::new();

    for (i, acc) in accounts.iter().enumerate() {
        if acc.is_null() {
            continue; // account doesn't exist (created during the tx, etc.)
        }
        existing.insert(account_keys[i].clone(), ());
        let Ok(address) = Address::from_str(&account_keys[i]) else {
            continue;
        };
        let owner = acc["owner"].as_str().unwrap_or_default();
        let executable = acc["executable"].as_bool().unwrap_or(false);

        if executable {
            // Resolve the program's ELF bytecode based on which loader owns it.
            let elf: Option<Vec<u8>> = if owner == NATIVE_LOADER {
                None // native programs are built into LiteSVM
            } else if owner == BPF_LOADER_2 {
                Some(b64_decode(acc["data"][0].as_str().unwrap_or_default()))
            } else if owner == BPF_LOADER_UPGRADEABLE {
                // Upgradeable: program account is a pointer; bytes 4..36 = the
                // programdata address, ELF starts at offset 45 inside it.
                let prog = b64_decode(acc["data"][0].as_str().unwrap_or_default());
                if prog.len() >= 36 {
                    let pd_bytes: [u8; 32] = prog[4..36].try_into().unwrap();
                    let pd_addr = Address::from(pd_bytes);
                    fetch_programdata_elf_cached(client, &pd_addr.to_string())
                } else {
                    None
                }
            } else {
                None
            };
            if let Some(elf) = elf {
                out.push((address, Loaded::Program(elf)));
            }
            continue;
        }

        let Ok(owner_addr) = Address::from_str(owner) else {
            continue;
        };
        out.push((
            address,
            Loaded::Data(Account {
                lamports: acc["lamports"].as_u64().unwrap_or(0),
                data: b64_decode(acc["data"][0].as_str().unwrap_or_default()),
                owner: owner_addr,
                executable: false,
                rent_epoch: 0,
            }),
        ));
    }
    Ok((out, existing))
}

/// A Solana runtime feature gate to flip for a replay: activate one that isn't
/// live on mainnet yet ("will my tx still work when this ships?"), or deactivate
/// an active one. Most execution-affecting SIMDs land on-chain as one of these.
#[derive(Clone, Debug)]
pub struct FeatureToggle {
    /// The feature gate's account address (its on-chain pubkey).
    pub id: Address,
    /// `true` = activate the gate, `false` = deactivate it.
    pub active: bool,
}

/// Build a fresh LiteSVM from already-fetched loadables (no network).
///
/// `features` optionally overrides the runtime feature set: we start from the
/// mainnet-active set (the replay baseline) and flip each requested gate, so a
/// transaction can be re-executed as if a feature were (in)active.
/// A fresh SVM holding `loaded`, with BPF register tracing on when `tracing`
/// is set (the profiler's world). Tracing costs memory per executed
/// instruction, so it is never on for ordinary replays.
fn svm_from_loaded_with(
    loaded: &[(Address, Loaded)],
    features: &[FeatureToggle],
    slot: u64,
    tracing: bool,
) -> LiteSVM {
    // Disable sigverify + blockhash check: we're replaying an already-signed
    // transaction, so its original blockhash won't be valid in a fresh SVM.
    #[cfg(feature = "profiler")]
    let base = LiteSVM::new_debuggable(tracing);
    #[cfg(not(feature = "profiler"))]
    let base = {
        debug_assert!(!tracing, "profiler feature is off");
        LiteSVM::new()
    };
    let mut svm = base
        .with_sigverify(false)
        .with_blockhash_check(false)
        // The debugger splits logs per step; LiteSVM's default 10 KB cap would
        // silently drop the tail of a long transaction's logs.
        .with_log_bytes_limit(None);
    if !features.is_empty() {
        // Start from mainnet's active gates and flip the requested ones. The
        // pubkey types differ across crates (Address vs solana_pubkey::Pubkey),
        // so bridge by their shared 32-byte representation.
        let mut fs = LiteSVM::mainnet_feature_set();
        for t in features {
            let pk = solana_pubkey::Pubkey::new_from_array(t.id.to_bytes());
            if t.active {
                fs.activate(&pk, slot);
            } else {
                fs.deactivate(&pk);
            }
        }
        // `with_builtins` is what rebuilds the program-runtime environment (CU
        // model, syscalls, feature-gated builtins) from the feature set — setting
        // the set alone doesn't, so execution would otherwise ignore the toggle.
        svm = svm
            .with_feature_set(fs)
            .with_builtins()
            .with_feature_accounts();
    }
    for (addr, l) in loaded {
        match l {
            // A load failure here (malformed account, unloadable ELF) surfaces
            // through the replay result itself — a library must not print.
            // Sysvar accounts (the Rent sysvar is always in the world, see
            // `with_rent_sysvar`) refresh LiteSVM's sysvar cache on insert, so
            // the runtime and programs read the cluster's parameters.
            Loaded::Data(a) => {
                let _ = svm.set_account(*addr, a.clone());
            }
            Loaded::Program(elf) => {
                let _ = svm.add_program(*addr, elf);
            }
        }
    }
    svm
}

/// Per-account load info for building a fidelity report: the address, what kind
/// of load it was, and a blake3 content hash of the bytes actually loaded.
pub(crate) struct LoadedInfo {
    pub address: String,
    pub is_program: bool,
    pub owner_is_system: bool,
    pub data_len: usize,
    pub hash: String,
}

/// The reconstructed world a transaction ran in, fetched once. Run any number of
/// scenarios against it with [`ReplayContext::run`] — each gets a pristine SVM.
#[derive(Clone)]
pub(crate) struct ReplayContext {
    /// The replayed transaction's signature (empty for pre-flight transactions).
    signature: String,
    tx: VersionedTransaction,
    loaded: Vec<(Address, Loaded)>,
    /// The slot the transaction landed in. The SVM clock is set to this so
    /// Address Lookup Table resolution — and slot-sensitive program logic like
    /// oracle staleness checks — sees the same world the transaction ran in.
    /// Without it, LiteSVM's default slot is below recent mainnet slots, and any
    /// ALT extended more recently fails with `InvalidAddressLookupTableIndex`.
    slot: Option<u64>,
    /// The chain's actual wall-clock time at that slot, from the RPC. Estimating
    /// it from genesis drifts by months (slots aren't reliably 400ms), and any
    /// program comparing against a real date would then be wrong.
    block_time: Option<i64>,
    /// Optional clock warp applied on top of `slot` (see [`TimeTravel`]).
    time_travel: TimeTravel,
    /// IDLs by owner program, used to resolve named-field assertions
    /// (`assert pool.reserveA`) against accounts the built-in decoders don't
    /// recognize. Populated by the library layer, which has RPC access.
    idls: HashMap<String, serde_json::Value>,
    /// Runtime feature gates to flip before replaying (empty = mainnet-as-is).
    feature_toggles: Vec<FeatureToggle>,
}

/// An account that the transaction changed, with raw before/after bytes. The
/// caller turns these into named field diffs using the account's layout.
pub(crate) struct RawAccountDiff {
    pub(crate) address: String,
    pub(crate) owner: String,
    pub(crate) lamports_before: u64,
    pub(crate) lamports_after: u64,
    pub(crate) data_before: Vec<u8>,
    pub(crate) data_after: Vec<u8>,
}

/// Move the SVM's clock forward (or to an absolute point) so time-gated program
/// logic can be tested without waiting: unstake after an epoch, claim after a
/// vesting cliff, withdraw after a cooldown, settle after an auction ends.
///
/// Relative fields add to the transaction's own clock; absolute fields replace it.
/// Slots and epochs are kept consistent with each other where possible
/// (Solana's ~432,000 slots per epoch, ~400ms per slot).
#[derive(Debug, Default, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct TimeTravel {
    /// Jump forward this many epochs (the common case for staking).
    #[serde(default)]
    pub epochs: Option<i64>,
    /// Jump forward this many slots.
    #[serde(default)]
    pub slots: Option<i64>,
    /// Jump forward this many seconds (vesting cliffs, cooldowns).
    #[serde(default)]
    pub seconds: Option<i64>,
    /// Set the unix timestamp outright.
    #[serde(default)]
    pub at_unix_timestamp: Option<i64>,
    /// Set the slot outright.
    #[serde(default)]
    pub at_slot: Option<u64>,
    /// Set the epoch outright.
    #[serde(default)]
    pub at_epoch: Option<u64>,
}

/// Slots per epoch on mainnet, used to keep slot/epoch/time coherent when warping.
const SLOTS_PER_EPOCH: i64 = 432_000;
/// Approximate seconds per slot.
const SECS_PER_SLOT: f64 = 0.4;

impl TimeTravel {
    /// True when no clock or balance rewind is requested.
    pub fn is_noop(&self) -> bool {
        self.epochs.is_none()
            && self.slots.is_none()
            && self.seconds.is_none()
            && self.at_unix_timestamp.is_none()
            && self.at_slot.is_none()
            && self.at_epoch.is_none()
    }

    /// Apply this warp to a clock, advancing the related fields together so a
    /// program that reads epoch *and* timestamp sees a consistent world.
    fn apply(&self, clock: &mut Clock) {
        // Saturating throughout: the warp values come from untrusted JSON, and an
        // extreme epoch/slot count must clamp, never overflow-panic (debug) or
        // wrap to a nonsense clock (release).
        let mut slot_delta: i64 = 0;
        if let Some(e) = self.epochs {
            slot_delta = slot_delta.saturating_add(e.saturating_mul(SLOTS_PER_EPOCH));
        }
        if let Some(s) = self.slots {
            slot_delta = slot_delta.saturating_add(s);
        }
        if slot_delta != 0 {
            clock.slot = clock.slot.saturating_add_signed(slot_delta);
            clock.epoch = clock
                .epoch
                .saturating_add_signed(slot_delta / SLOTS_PER_EPOCH);
            clock.unix_timestamp = clock
                .unix_timestamp
                .saturating_add((slot_delta as f64 * SECS_PER_SLOT) as i64);
        }
        if let Some(secs) = self.seconds {
            clock.unix_timestamp = clock.unix_timestamp.saturating_add(secs);
            // Keep slots roughly in step so slot-based checks move too.
            let by = (secs as f64 / SECS_PER_SLOT) as i64;
            clock.slot = clock.slot.saturating_add_signed(by);
            clock.epoch = clock.epoch.saturating_add_signed(by / SLOTS_PER_EPOCH);
        }
        // Absolute settings win over the relative jumps above.
        if let Some(t) = self.at_unix_timestamp {
            clock.unix_timestamp = t;
        }
        if let Some(s) = self.at_slot {
            clock.slot = s;
        }
        if let Some(e) = self.at_epoch {
            clock.epoch = e;
        }
        clock.leader_schedule_epoch = clock.epoch + 1;
        // The epoch's start can't be after "now" once we've moved.
        clock.epoch_start_timestamp = clock.epoch_start_timestamp.min(clock.unix_timestamp);
    }

    /// A human summary of where we travelled to, for the UI.
    pub(crate) fn describe(&self, clock: &Clock) -> String {
        format!(
            "slot {} · epoch {} · {}",
            clock.slot,
            clock.epoch,
            chrono_like(clock.unix_timestamp)
        )
    }
}

/// Format a unix timestamp as UTC without pulling in a date crate.
fn chrono_like(ts: i64) -> String {
    // days since epoch → civil date (Howard Hinnant's algorithm)
    let days = ts.div_euclid(86_400);
    let secs = ts.rem_euclid(86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02} UTC",
        secs / 3600,
        (secs % 3600) / 60
    )
}

impl ReplayContext {
    /// Warp the clock for subsequent runs (see [`TimeTravel`]).
    /// The ELF loaded for `program` in this replay's world (the on-chain
    /// bytes, or a replacement), if it is a program.
    pub(crate) fn program_elf(&self, program: &str) -> Option<&[u8]> {
        self.loaded.iter().find_map(|(addr, l)| match l {
            Loaded::Program(elf) if addr.to_string() == program => Some(elf.as_slice()),
            _ => None,
        })
    }

    pub(crate) fn set_time_travel(&mut self, tt: TimeTravel) {
        self.time_travel = tt;
    }

    /// Replace a loaded program's ELF bytecode (for A/B patch comparison).
    /// Returns the previous ELF, or `None` if `program_id` isn't loaded here as
    /// an executable program.
    pub(crate) fn replace_program(&mut self, program_id: &str, elf: Vec<u8>) -> Option<Vec<u8>> {
        for (addr, l) in self.loaded.iter_mut() {
            if addr.to_string() == program_id {
                if let Loaded::Program(existing) = l {
                    return Some(std::mem::replace(existing, elf));
                }
            }
        }
        None
    }

    /// Enumerate every loaded account with the facts a fidelity certificate
    /// needs — provenance and hashing are derived from this in the scope layer.
    pub(crate) fn loaded_info(&self) -> Vec<LoadedInfo> {
        self.loaded
            .iter()
            .map(|(addr, l)| match l {
                Loaded::Data(a) => LoadedInfo {
                    address: addr.to_string(),
                    is_program: false,
                    owner_is_system: a.owner == Address::default(),
                    data_len: a.data.len(),
                    hash: solana_blake3_hasher::hash(&a.data).to_string(),
                },
                Loaded::Program(elf) => LoadedInfo {
                    address: addr.to_string(),
                    is_program: true,
                    owner_is_system: false,
                    data_len: elf.len(),
                    hash: solana_blake3_hasher::hash(elf).to_string(),
                },
            })
            .collect()
    }

    /// Flip runtime feature gates for subsequent runs (see [`FeatureToggle`]).
    /// Every run rebuilds its SVM, so this takes effect on the next replay.
    pub(crate) fn set_feature_toggles(&mut self, toggles: Vec<FeatureToggle>) {
        self.feature_toggles = toggles;
    }

    /// Seed the clock with the transaction's real slot, and derive the epoch and
    /// wall-clock time from it. LiteSVM starts at epoch 0 / timestamp 0, which
    /// would make any date-based program logic nonsense, so we anchor to reality:
    /// Solana's genesis (2020-03-16) plus ~400ms per slot.
    fn base_clock(&self, clock: &mut Clock) {
        let Some(slot) = self.slot else { return };
        clock.slot = slot;
        clock.epoch = slot / SLOTS_PER_EPOCH as u64;
        // Prefer the real block time; fall back to a genesis estimate only if the
        // RPC didn't give us one (that estimate can be months off).
        const GENESIS_UNIX: i64 = 1_584_368_940; // 2020-03-16, mainnet genesis
        clock.unix_timestamp = self
            .block_time
            .unwrap_or_else(|| GENESIS_UNIX + (slot as f64 * SECS_PER_SLOT) as i64);
        clock.epoch_start_timestamp =
            clock.unix_timestamp - ((slot % SLOTS_PER_EPOCH as u64) as f64 * SECS_PER_SLOT) as i64;
        clock.leader_schedule_epoch = clock.epoch + 1;
    }

    /// A human description of the clock this context replays with, e.g.
    /// "slot 487,817,148 · epoch 1128 · 2026-09-01 04:12 UTC".
    pub(crate) fn describe_clock(&self) -> String {
        let svm = LiteSVM::new();
        let mut clock = svm.get_sysvar::<Clock>();
        self.base_clock(&mut clock);
        self.time_travel.apply(&mut clock);
        self.time_travel.describe(&clock)
    }

    /// A pristine SVM loaded with the reconstructed state, its clock advanced to
    /// the transaction's slot (and then by any requested time travel).
    pub(crate) fn fresh_svm(&self) -> LiteSVM {
        self.fresh_svm_with(false)
    }

    /// [`Self::fresh_svm`], optionally with BPF register tracing enabled.
    pub(crate) fn fresh_svm_with(&self, tracing: bool) -> LiteSVM {
        let mut svm = svm_from_loaded_with(
            &self.loaded,
            &self.feature_toggles,
            self.slot.unwrap_or(0),
            tracing,
        );
        let mut clock = svm.get_sysvar::<Clock>();
        self.base_clock(&mut clock);
        if !self.time_travel.is_noop() {
            self.time_travel.apply(&mut clock);
        }
        svm.set_sysvar::<Clock>(&clock);
        svm
    }

    /// Replay and report **what changed** among the loaded accounts — each one's
    /// lamports and raw data, before and after. The caller decodes the bytes into
    /// named fields. Accounts *created* by the transaction aren't in the loaded
    /// set, so they don't appear here.
    pub(crate) fn run_with_diff(
        &self,
        mutations: &[Mutation],
    ) -> Result<(ReplayResult, Vec<RawAccountDiff>)> {
        let (result, svm) = self.run_full(mutations)?;
        let mut diffs = Vec::new();
        for (addr, l) in &self.loaded {
            let Loaded::Data(before) = l else { continue };
            let Some(after) = svm.get_account(addr) else {
                continue;
            };
            if after.lamports == before.lamports && after.data == before.data {
                continue; // untouched
            }
            diffs.push(RawAccountDiff {
                address: addr.to_string(),
                owner: after.owner.to_string(),
                lamports_before: before.lamports,
                lamports_after: after.lamports,
                data_before: before.data.clone(),
                data_after: after.data.clone(),
            });
        }
        Ok((result, diffs))
    }

    /// Replay with `mutations`, then read one account's raw post-execution state
    /// — the primitive historical reconstruction chains: inject an account's
    /// reconstructed bytes, replay the next write, read the account out again.
    pub(crate) fn run_and_read_account(
        &self,
        mutations: &[Mutation],
        address: &str,
    ) -> Result<(ReplayResult, Option<Account>)> {
        let (result, svm) = self.run_full(mutations)?;
        let acc = Address::from_str(address)
            .ok()
            .and_then(|a| svm.get_account(&a));
        Ok((result, acc))
    }

    /// The pre-transaction state of an account (for delta assertions).
    fn pre_account(&self, address: &str) -> Option<&Account> {
        let addr = Address::from_str(address).ok()?;
        self.loaded.iter().find_map(|(a, l)| match l {
            Loaded::Data(acc) if *a == addr => Some(acc),
            _ => None,
        })
    }

    /// Whether an address is part of the replay's world at all (a loaded data
    /// account or program). An assertion against an address that is *not* known
    /// is a typo, not an account that happens to be empty — the difference
    /// between a hard error and a check that silently reads zero and passes.
    fn is_known(&self, address: &str) -> bool {
        match Address::from_str(address) {
            Ok(addr) => self.loaded.iter().any(|(a, _)| *a == addr),
            Err(_) => false,
        }
    }

    /// Register a program's IDL for named-field assertion resolution.
    pub(crate) fn add_idl(&mut self, owner: String, idl: serde_json::Value) {
        self.idls.insert(owner, idl);
    }

    /// The registered IDLs, by program id.
    pub(crate) fn idl_map(&self) -> &HashMap<String, serde_json::Value> {
        &self.idls
    }

    /// Every program whose IDL could matter here: loaded programs (error-name
    /// resolution) plus each distinct data-account owner (named-field decode).
    pub(crate) fn interesting_programs(&self) -> Vec<String> {
        let mut seen = std::collections::HashSet::new();
        for (addr, l) in &self.loaded {
            match l {
                Loaded::Program(_) => {
                    seen.insert(addr.to_string());
                }
                Loaded::Data(acc) => {
                    seen.insert(acc.owner.to_string());
                }
            }
        }
        seen.into_iter().collect()
    }

    /// Add one feature toggle on top of those already set.
    pub(crate) fn push_feature_toggle(&mut self, t: FeatureToggle) {
        self.feature_toggles.push(t);
    }

    /// Decode an account's pre-state into named fields: built-in layouts (SPL
    /// token & friends) first, then the owner program's registered IDL. The
    /// pre-state layout is authoritative for both sides of a delta — an assert
    /// shouldn't silently change meaning because the tx rewrote the account.
    fn decode_pre(&self, address: &str) -> Result<(crate::decode::DecodedAccount, &Account)> {
        let pre = self
            .pre_account(address)
            .ok_or_else(|| Error::AccountNotFound(format!("{address} (no pre-state)")))?;
        let owner = pre.owner.to_string();
        crate::decode::decode_bytes(&owner, &pre.data)
            .or_else(|| {
                self.idls
                    .get(&owner)
                    .and_then(|i| crate::idl::decode_with_idl(i, &pre.data))
            })
            .map(|dec| (dec, pre))
            .ok_or_else(|| Error::UndecodableAccount {
                address: address.to_string(),
                owner,
            })
    }
}

/// The transaction's *pre-execution* state, recovered for free from its own
/// metadata (`meta.preTokenBalances`) — no archival RPC.
///
/// Live account state drifts after a transaction lands, which is why replaying a
/// swap against current state usually slips. But `getTransaction` reports the
/// exact SPL token amount each account held *just before* the transaction ran, so
/// we restore those — making constant-product pool reserves faithful again.
///
/// (Only token accounts that existed pre-transaction appear in `preTokenBalances`,
/// so restoring them is safe — it never resurrects an account the tx creates.)
const SPL_TOKEN_PROGRAM: &str = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA";
/// The Rent sysvar. Always loaded with the world (see [`with_rent_sysvar`]) so
/// the replay enforces the *cluster's* rent parameters, not LiteSVM's defaults.
pub(crate) const SYSVAR_RENT: &str = "SysvarRent111111111111111111111111111111111";

/// Add the Rent sysvar to a key list unless the transaction already references
/// it. Mainnet lowered rent (6,333 lamports per byte-year at a 1.0 exemption
/// threshold, versus the 3,480 × 2.0 default LiteSVM starts from), so an account
/// created since then holds *less* than the default minimum: every
/// `ConstraintRentExempt` check and every system-program rent check on such an
/// account would fail in a replay of a transaction that succeeded on-chain.
pub(crate) fn with_rent_sysvar(keys: &mut Vec<String>) {
    if !keys.iter().any(|k| k == SYSVAR_RENT) {
        keys.push(SYSVAR_RENT.to_string());
    }
}

/// The rent parameters encoded in a Rent sysvar account (`u64` lamports per
/// byte-year, `f64` exemption threshold, `u8` burn percent, little-endian).
/// Test-only: production code loads the sysvar bytes verbatim and lets LiteSVM
/// decode them.
#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct RentParams {
    pub(crate) lamports_per_byte_year: u64,
    pub(crate) exemption_threshold: f64,
    pub(crate) burn_percent: u8,
}

#[cfg(test)]
impl RentParams {
    /// The rent-exempt minimum for an account with `data_len` bytes of data.
    pub(crate) fn minimum_balance(&self, data_len: usize) -> u64 {
        const ACCOUNT_STORAGE_OVERHEAD: u64 = 128;
        (((ACCOUNT_STORAGE_OVERHEAD + data_len as u64) * self.lamports_per_byte_year) as f64
            * self.exemption_threshold) as u64
    }
}

#[cfg(test)]
pub(crate) fn parse_rent(data: &[u8]) -> Option<RentParams> {
    if data.len() < 17 {
        return None;
    }
    Some(RentParams {
        lamports_per_byte_year: u64::from_le_bytes(data[0..8].try_into().ok()?),
        exemption_threshold: f64::from_le_bytes(data[8..16].try_into().ok()?),
        burn_percent: data[16],
    })
}

/// Pre-transaction token-account details, enough to rebuild a closed one.
struct TokenInfo {
    mint: String,
    owner: String, // the token authority (SPL "owner" field), not the program
    amount: u64,
}

#[derive(Default)]
pub(crate) struct PreState {
    /// account address -> raw SPL token amount before the transaction.
    token_amounts: HashMap<String, u64>,
    /// account address -> its lamports before the transaction.
    lamports: HashMap<String, u64>,
    /// token account address -> details, to reconstruct it if it's since closed.
    token_info: HashMap<String, TokenInfo>,
}

impl PreState {
    pub(crate) fn from_meta(tx: &serde_json::Value, account_keys: &[String]) -> PreState {
        let mut ps = PreState::default();

        // Pre-transaction lamports, parallel to the resolved account list.
        if let Some(pre) = tx["meta"]["preBalances"].as_array() {
            for (i, v) in pre.iter().enumerate() {
                if let (Some(addr), Some(l)) = (account_keys.get(i), v.as_u64()) {
                    ps.lamports.insert(addr.clone(), l);
                }
            }
        }
        // Pre-transaction token balances (amount + enough to rebuild the account).
        if let Some(pre) = tx["meta"]["preTokenBalances"].as_array() {
            for e in pre {
                let idx = e["accountIndex"].as_u64().unwrap_or(u64::MAX) as usize;
                let amt = e["uiTokenAmount"]["amount"]
                    .as_str()
                    .and_then(|s| s.parse::<u64>().ok());
                let (Some(addr), Some(amt)) = (account_keys.get(idx), amt) else {
                    continue;
                };
                ps.token_amounts.insert(addr.clone(), amt);
                if let (Some(mint), Some(owner)) = (e["mint"].as_str(), e["owner"].as_str()) {
                    ps.token_info.insert(
                        addr.clone(),
                        TokenInfo {
                            mint: mint.to_string(),
                            owner: owner.to_string(),
                            amount: amt,
                        },
                    );
                }
            }
        }
        ps
    }

    /// Reconstruct an account that existed before the transaction but has since
    /// been closed (so `getMultipleAccounts` returns null). Returns `None` for
    /// accounts that never existed (the transaction creates those itself).
    fn reconstruct(&self, address: &str) -> Option<Account> {
        // A closed token account: rebuild the full SPL layout from meta.
        if let Some(info) = self.token_info.get(address) {
            let mut data = vec![0u8; 165];
            let mint = Address::from_str(&info.mint).ok()?;
            let owner = Address::from_str(&info.owner).ok()?;
            data[0..32].copy_from_slice(mint.as_array());
            data[32..64].copy_from_slice(owner.as_array());
            data[64..72].copy_from_slice(&info.amount.to_le_bytes());
            data[108] = 1; // AccountState::Initialized
            return Some(Account {
                lamports: self.lamports.get(address).copied().unwrap_or(2_039_280),
                data,
                owner: Address::from_str(SPL_TOKEN_PROGRAM).ok()?,
                executable: false,
                rent_epoch: 0,
            });
        }
        // A closed system account (e.g. a fee payer that has since drained): a
        // system-owned account with its pre-transaction lamports is faithful.
        match self.lamports.get(address) {
            Some(&l) if l > 0 => Some(Account {
                lamports: l,
                data: vec![],
                owner: Address::default(), // system program (all-zero id)
                executable: false,
                rent_epoch: 0,
            }),
            _ => None,
        }
    }

    fn is_empty(&self) -> bool {
        self.token_amounts.is_empty() && self.lamports.is_empty()
    }
}

/// Fetch everything needed to replay `signature` (transaction + all touched
/// accounts + program ELFs, including Address Lookup Table accounts) once.
/// `pre_state` restores pre-transaction token balances so swaps replay faithfully
/// instead of slipping. (`_tx_slot` is the transaction's original slot, kept for
/// the signature's sake but no longer used to set the clock — see below.)
pub(crate) fn build_context(
    client: &RpcClient,
    signature: &str,
    account_keys: &[String],
    _tx_slot: Option<u64>,
    pre_state: &PreState,
) -> Result<ReplayContext> {
    // Replay runs against CURRENT account state — an RPC returns today's accounts,
    // not the tx-time snapshot. Anchoring the clock to the transaction's ORIGINAL
    // slot then contradicts that state: a program that checks the clock against an
    // account's (now newer) last-updated timestamp reverts with InvalidTimestamp
    // (e.g. Orca's oracle check). So we anchor the clock to NOW, consistent with the
    // accounts we actually loaded, and let time travel warp forward from there.
    let slot = client.get_slot().ok().or(_tx_slot);
    let block_time = slot.and_then(|s| client.get_block_time(s).ok());
    let tx = fetch_transaction(client, signature)?;
    let mut all_keys = account_keys.to_vec();
    if let Some(lookups) = tx.message.address_table_lookups() {
        for l in lookups {
            all_keys.push(l.account_key.to_string());
        }
    }
    with_rent_sysvar(&mut all_keys);
    let (mut loaded, existing) = fetch_loaded(client, &all_keys)?;

    if !pre_state.is_empty() {
        // Reconstruct accounts that existed at tx-time but are *closed now* (null
        // on-chain), so the transaction can load — a missing fee payer alone fails
        // it at cu=0. Crucially we key off `existing` (what getMultipleAccounts
        // actually returned), not what loaded: a program whose ELF failed to
        // resolve still exists, and must not be rebuilt as a data account.
        for key in account_keys {
            if existing.contains_key(key) {
                continue;
            }
            if let (Some(acc), Ok(addr)) = (pre_state.reconstruct(key), Address::from_str(key)) {
                loaded.push((addr, Loaded::Data(acc)));
            }
        }

        // Rewind still-existing accounts to their pre-transaction balances,
        // reconstructed from the transaction's own metadata (free on any RPC):
        // SOL/lamport balances from `preBalances`, SPL token amounts from
        // `preTokenBalances`. This is metadata reconstruction — faithful for
        // balances, though account *data* is still current-state.
        for (addr, l) in loaded.iter_mut() {
            if let Loaded::Data(acc) = l {
                let key = addr.to_string();
                if let Some(&lamports) = pre_state.lamports.get(&key) {
                    acc.lamports = lamports;
                }
                if let Some(&amt) = pre_state.token_amounts.get(&key) {
                    if acc.data.len() >= 72 {
                        acc.data[64..72].copy_from_slice(&amt.to_le_bytes());
                    }
                }
            }
        }
    }

    Ok(ReplayContext {
        signature: signature.to_string(),
        tx,
        loaded,
        slot,
        block_time,
        time_travel: TimeTravel::default(),
        idls: HashMap::new(),
        feature_toggles: Vec::new(),
    })
}

/// `state_slot` is the archival boundary accounts are fetched at; `clock_slot`
/// anchors the replay clock. They differ when replaying a transaction *at its
/// own slot S*: the archive answers "latest version at or before S", which for
/// accounts the transaction itself wrote is **post**-transaction state — so the
/// caller passes `state_slot = S - 1` (end of the previous slot) and patches
/// same-slot predecessors' balance effects from the transaction's own recorded
/// pre-balances via `pre_state`. Same-slot predecessor *data* writes remain the
/// one disclosed gap of a slot-granular archive.
pub(crate) fn build_context_at_slot(
    archive: &RpcClient,
    signature: &str,
    account_keys: &[String],
    state_slot: u64,
    clock_slot: u64,
    tx_block_time: Option<i64>,
    pre_state: &PreState,
) -> Result<ReplayContext> {
    let tx = fetch_transaction(archive, signature)?;
    let mut all_keys = account_keys.to_vec();

    if let Some(lookups) = tx.message.address_table_lookups() {
        for l in lookups {
            all_keys.push(l.account_key.to_string());
        }
    };
    with_rent_sysvar(&mut all_keys);
    let (mut loaded, existing) = fetch_loaded_at_slot(archive, &all_keys, state_slot)?;

    if !pre_state.is_empty() {
        for key in account_keys {
            if existing.contains_key(key) {
                continue;
            }
            if let (Some(acc), Ok(addr)) = (pre_state.reconstruct(key), Address::from_str(key)) {
                loaded.push((addr, Loaded::Data(acc)));
            }
        }

        // The transaction's own recorded pre-balances are ground truth at its
        // boundary — they override the archive wherever they disagree (a
        // same-slot predecessor transaction wrote the account after S-1).
        for (addr, l) in loaded.iter_mut() {
            if let Loaded::Data(acc) = l {
                let key = addr.to_string();
                if let Some(&lamports) = pre_state.lamports.get(&key) {
                    acc.lamports = lamports;
                }
                if let Some(&amt) = pre_state.token_amounts.get(&key) {
                    if acc.data.len() >= 72 {
                        acc.data[64..72].copy_from_slice(&amt.to_le_bytes());
                    }
                }
            }
        }
    }

    Ok(ReplayContext {
        signature: signature.to_string(),
        tx,
        loaded,
        slot: Some(clock_slot),
        block_time: tx_block_time,
        time_travel: TimeTravel::default(),
        idls: HashMap::new(),
        feature_toggles: Vec::new(),
    })
}

fn b64_encode(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

/// Resolve an (unsigned) transaction's Address Lookup Table references to concrete
/// addresses — writable first, then readonly, the order the runtime resolves them.
/// An ALT account stores its address list at offset 56, 32 bytes each.
pub(crate) fn resolve_alt_addresses(
    client: &RpcClient,
    tx: &VersionedTransaction,
) -> (Vec<String>, Vec<String>) {
    let mut writable = Vec::new();
    let mut readonly = Vec::new();
    if let Some(lookups) = tx.message.address_table_lookups() {
        for l in lookups {
            let Some(data) = fetch_account_data(client, &l.account_key.to_string()) else {
                continue;
            };
            let read = |idx: u8| -> Option<String> {
                let off = 56 + idx as usize * 32;
                let bytes: [u8; 32] = data.get(off..off + 32)?.try_into().ok()?;
                Some(Address::from(bytes).to_string())
            };
            for &idx in &l.writable_indexes {
                if let Some(a) = read(idx) {
                    writable.push(a);
                }
            }
            for &idx in &l.readonly_indexes {
                if let Some(a) = read(idx) {
                    readonly.push(a);
                }
            }
        }
    }
    (writable, readonly)
}

/// Build a replay context for an **unsigned / pre-flight** transaction — one that
/// hasn't been sent yet. Resolves its accounts (incl. ALTs), loads their *current*
/// on-chain state (which, for a not-yet-sent tx, IS the pre-state — no drift, no
/// archival), and sets the slot to the current slot so ALT resolution works.
pub(crate) fn preflight_context(
    client: &RpcClient,
    tx: VersionedTransaction,
) -> Result<ReplayContext> {
    let mut keys: Vec<String> = tx
        .message
        .static_account_keys()
        .iter()
        .map(|k| k.to_string())
        .collect();
    let (writable, readonly) = resolve_alt_addresses(client, &tx);
    keys.extend(writable);
    keys.extend(readonly);

    // Also load the ALT accounts themselves so LiteSVM can resolve the lookups.
    let mut all_keys = keys.clone();
    if let Some(lookups) = tx.message.address_table_lookups() {
        for l in lookups {
            all_keys.push(l.account_key.to_string());
        }
    }
    with_rent_sysvar(&mut all_keys);
    let (loaded, _existing) = fetch_loaded(client, &all_keys)?;
    let slot = client.get_slot().ok();
    // Real wall-clock time for that slot — a pre-flight simulation should run
    // "now", and any date-based program logic depends on this being accurate.
    let block_time = slot.and_then(|s| client.get_block_time(s).ok());
    let signature = tx
        .signatures
        .first()
        .map(|s| s.to_string())
        .unwrap_or_default();
    Ok(ReplayContext {
        signature,
        tx,
        loaded,
        slot,
        block_time,
        time_travel: TimeTravel::default(),
        idls: HashMap::new(),
        feature_toggles: Vec::new(),
    })
}

impl ReplayContext {
    /// Freeze this context into a portable, self-contained [`Fixture`] — the tx
    /// wire bytes plus every account/program, so it can replay offline forever.
    ///
    /// The captured slot and block time come from the context itself (`self.slot`
    /// / `self.block_time`) — the exact clock the live replay ran with — so a
    /// reloaded fixture reproduces the live outcome instead of drifting to the
    /// transaction's original slot.
    pub(crate) fn to_fixture(&self) -> Result<Fixture> {
        let tx_bytes = bincode::serialize(&self.tx)
            .map_err(|e| Error::Fixture(format!("serialize transaction: {e}")))?;
        let entries = self
            .loaded
            .iter()
            .map(|(addr, l)| match l {
                Loaded::Data(a) => FixtureEntry::Data {
                    address: addr.to_string(),
                    owner: a.owner.to_string(),
                    lamports: a.lamports,
                    data_b64: b64_encode(&a.data),
                },
                Loaded::Program(elf) => FixtureEntry::Program {
                    address: addr.to_string(),
                    elf_b64: b64_encode(elf),
                },
            })
            .collect();
        Ok(Fixture {
            version: crate::fixture::FIXTURE_VERSION,
            signature: self.signature.clone(),
            captured_slot: self.slot,
            captured_block_time: self.block_time,
            tx_b64: b64_encode(&tx_bytes),
            entries,
            idls: self
                .idls
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
            recorded: None, // the Replay layer fills this in — it knows the outcome
        })
    }

    /// Rebuild a replay context from a frozen fixture — **no network**. This is
    /// what makes fixture-backed suites deterministic and CI-safe.
    pub(crate) fn from_fixture(fx: &Fixture) -> Result<ReplayContext> {
        let tx_bytes = base64::engine::general_purpose::STANDARD
            .decode(&fx.tx_b64)
            .map_err(|e| Error::Fixture(format!("bad tx base64: {e}")))?;
        let tx: VersionedTransaction = bincode::deserialize(&tx_bytes)
            .map_err(|e| Error::Fixture(format!("deserialize tx: {e}")))?;

        let mut loaded = Vec::with_capacity(fx.entries.len());
        for e in &fx.entries {
            match e {
                FixtureEntry::Data {
                    address,
                    owner,
                    lamports,
                    data_b64,
                } => {
                    let addr = Address::from_str(address)
                        .map_err(|_| Error::Fixture(format!("bad address {address}")))?;
                    let owner_addr = Address::from_str(owner)
                        .map_err(|_| Error::Fixture(format!("bad owner {owner}")))?;
                    let data = base64::engine::general_purpose::STANDARD
                        .decode(data_b64)
                        .map_err(|e| {
                            Error::Fixture(format!("bad data base64 for {address}: {e}"))
                        })?;
                    loaded.push((
                        addr,
                        Loaded::Data(Account {
                            lamports: *lamports,
                            data,
                            owner: owner_addr,
                            executable: false,
                            rent_epoch: 0,
                        }),
                    ));
                }
                FixtureEntry::Program { address, elf_b64 } => {
                    let addr = Address::from_str(address)
                        .map_err(|_| Error::Fixture(format!("bad address {address}")))?;
                    let elf = base64::engine::general_purpose::STANDARD
                        .decode(elf_b64)
                        .map_err(|e| {
                            Error::Fixture(format!("bad elf base64 for {address}: {e}"))
                        })?;
                    loaded.push((addr, Loaded::Program(elf)));
                }
            }
        }
        Ok(ReplayContext {
            signature: fx.signature.clone(),
            tx,
            loaded,
            slot: fx.captured_slot,
            block_time: fx.captured_block_time,
            time_travel: TimeTravel::default(),
            // v2 fixtures carry their IDLs, so named-field asserts, error
            // names, and decoded diffs resolve offline too.
            idls: fx
                .idls
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
            feature_toggles: Vec::new(),
        })
    }
}

/// Fetch the raw transaction (base64 wire bytes) and deserialize it.
fn fetch_transaction(client: &RpcClient, signature: &str) -> Result<VersionedTransaction> {
    let resp: serde_json::Value = client
        .send(
            RpcRequest::GetTransaction,
            json!([signature, { "encoding": "base64", "maxSupportedTransactionVersion": 0 }]),
        )
        .map_err(Error::rpc)?;
    if resp.is_null() {
        return Err(Error::TransactionNotFound(signature.to_string()));
    }
    let tx_b64 = resp["transaction"][0].as_str().ok_or_else(|| {
        Error::MalformedRpcResponse(format!("no base64 transaction in response for {signature}"))
    })?;
    let bytes = b64_decode(tx_b64);
    bincode::deserialize::<VersionedTransaction>(&bytes)
        .map_err(|e| Error::TxDecode(format!("{signature}: {e}")))
}

/// A change to apply to an account before replaying.
///
/// `Data` replaces an account's bytes wholesale; `DataPatch` overwrites a slice
/// at an offset; `Field` sets a *named* field with no offsets at all (how you'd
/// flip a token balance, an oracle price, or a vesting timestamp by name).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mutation {
    /// Set an account's lamport balance.
    Lamports {
        /// The account to mutate.
        address: String,
        /// The new lamport balance.
        value: u64,
    },
    /// Replace an account's data wholesale.
    Data {
        /// The account to mutate.
        address: String,
        /// The complete replacement data.
        bytes: Vec<u8>,
    },
    /// Overwrite a slice of an account's data at an offset.
    DataPatch {
        /// The account to mutate.
        address: String,
        /// Byte offset where the patch begins.
        offset: usize,
        /// The bytes to write at that offset.
        bytes: Vec<u8>,
    },
    /// Set a named argument of a top-level instruction, re-encoded through the
    /// program's IDL — "what if the slippage were 5%?". Resolves to
    /// [`Mutation::IxData`] before application; only fixed-size scalar
    /// arguments preceded by fixed-size arguments can be located.
    IxArg {
        /// Zero-based index of the top-level instruction.
        index: usize,
        /// The argument's IDL name.
        arg: String,
        /// The new value (number, bool, or base58 string for a pubkey).
        value: serde_json::Value,
    },
    /// Overwrite bytes of a top-level instruction's data at an offset.
    IxData {
        /// Zero-based index of the top-level instruction.
        index: usize,
        /// Byte offset into the instruction data.
        offset: usize,
        /// The bytes to write.
        bytes: Vec<u8>,
    },
    /// Remove top-level instruction `index` (its original position) from the
    /// transaction before replaying: "what if this instruction were not
    /// here?" Later instructions shift up; instruction-index mutations still
    /// refer to original positions.
    SkipIx {
        /// Zero-based position among the transaction's top-level instructions.
        index: usize,
    },
    /// Replace top-level instruction `index`'s data wholesale (any length),
    /// for arguments the fixed-offset patch cannot express.
    IxDataReplace {
        /// Zero-based position among the transaction's top-level instructions.
        index: usize,
        /// The new instruction data.
        bytes: Vec<u8>,
    },
    /// Move a top-level instruction from position `from` to position `to`
    /// (positions after skips are applied): "what if it ran earlier/later?"
    MoveIx {
        /// Current position.
        from: usize,
        /// Destination position.
        to: usize,
    },
    /// Set a **named** integer/bool field — no byte offsets. The field resolves
    /// through the account's decoded layout (SPL layouts, or the owner
    /// program's IDL) exactly like [`crate::Check`]'s named-field asserts:
    /// matched by exact name or unambiguous final dot-segment, with unknown
    /// names erroring and listing the fields that do exist.
    Field {
        /// The account to mutate.
        address: String,
        /// The field's name (e.g. `"amount"`, or a dotted path like
        /// `"pool.reserve_a"`).
        field: String,
        /// The new value. Must fit the field's declared type — an
        /// out-of-range value is a hard error, never a silent truncation.
        value: i128,
    },
    /// Reassign the program that **owns** an account (not a data field — the
    /// account's owner program itself). Breaks any program that checks it owns
    /// its accounts.
    Owner {
        /// The account to mutate.
        address: String,
        /// The new owner program, base58.
        owner: String,
    },
}

impl Mutation {
    /// Set an account's lamports.
    pub fn lamports(address: impl Into<String>, value: u64) -> Mutation {
        Mutation::Lamports {
            address: address.into(),
            value,
        }
    }

    /// Replace an account's data wholesale.
    pub fn data(address: impl Into<String>, bytes: Vec<u8>) -> Mutation {
        Mutation::Data {
            address: address.into(),
            bytes,
        }
    }

    /// Reassign an account's owner program.
    pub fn owner(address: impl Into<String>, owner: impl Into<String>) -> Mutation {
        Mutation::Owner {
            address: address.into(),
            owner: owner.into(),
        }
    }

    /// Overwrite a slice of an account's data at `offset`.
    pub fn patch(address: impl Into<String>, offset: usize, bytes: Vec<u8>) -> Mutation {
        Mutation::DataPatch {
            address: address.into(),
            offset,
            bytes,
        }
    }

    /// Set a named integer/bool field of a decoded account — the mutation-side
    /// twin of `Check::account(addr).field(name, …)`. Resolved through SPL
    /// layouts or the program's IDL; the value must fit the field's type.
    ///
    /// ```no_run
    /// # use svmscope::Mutation;
    /// let m = Mutation::field("CounterPda111", "count", 99);
    /// ```
    pub fn field(
        address: impl Into<String>,
        field: impl Into<String>,
        value: impl Into<i128>,
    ) -> Mutation {
        Mutation::Field {
            address: address.into(),
            field: field.into(),
            value: value.into(),
        }
    }

    /// Set a top-level instruction's named argument (see [`Mutation::IxArg`]).
    pub fn ix_arg(index: usize, arg: impl Into<String>, value: serde_json::Value) -> Mutation {
        Mutation::IxArg {
            index,
            arg: arg.into(),
            value,
        }
    }

    fn address(&self) -> &str {
        match self {
            Mutation::Lamports { address, .. }
            | Mutation::Data { address, .. }
            | Mutation::DataPatch { address, .. }
            | Mutation::Field { address, .. }
            | Mutation::Owner { address, .. } => address,
            Mutation::IxArg { .. }
            | Mutation::IxData { .. }
            | Mutation::SkipIx { .. }
            | Mutation::IxDataReplace { .. }
            | Mutation::MoveIx { .. } => "",
        }
    }
}

/// Encode `value` as the little-endian bytes of the field's declared type,
/// rejecting anything that doesn't fit exactly. Supports the same fixed-width
/// integer/bool set the named-field *asserts* can read, so the two sides of the
/// DSL stay symmetric.
fn encode_field_value(f: &crate::decode::Field, value: i128) -> Result<Vec<u8>> {
    let out_of_range = || Error::FieldValueOutOfRange {
        field: f.name.clone(),
        ty: f.ty.clone(),
        value,
    };
    match (f.ty.as_str(), f.size) {
        ("bool", _) => match value {
            0 | 1 => Ok(vec![value as u8]),
            _ => Err(out_of_range()),
        },
        ("u8" | "u16" | "u32" | "u64", n @ 1..=8) => {
            let v = u64::try_from(value).map_err(|_| out_of_range())?;
            if n < 8 && (v >> (8 * n)) != 0 {
                return Err(out_of_range());
            }
            Ok(v.to_le_bytes()[..n].to_vec())
        }
        ("i8" | "i16" | "i32" | "i64", n @ 1..=8) => {
            let v = i64::try_from(value).map_err(|_| out_of_range())?;
            if n < 8 {
                let bound = 1i64 << (8 * n - 1);
                if v < -bound || v >= bound {
                    return Err(out_of_range());
                }
            }
            // Two's-complement truncation: the low n bytes of the i64 are the
            // iN encoding for any value already checked to be in range.
            Ok((v as u64).to_le_bytes()[..n].to_vec())
        }
        (ty, _) => Err(Error::NonNumericField {
            field: f.name.clone(),
            ty: ty.to_string(),
        }),
    }
}

/// Apply one mutation to the SVM. Callers validate first (see
/// [`ReplayContext::validate_mutations`]), so a failure here is a hard error,
/// never folded into a "failed replay" — a typo'd address must not satisfy an
/// `expect: revert` scenario.
fn apply_mutation(svm: &mut LiteSVM, m: &Mutation) -> Result<()> {
    if matches!(
        m,
        Mutation::IxArg { .. }
            | Mutation::IxData { .. }
            | Mutation::SkipIx { .. }
            | Mutation::IxDataReplace { .. }
            | Mutation::MoveIx { .. }
    ) {
        return Ok(());
    }
    let address = m.address();
    let addr =
        Address::from_str(address).map_err(|_| Error::InvalidAddress(address.to_string()))?;
    let mut account = svm
        .get_account(&addr)
        .ok_or_else(|| Error::MutationTargetMissing(address.to_string()))?;

    match m {
        Mutation::Lamports { value, .. } => account.lamports = *value,
        Mutation::Data { bytes, .. } => account.data = bytes.clone(),
        Mutation::DataPatch { offset, bytes, .. } => {
            let end = offset
                .checked_add(bytes.len())
                .filter(|&e| e <= account.data.len())
                .ok_or_else(|| Error::PatchOutOfRange {
                    address: address.to_string(),
                    offset: *offset,
                    len: bytes.len(),
                    size: account.data.len(),
                })?;
            account.data[*offset..end].copy_from_slice(bytes);
        }
        // Named-field mutations are resolved into concrete patches by the
        // context (which holds the layouts/IDLs) before application; reaching
        // here unresolved is a caller bug, reported rather than guessed at.
        Mutation::Field { field, .. } => {
            return Err(Error::InvalidSpec(format!(
                "field mutation \"{field}\" was not resolved before application"
            )));
        }
        Mutation::Owner { owner, .. } => {
            account.owner =
                Address::from_str(owner).map_err(|_| Error::InvalidAddress(owner.to_string()))?;
        }
        // Instruction mutations rewrite the transaction, not an account; see
        // `ReplayContext::tx_with_mutations`.
        Mutation::IxArg { .. }
        | Mutation::IxData { .. }
        | Mutation::SkipIx { .. }
        | Mutation::IxDataReplace { .. }
        | Mutation::MoveIx { .. } => return Ok(()),
    }
    svm.set_account(addr, account).map_err(|e| {
        Error::MalformedRpcResponse(format!("set_account failed for {address}: {e:?}"))
    })
}

/// Convert a raw transaction result into a `ReplayResult`.
///
/// This does NOT print — it just extracts the data. The caller (CLI or a UI)
/// decides how to display it.
fn to_replay_result(result: TransactionResult) -> ReplayResult {
    match result {
        Ok(meta) => ReplayResult {
            success: true,
            logs: meta.logs,
            compute_units: meta.compute_units_consumed,
            ..Default::default()
        },
        Err(failed) => ReplayResult {
            success: false,
            error: Some(format!("{:?}", failed.err)),
            logs: failed.meta.logs,
            compute_units: failed.meta.compute_units_consumed,
            ..Default::default()
        },
    }
}

// ---------------------------------------------------------------------------
// Scenario testing: assert expected outcomes across a suite of what-ifs.
// ---------------------------------------------------------------------------

/// The outcome a scenario asserts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Expect {
    /// The transaction must succeed.
    Success,
    /// The transaction must fail (any error).
    Revert,
    /// The transaction must fail *and* the error or a log line contains this text
    /// (e.g. an error code `Custom(6025)` or a message `DivisionByZero`).
    RevertContains(String),
    /// No assertion — just report what happened.
    Any,
}

impl Expect {
    pub(crate) fn matches(&self, r: &ReplayResult) -> bool {
        match self {
            Expect::Success => r.success,
            Expect::Revert => !r.success,
            Expect::RevertContains(s) => {
                !r.success
                    && (r.error.as_deref().is_some_and(|e| e.contains(s))
                        || r.logs.iter().any(|l| l.contains(s)))
            }
            Expect::Any => true,
        }
    }

    pub(crate) fn describe(&self) -> String {
        match self {
            Expect::Success => "succeeds".into(),
            Expect::Revert => "reverts".into(),
            Expect::RevertContains(s) => format!("reverts with \"{s}\""),
            Expect::Any => "any outcome".into(),
        }
    }
}

/// A comparison operator for numeric state assertions.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CmpOp {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

impl CmpOp {
    pub(crate) fn test_i128(self, a: i128, b: i128) -> bool {
        match self {
            CmpOp::Eq => a == b,
            CmpOp::Ne => a != b,
            CmpOp::Lt => a < b,
            CmpOp::Le => a <= b,
            CmpOp::Gt => a > b,
            CmpOp::Ge => a >= b,
        }
    }
    pub(crate) fn symbol(self) -> &'static str {
        match self {
            CmpOp::Eq => "==",
            CmpOp::Ne => "!=",
            CmpOp::Lt => "<",
            CmpOp::Le => "<=",
            CmpOp::Gt => ">",
            CmpOp::Ge => ">=",
        }
    }
}

/// A check on an account's state *after* the transaction replays. This is what
/// makes a scenario a real test — asserting on resulting state, not just pass/fail.
#[derive(Debug, Clone)]
pub(crate) enum StateCheck {
    /// The account's lamports satisfy `op value`.
    Lamports { op: CmpOp, value: i128 },
    /// The little-endian u64 at `offset` satisfies `op value`
    /// (e.g. an SPL token amount at offset 64).
    U64At {
        offset: usize,
        op: CmpOp,
        value: i128,
    },
    /// The *change* in lamports (post - pre) satisfies `op value` (may be negative)
    /// — e.g. "the fee vault gained ≥ N": `LamportsDelta { Ge, N }`.
    LamportsDelta { op: CmpOp, value: i128 },
    /// The *change* in SPL token amount (u64 @ 64, post - pre) satisfies `op value`.
    TokenDelta { op: CmpOp, value: i128 },
    /// A named field of the account's decoded layout satisfies `op value` —
    /// `pool.reserveA >= 1_000` instead of `u64@72`. The name resolves against
    /// the pre-state layout (built-in decoders, then the owner program's IDL);
    /// matched by exact name or by final dot-segment.
    Field {
        name: String,
        op: CmpOp,
        value: i128,
    },
    /// The *change* in a named field (post - pre) satisfies `op value`.
    FieldDelta {
        name: String,
        op: CmpOp,
        value: i128,
    },
    /// The named field's raw bytes are identical before and after — works for
    /// every field type (pubkeys, options, integers), not just numeric ones.
    FieldUnchanged { name: String },
}

/// A post-replay assertion targeting one account.
#[derive(Debug, Clone)]
pub(crate) struct AccountAssert {
    pub(crate) address: String,
    pub(crate) check: StateCheck,
}

/// Read a little-endian u64 at `offset`, or 0 if out of range.
fn read_u64_at(data: &[u8], offset: usize) -> u64 {
    match data.get(offset..offset + 8) {
        Some(s) => u64::from_le_bytes(s.try_into().unwrap()),
        None => 0,
    }
}

/// Find a field in a decoded layout by exact name, or — so `pool.reserveA`
/// works when the field is just `reserveA` — by its final dot-segment. An
/// ambiguous short name (several fields end in it) is an error naming the
/// candidates, not a silent first-match.
pub(crate) fn find_field<'a>(
    dec: &'a crate::decode::DecodedAccount,
    name: &'a str,
) -> Result<&'a crate::decode::Field> {
    if let Some(f) = dec.fields.iter().find(|f| f.name == name) {
        return Ok(f);
    }
    let last = name.rsplit('.').next().unwrap_or(name);
    let hits: Vec<&crate::decode::Field> = dec
        .fields
        .iter()
        .filter(|f| f.name.rsplit('.').next().unwrap_or(&f.name) == last)
        .collect();
    match hits.len() {
        1 => Ok(hits[0]),
        0 => Err(Error::UnknownField {
            field: name.to_string(),
            type_name: dec.type_name.clone(),
            available: dec.fields.iter().map(|f| f.name.clone()).take(12).collect(),
        }),
        _ => Err(Error::AmbiguousField {
            field: name.to_string(),
            candidates: hits.iter().map(|f| f.name.clone()).collect(),
        }),
    }
}

/// Read a named integer field's bytes as a number. Unsigned ints zero-extend,
/// signed ints sign-extend, bool reads as 0/1; anything else (pubkey, string,
/// 128-bit ints) can't be compared numerically and says so.
pub(crate) fn read_field_int(data: &[u8], f: &crate::decode::Field) -> Result<i128> {
    let bytes = f
        .offset
        .checked_add(f.size)
        .and_then(|end| data.get(f.offset..end))
        .ok_or_else(|| Error::OutOfRange {
            what: format!("{} @{}", f.name, f.offset),
            len: data.len(),
        })?;
    let le = |b: &[u8]| -> u128 { b.iter().rev().fold(0u128, |acc, &x| (acc << 8) | x as u128) };
    match (f.ty.as_str(), f.size) {
        ("bool", _) => Ok(bytes.first().is_some_and(|&b| b != 0) as i128),
        ("u8" | "u16" | "u32" | "u64", _) => Ok(le(bytes) as i128),
        ("i8" | "i16" | "i32" | "i64", n) => {
            let raw = le(bytes);
            let shift = 128 - 8 * n as u32;
            Ok(((raw as i128) << shift) >> shift) // sign-extend from n bytes
        }
        (ty, _) => Err(Error::NonNumericField {
            field: f.name.clone(),
            ty: ty.to_string(),
        }),
    }
}

impl AccountAssert {
    fn describe(&self) -> String {
        let a = short(&self.address);
        match &self.check {
            StateCheck::Lamports { op, value } => format!("{a} lamports {} {value}", op.symbol()),
            StateCheck::U64At { offset, op, value } => {
                format!("{a} u64@{offset} {} {value}", op.symbol())
            }
            StateCheck::LamportsDelta { op, value } => {
                format!("{a} lamports Δ {} {value}", op.symbol())
            }
            StateCheck::TokenDelta { op, value } => format!("{a} token Δ {} {value}", op.symbol()),
            StateCheck::Field { name, op, value } => format!("{a} {name} {} {value}", op.symbol()),
            StateCheck::FieldDelta { name, op, value } => {
                format!("{a} {name} Δ {} {value}", op.symbol())
            }
            StateCheck::FieldUnchanged { name } => format!("{a} {name} unchanged"),
        }
    }

    /// Evaluate against the post-replay `svm`; `ctx` supplies pre-transaction values
    /// for the delta checks.
    fn eval(&self, svm: &LiteSVM, ctx: &ReplayContext) -> Result<bool> {
        let addr = Address::from_str(&self.address)
            .map_err(|_| Error::InvalidAddress(self.address.clone()))?;
        let acc = svm.get_account(&addr);
        // A check against an address the replay never loaded is a typo, not an
        // account that is legitimately empty. Erroring here (rather than reading
        // a silent zero) is what stops a mistyped assert from passing vacuously —
        // the same guarantee the mutation path gives via MutationTargetMissing.
        // A known account that is now gone (closed/drained) still reads zero,
        // which is its true post-transaction state.
        if acc.is_none() && !ctx.is_known(&self.address) {
            return Err(Error::AccountNotFound(self.address.clone()));
        }
        match &self.check {
            StateCheck::Lamports { op, value } => {
                Ok(op.test_i128(acc.map(|a| a.lamports).unwrap_or(0) as i128, *value))
            }
            StateCheck::U64At { offset, op, value } => {
                let acc = acc.ok_or_else(|| Error::AccountNotFound(self.address.clone()))?;
                if offset
                    .checked_add(8)
                    .filter(|&e| e <= acc.data.len())
                    .is_none()
                {
                    return Err(Error::OutOfRange {
                        what: format!("u64@{offset}"),
                        len: acc.data.len(),
                    });
                }
                Ok(op.test_i128(read_u64_at(&acc.data, *offset) as i128, *value))
            }
            StateCheck::LamportsDelta { op, value } => {
                let pre = ctx
                    .pre_account(&self.address)
                    .map(|a| a.lamports)
                    .unwrap_or(0) as i128;
                let post = acc.map(|a| a.lamports).unwrap_or(0) as i128;
                Ok(op.test_i128(post - pre, *value))
            }
            StateCheck::TokenDelta { op, value } => {
                let pre = ctx
                    .pre_account(&self.address)
                    .map(|a| read_u64_at(&a.data, 64))
                    .unwrap_or(0) as i128;
                let post = acc.map(|a| read_u64_at(&a.data, 64)).unwrap_or(0) as i128;
                Ok(op.test_i128(post - pre, *value))
            }
            StateCheck::Field { name, op, value } => {
                let (dec, _) = ctx.decode_pre(&self.address)?;
                let f = find_field(&dec, name)?;
                let acc = acc.ok_or_else(|| Error::AccountNotFound(self.address.clone()))?;
                Ok(op.test_i128(read_field_int(&acc.data, f)?, *value))
            }
            StateCheck::FieldDelta { name, op, value } => {
                let (dec, pre) = ctx.decode_pre(&self.address)?;
                let f = find_field(&dec, name)?;
                let acc = acc.ok_or_else(|| Error::AccountNotFound(self.address.clone()))?;
                let delta = read_field_int(&acc.data, f)? - read_field_int(&pre.data, f)?;
                Ok(op.test_i128(delta, *value))
            }
            StateCheck::FieldUnchanged { name } => {
                let (dec, pre) = ctx.decode_pre(&self.address)?;
                let f = find_field(&dec, name)?;
                let acc = acc.ok_or_else(|| Error::AccountNotFound(self.address.clone()))?;
                Ok(field_bytes(&acc.data, f)? == field_bytes(&pre.data, f)?)
            }
        }
    }
}

/// The raw bytes a decoded field occupies — for `coption-*` fields that is the
/// 4-byte tag plus the payload, so a `None → Some(x)` flip counts as a change.
pub(crate) fn field_bytes<'a>(data: &'a [u8], f: &crate::decode::Field) -> Result<&'a [u8]> {
    let span = if f.ty.starts_with("coption-") {
        f.size + 4
    } else {
        f.size
    };
    f.offset
        .checked_add(span)
        .and_then(|end| data.get(f.offset..end))
        .ok_or_else(|| Error::OutOfRange {
            what: format!("{} @{}", f.name, f.offset),
            len: data.len(),
        })
}

fn short(s: &str) -> String {
    // Count by chars, not bytes: an address string is normally ASCII, but this
    // also formats arbitrary user-supplied strings (a typo'd assert address),
    // and byte-slicing one at a non-char-boundary would panic.
    let count = s.chars().count();
    if count > 12 {
        let head: String = s.chars().take(4).collect();
        let tail: String = s.chars().skip(count - 4).collect();
        format!("{head}…{tail}")
    } else {
        s.to_string()
    }
}

/// The result of one post-replay assertion.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct AssertOutcome {
    /// Human description of what was asserted.
    pub description: String,
    /// Whether the assertion held.
    pub pass: bool,
}

/// The result of running one scenario.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct ScenarioOutcome {
    /// The scenario's name.
    pub name: String,
    /// Human description of the transaction-level expectation.
    pub expect: String,
    /// Whether everything asserted (outcome + all state checks) held.
    pub pass: bool,
    /// What the replay actually did.
    pub actual: ReplayResult,
    /// Each state assertion's individual result.
    pub asserts: Vec<AssertOutcome>,
}

impl ReplayContext {
    /// Resolve a named-field mutation into a concrete byte patch using the
    /// account's decoded layout — the same resolution (exact name or
    /// unambiguous final dot-segment) the Check DSL's named-field asserts use.
    /// Offsets come from the *pre-transaction* layout; a preceding `Data`
    /// mutation that replaces the account with a different layout is on the
    /// caller.
    fn resolve_mutation(&self, m: &Mutation) -> Result<Mutation> {
        if let Mutation::IxArg { index, arg, value } = m {
            let ixs = self.tx.message.instructions();
            let ix = ixs.get(*index).ok_or_else(|| {
                Error::InvalidSpec(format!(
                    "instruction index {index} out of range ({} instructions)",
                    ixs.len()
                ))
            })?;
            let keys = self.tx.message.static_account_keys();
            let program = keys
                .get(ix.program_id_index as usize)
                .map(|p| p.to_string())
                .unwrap_or_default();
            let idl = self.idls.get(&program).ok_or_else(|| {
                Error::InvalidSpec(format!(
                    "instruction {index}: program {program} has no IDL, so arguments cannot be re-encoded"
                ))
            })?;
            let def = crate::idl::find_ix(idl, &ix.data).ok_or_else(|| {
                Error::InvalidSpec(format!(
                    "instruction {index}: data does not match any IDL instruction"
                ))
            })?;
            let (offset, size, label) = crate::idl::ix_arg_span(&def, arg).ok_or_else(|| {
                Error::InvalidSpec(format!(
                    "instruction {index}: argument \"{arg}\" is not a fixed-size scalar at a known offset"
                ))
            })?;
            let bytes = crate::idl::encode_fixed(&label, size, value).ok_or_else(|| {
                Error::InvalidSpec(format!(
                    "instruction {index}: value {value} does not fit argument \"{arg}\" ({label})"
                ))
            })?;
            return Ok(Mutation::IxData {
                index: *index,
                offset,
                bytes,
            });
        }
        let Mutation::Field {
            address,
            field,
            value,
        } = m
        else {
            return Ok(m.clone());
        };
        let (decoded, _pre) = self.decode_pre(address)?;
        let f = find_field(&decoded, field)?;
        let bytes = encode_field_value(f, *value)?;
        Ok(Mutation::DataPatch {
            address: address.clone(),
            offset: f.offset,
            bytes,
        })
    }

    /// The transaction with any instruction-data mutations applied (already
    /// resolved to [`Mutation::IxData`]). Account mutations are ignored here.
    fn tx_with_mutations(&self, resolved: &[Mutation]) -> Result<VersionedTransaction> {
        use solana_message::VersionedMessage;
        let mut tx = self.tx.clone();
        for m in resolved {
            let Mutation::IxData {
                index,
                offset,
                bytes,
            } = m
            else {
                continue;
            };
            let ixs: &mut Vec<_> = match &mut tx.message {
                VersionedMessage::Legacy(m) => &mut m.instructions,
                VersionedMessage::V0(m) => &mut m.instructions,
                VersionedMessage::V1(m) => &mut m.instructions,
            };
            let ix = ixs.get_mut(*index).ok_or_else(|| {
                Error::InvalidSpec(format!("instruction index {index} out of range"))
            })?;
            let end = offset
                .checked_add(bytes.len())
                .filter(|&e| e <= ix.data.len())
                .ok_or_else(|| {
                    Error::InvalidSpec(format!(
                        "instruction {index}: patch at {offset}+{} exceeds data length {}",
                        bytes.len(),
                        ix.data.len()
                    ))
                })?;
            ix.data[*offset..end].copy_from_slice(bytes);
        }
        // Whole-data replacements, by original position.
        for m in resolved {
            if let Mutation::IxDataReplace { index, bytes } = m {
                let ixs: &mut Vec<_> = match &mut tx.message {
                    VersionedMessage::Legacy(m) => &mut m.instructions,
                    VersionedMessage::V0(m) => &mut m.instructions,
                    VersionedMessage::V1(m) => &mut m.instructions,
                };
                let ix = ixs.get_mut(*index).ok_or_else(|| {
                    Error::InvalidSpec(format!("instruction index {index} out of range"))
                })?;
                ix.data = bytes.clone();
            }
        }
        // Skips next, so every index above still meant the original position.
        let skipped: std::collections::BTreeSet<usize> = resolved
            .iter()
            .filter_map(|m| match m {
                Mutation::SkipIx { index } => Some(*index),
                _ => None,
            })
            .collect();
        if !skipped.is_empty() {
            let ixs: &mut Vec<_> = match &mut tx.message {
                VersionedMessage::Legacy(m) => &mut m.instructions,
                VersionedMessage::V0(m) => &mut m.instructions,
                VersionedMessage::V1(m) => &mut m.instructions,
            };
            if let Some(&bad) = skipped.iter().find(|&&i| i >= ixs.len()) {
                return Err(Error::InvalidSpec(format!(
                    "instruction index {bad} out of range"
                )));
            }
            if skipped.len() >= ixs.len() {
                return Err(Error::InvalidSpec("cannot skip every instruction".into()));
            }
            let mut i = 0;
            ixs.retain(|_| {
                let keep = !skipped.contains(&i);
                i += 1;
                keep
            });
        }
        // Moves last, in the post-skip numbering.
        for m in resolved {
            if let Mutation::MoveIx { from, to } = m {
                let ixs: &mut Vec<_> = match &mut tx.message {
                    VersionedMessage::Legacy(m) => &mut m.instructions,
                    VersionedMessage::V0(m) => &mut m.instructions,
                    VersionedMessage::V1(m) => &mut m.instructions,
                };
                if *from >= ixs.len() || *to >= ixs.len() {
                    return Err(Error::InvalidSpec(format!(
                        "move {from} → {to}: out of range for {} instructions",
                        ixs.len()
                    )));
                }
                let ix = ixs.remove(*from);
                ixs.insert(*to, ix);
            }
        }
        Ok(tx)
    }

    /// The transaction with `mutations` applied to its instruction data, for
    /// callers that drive the SVM themselves (the profiler).
    pub(crate) fn tx_for(&self, mutations: &[Mutation]) -> Result<VersionedTransaction> {
        let resolved: Vec<Mutation> = mutations
            .iter()
            .map(|m| self.resolve_mutation(m))
            .collect::<Result<_>>()?;
        self.tx_with_mutations(&resolved)
    }

    /// Apply `mutations` to `svm` (the profiler's copy of [`Self::run_full`]'s
    /// setup without executing).
    pub(crate) fn apply_mutations_to(
        &self,
        svm: &mut LiteSVM,
        mutations: &[Mutation],
    ) -> Result<()> {
        for m in mutations {
            apply_mutation(svm, &self.resolve_mutation(m)?)?;
        }
        Ok(())
    }

    /// Run mutations and keep the post-replay SVM so state can be inspected.
    /// A mutation that can't be applied is a hard `Err`, never a failed replay.
    fn run_full(&self, mutations: &[Mutation]) -> Result<(ReplayResult, LiteSVM)> {
        let mut svm = self.fresh_svm();
        let resolved: Vec<Mutation> = mutations
            .iter()
            .map(|m| self.resolve_mutation(m))
            .collect::<Result<_>>()?;
        for m in &resolved {
            apply_mutation(&mut svm, m)?;
        }
        let tx = self.tx_with_mutations(&resolved)?;
        let result = to_replay_result(svm.send_transaction(tx));
        Ok((result, svm))
    }

    /// Check every mutation against the loaded state without running anything:
    /// the address parses, the target account is loaded, and a patch stays in
    /// bounds. Lets a suite fail fast — before any scenario executes.
    pub(crate) fn validate_mutations(&self, mutations: &[Mutation]) -> Result<()> {
        for m in mutations {
            if matches!(
                m,
                Mutation::IxArg { .. }
                    | Mutation::IxData { .. }
                    | Mutation::SkipIx { .. }
                    | Mutation::IxDataReplace { .. }
                    | Mutation::MoveIx { .. }
            ) {
                // Resolving is the validation: index, IDL, offset and fit.
                let r = self.resolve_mutation(m)?;
                self.tx_with_mutations(&[r])?;
                continue;
            }
            let address = m.address();
            let addr = Address::from_str(address)
                .map_err(|_| Error::InvalidAddress(address.to_string()))?;
            let data_len = self
                .loaded
                .iter()
                .find_map(|(a, l)| match l {
                    Loaded::Data(acc) if *a == addr => Some(acc.data.len()),
                    Loaded::Program(_) if *a == addr => Some(0),
                    _ => None,
                })
                .ok_or_else(|| Error::MutationTargetMissing(address.to_string()))?;
            // A named-field mutation validates by fully resolving: the account
            // decodes, the field exists unambiguously, and the value fits.
            let m = self.resolve_mutation(m)?;
            if let Mutation::DataPatch { offset, bytes, .. } = &m {
                if offset
                    .checked_add(bytes.len())
                    .filter(|&e| e <= data_len)
                    .is_none()
                {
                    return Err(Error::PatchOutOfRange {
                        address: address.to_string(),
                        offset: *offset,
                        len: bytes.len(),
                        size: data_len,
                    });
                }
            }
        }
        Ok(())
    }
}

/// Run a suite of scenarios against one pre-fetched context and report pass/fail.
///
/// Every scenario's mutations are validated **before anything runs** — a typo'd
/// mutation address fails the whole suite with a hard error instead of showing
/// up as a revert that an `expect: revert` scenario would silently accept.
///
/// A scenario with no outcome check implicitly asserts success.
pub(crate) fn run_suite(
    ctx: &ReplayContext,
    recorded: Option<&crate::scope::OnchainRecord>,
    scenarios: &[crate::check::Scenario],
) -> Result<Vec<ScenarioOutcome>> {
    use crate::check::CheckKind;

    for s in scenarios {
        ctx.validate_mutations(&s.mutations)?;
    }
    scenarios
        .iter()
        .map(|s| {
            let (actual, svm) = ctx.run_full(&s.mutations)?;

            // Transaction-level outcome: every Outcome check must hold; a
            // scenario that declares none implicitly asserts success — unless it
            // carries a `matches_onchain` check, which is itself the outcome
            // assertion (and would otherwise be contradicted for a transaction
            // whose recorded outcome is a revert).
            let outcomes: Vec<&Expect> = s
                .checks
                .iter()
                .filter_map(|c| match &c.0 {
                    CheckKind::Outcome(e) => Some(e),
                    _ => None,
                })
                .collect();
            let has_matches_onchain = s
                .checks
                .iter()
                .any(|c| matches!(&c.0, CheckKind::MatchesOnchain));
            let implicit = [if has_matches_onchain {
                Expect::Any
            } else {
                Expect::Success
            }];
            let effective: &[&Expect] = if outcomes.is_empty() {
                &[&implicit[0]]
            } else {
                &outcomes
            };
            let outcome_pass = effective.iter().all(|e| e.matches(&actual));
            let expect = effective
                .iter()
                .map(|e| e.describe())
                .collect::<Vec<_>>()
                .join(" & ");

            // Result- and state-level checks all report as assert outcomes.
            let mut asserts: Vec<AssertOutcome> = Vec::new();
            for c in &s.checks {
                match &c.0 {
                    CheckKind::Outcome(_) => {}
                    CheckKind::LogContains(t) => asserts.push(AssertOutcome {
                        description: format!("logs contain \"{t}\""),
                        pass: actual.error.as_deref().is_some_and(|e| e.contains(t))
                            || actual.logs.iter().any(|l| l.contains(t)),
                    }),
                    CheckKind::ComputeUnits(c) => asserts.push(AssertOutcome {
                        description: format!("compute units {} {}", c.op.symbol(), c.value),
                        pass: c.op.test_i128(actual.compute_units as i128, c.value),
                    }),
                    CheckKind::MatchesOnchain => asserts.push(match recorded {
                        Some(r) => AssertOutcome {
                            description: format!(
                                "matches the on-chain outcome ({})",
                                if r.success { "success" } else { "failure" }
                            ),
                            pass: actual.success == r.success,
                        },
                        None => AssertOutcome {
                            description:
                                "matches the on-chain outcome — no recorded outcome available \
                                 (pre-flight tx, or a v1 fixture; re-capture to record it)"
                                    .into(),
                            pass: false,
                        },
                    }),
                    CheckKind::Account(list) => {
                        for a in list {
                            asserts.push(match a.eval(&svm, ctx) {
                                Ok(pass) => AssertOutcome {
                                    description: a.describe(),
                                    pass,
                                },
                                // A failed eval (missing account, unknown field,
                                // bad offset) is a failed assertion — and the
                                // description says why.
                                Err(e) => AssertOutcome {
                                    description: format!("{} — {e}", a.describe()),
                                    pass: false,
                                },
                            });
                        }
                    }
                }
            }

            let pass = outcome_pass && asserts.iter().all(|a| a.pass);
            Ok(ScenarioOutcome {
                name: s.name.clone(),
                expect,
                pass,
                actual,
                asserts,
            })
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Debugger primitives: prefix replays with post-state capture.
// ---------------------------------------------------------------------------

/// One prefix run for the step debugger.
pub(crate) struct RawStepRun {
    /// Top-level instruction indexes that were executed, in message order.
    pub(crate) keep: Vec<usize>,
    /// Account state after each inner instruction (CPI) of this top-level
    /// instruction, in completion order — from the runtime observer, single
    /// run only. Empty when unavailable (prefix mode).
    pub(crate) inner_posts: Vec<InnerSnapshot>,
    /// The runtime outcome (logs, inner instructions, CU, error).
    pub(crate) result: litesvm::types::TransactionResult,
    /// Every account after the run, when it succeeded. `None` on failure: a
    /// failed prefix commits nothing.
    pub(crate) post: Option<Vec<(Address, Account)>>,
}

const COMPUTE_BUDGET_ID: &str = "ComputeBudget111111111111111111111111111111";

impl ReplayContext {
    /// The transaction's signature (empty for pre-flight transactions).
    pub(crate) fn signature(&self) -> &str {
        &self.signature
    }

    /// The transaction being replayed.
    pub(crate) fn transaction(&self) -> &VersionedTransaction {
        &self.tx
    }

    /// The message's full account list in runtime order: static keys, then
    /// ALT-writable, then ALT-readonly — resolved offline from the loaded
    /// lookup-table accounts (address list at offset 56, 32 bytes each).
    pub(crate) fn message_account_keys(&self) -> Vec<String> {
        let mut keys: Vec<String> = self
            .tx
            .message
            .static_account_keys()
            .iter()
            .map(|a| a.to_string())
            .collect();
        let Some(lookups) = self.tx.message.address_table_lookups() else {
            return keys;
        };
        let (mut writable, mut readonly) = (Vec::new(), Vec::new());
        for l in lookups {
            let data = self.loaded.iter().find_map(|(a, ld)| match ld {
                Loaded::Data(acc) if *a == l.account_key => Some(&acc.data),
                _ => None,
            });
            let Some(data) = data else { continue };
            let read = |idx: u8| -> Option<String> {
                let off = 56 + idx as usize * 32;
                let bytes: [u8; 32] = data.get(off..off + 32)?.try_into().ok()?;
                Some(Address::from(bytes).to_string())
            };
            writable.extend(l.writable_indexes.iter().filter_map(|&i| read(i)));
            readonly.extend(l.readonly_indexes.iter().filter_map(|&i| read(i)));
        }
        keys.extend(writable);
        keys.extend(readonly);
        keys
    }

    /// The pre-transaction state of an account, owned. `None` if it did not
    /// exist (or is a program).
    pub(crate) fn pre_account_owned(&self, address: &str) -> Option<Account> {
        self.pre_account(address).cloned()
    }

    /// The transaction with only the top-level instructions at `keep`, in
    /// order. Signatures and header are untouched: sigverify is off, so the
    /// runtime only checks that the signature count matches the header.
    fn tx_with_instructions(
        &self,
        base: &VersionedTransaction,
        keep: &[usize],
    ) -> VersionedTransaction {
        use solana_message::VersionedMessage;
        let all = base.message.instructions();
        let subset: Vec<_> = keep.iter().filter_map(|&i| all.get(i).cloned()).collect();
        let mut tx = base.clone();
        match &mut tx.message {
            VersionedMessage::Legacy(m) => m.instructions = subset,
            VersionedMessage::V0(m) => m.instructions = subset,
            VersionedMessage::V1(m) => m.instructions = subset,
        }
        tx
    }

    /// Replay every instruction prefix `0..=k` against one fresh SVM (read-only
    /// simulations, so the state never advances) and capture the post-state of
    /// each. ComputeBudget instructions are kept in every prefix because the
    /// runtime derives the transaction's limits from the whole message.
    pub(crate) fn trace_raw(&self, mutations: &[Mutation]) -> Result<Vec<RawStepRun>> {
        #[cfg(feature = "single-run-trace")]
        {
            return self.trace_raw_single_run(mutations);
        }
        #[allow(unreachable_code)]
        self.trace_raw_prefixes(mutations)
    }

    /// One execution of the whole transaction. LiteSVM's `after_instruction`
    /// hook hands over the transaction context after each top-level
    /// instruction, so every step's post-state comes from the same run the
    /// chain would have done: the Instructions sysvar is complete, a failure
    /// is the real one, and no step's changes are folded into another's.
    #[cfg(feature = "single-run-trace")]
    fn trace_raw_single_run(&self, mutations: &[Mutation]) -> Result<Vec<RawStepRun>> {
        use std::sync::{Arc, Mutex};
        let mut svm = self.fresh_svm();
        let resolved: Vec<Mutation> = mutations
            .iter()
            .map(|m| self.resolve_mutation(m))
            .collect::<Result<_>>()?;
        for m in &resolved {
            apply_mutation(&mut svm, m)?;
        }
        let tx = self.tx_with_mutations(&resolved)?;
        let n = tx.message.instructions().len();
        let snaps: Arc<Mutex<Vec<InstructionSnapshot>>> = Arc::new(Mutex::new(Vec::new()));
        svm.set_invocation_inspect_callback(StateCollector {
            snaps: Arc::clone(&snaps),
        });
        // Inner instructions: the runtime observer fires after every
        // instruction at every depth, in completion order. Height-1 firings
        // mark the end of a top-level instruction, so the count of those seen
        // so far is the top-level index an inner snapshot belongs to.
        let inner: Arc<Mutex<Vec<(usize, InnerSnapshot)>>> = Arc::new(Mutex::new(Vec::new()));
        {
            let inner = Arc::clone(&inner);
            let mut tops = 0usize;
            solana_program_runtime::instruction_hook::set(Box::new(move |ctx, ok| {
                let height = ctx.get_stack_height();
                if height <= 1 {
                    tops += 1;
                    return;
                }
                let program = ctx
                    .transaction_context
                    .get_current_instruction_context()
                    .ok()
                    .and_then(|ic| ic.get_program_key().ok())
                    .map(|k| k.to_string())
                    .unwrap_or_default();
                let snap = InnerSnapshot {
                    height: height as u8,
                    program,
                    ok,
                    accounts: snapshot_accounts(ctx),
                };
                if let Ok(mut v) = inner.lock() {
                    v.push((tops, snap));
                }
            }));
        }
        let result = svm.simulate_transaction(tx);
        solana_program_runtime::instruction_hook::clear();
        let inner = std::mem::take(&mut *inner.lock().unwrap());
        let snaps = std::mem::take(&mut *snaps.lock().unwrap());
        // The transaction's own verdict is authoritative. A failure tied to an
        // instruction is that instruction's; a failure with no instruction
        // index (a rent check after the last instruction, a rejection before
        // the first) still fails the trace: it is pinned to the last
        // instruction that ran, or to the first when nothing ran, so no step
        // and no run can read as a success the chain never produced.
        let failed_at: Option<usize> = match &result {
            Ok(_) => None,
            Err(failed) => {
                // `InstructionError(<idx>, ..)` carries the instruction; the
                // error type is not a direct dependency, so read the index the
                // same way the rest of the crate does, from its formatting.
                let text = format!("{:?}", failed.err);
                let by_index = text
                    .strip_prefix("InstructionError(")
                    .and_then(|r| r.split(',').next())
                    .and_then(|i| i.trim().parse::<usize>().ok());
                Some(by_index.unwrap_or_else(|| {
                    snaps
                        .iter()
                        .filter(|s| s.ok)
                        .map(|s| s.index)
                        .max()
                        .unwrap_or(0)
                }))
            }
        };
        let keep: Vec<usize> = (0..n).collect();
        let mut runs = Vec::with_capacity(n);
        for k in 0..n {
            let ran_ok = failed_at.is_none_or(|f| k < f);
            let post = if ran_ok {
                snaps
                    .iter()
                    .find(|s| s.index == k && s.ok)
                    .map(|s| s.accounts.clone())
            } else {
                None
            };
            // A step that ran before the failure succeeded in its own right:
            // it gets the run's metadata as a success, so it is never mistaken
            // for a prefix artifact. The failing step and everything after
            // carry the failure.
            let step_result = match (&result, ran_ok) {
                (Ok(info), _) => Ok(info.meta.clone()),
                (Err(failed), true) => Ok(failed.meta.clone()),
                (Err(failed), false) => Err(failed.clone()),
            };
            let inner_posts: Vec<InnerSnapshot> = inner
                .iter()
                .filter(|(top, _)| *top == k)
                .map(|(_, s)| s.clone())
                .collect();
            runs.push(RawStepRun {
                keep: keep.clone(),
                inner_posts,
                result: step_result,
                post,
            });
        }
        Ok(runs)
    }

    #[cfg_attr(feature = "single-run-trace", allow(dead_code))]
    fn trace_raw_prefixes(&self, mutations: &[Mutation]) -> Result<Vec<RawStepRun>> {
        let mut svm = self.fresh_svm();
        let resolved: Vec<Mutation> = mutations
            .iter()
            .map(|m| self.resolve_mutation(m))
            .collect::<Result<_>>()?;
        for m in &resolved {
            apply_mutation(&mut svm, m)?;
        }
        let base = self.tx_with_mutations(&resolved)?;
        let keys = base.message.static_account_keys();
        let n = base.message.instructions().len();
        let is_cb: Vec<bool> = base
            .message
            .instructions()
            .iter()
            .map(|ix| {
                keys.get(ix.program_id_index as usize)
                    .map(|p| p.to_string() == COMPUTE_BUDGET_ID)
                    .unwrap_or(false)
            })
            .collect();
        let mut runs = Vec::with_capacity(n);
        for k in 0..n {
            let keep: Vec<usize> = (0..n).filter(|&i| i <= k || is_cb[i]).collect();
            let tx = self.tx_with_instructions(&base, &keep);
            let (result, post) = match svm.simulate_transaction(tx) {
                Ok(info) => (
                    Ok(info.meta),
                    Some(
                        info.post_accounts
                            .into_iter()
                            .map(|(a, acc)| (a, Account::from(acc)))
                            .collect(),
                    ),
                ),
                Err(failed) => (Err(failed), None),
            };
            runs.push(RawStepRun {
                keep,
                inner_posts: Vec::new(),
                result,
                post,
            });
        }
        Ok(runs)
    }
}

/// The `ReplayResult` view of a raw runtime result (shared with the debugger).
/// Account state after one inner instruction (a CPI), from the runtime observer.
#[derive(Clone)]
pub(crate) struct InnerSnapshot {
    /// Stack height of the instruction (2 = called by a top-level instruction).
    pub(crate) height: u8,
    /// The program that ran.
    pub(crate) program: String,
    /// Whether it succeeded.
    #[allow(dead_code)]
    pub(crate) ok: bool,
    /// Every transaction account as it stood when this instruction finished.
    pub(crate) accounts: Vec<(Address, Account)>,
}

/// Account state after one top-level instruction, from the single-run hook.
#[cfg(feature = "single-run-trace")]
struct InstructionSnapshot {
    index: usize,
    ok: bool,
    accounts: Vec<(Address, Account)>,
}

/// Snapshots the transaction context after every top-level instruction.
#[cfg(feature = "single-run-trace")]
struct StateCollector {
    snaps: std::sync::Arc<std::sync::Mutex<Vec<InstructionSnapshot>>>,
}

#[cfg(feature = "single-run-trace")]
impl litesvm::InvocationInspectCallback for StateCollector {
    fn before_invocation(
        &self,
        _svm: &LiteSVM,
        _tx: &solana_transaction::sanitized::SanitizedTransaction,
        _program_indices: &[solana_transaction_context::IndexOfAccount],
        _invoke_context: &mut solana_program_runtime::invoke_context::InvokeContext,
        _enable_register_tracing: bool,
    ) {
    }

    fn after_invocation(
        &self,
        _svm: &LiteSVM,
        _tx: &solana_transaction::sanitized::SanitizedTransaction,
        _program_indices: &[solana_transaction_context::IndexOfAccount],
        _invoke_context: &solana_program_runtime::invoke_context::InvokeContext,
        _enable_register_tracing: bool,
    ) {
    }

    fn after_instruction(
        &self,
        index: usize,
        invoke_context: &solana_program_runtime::invoke_context::InvokeContext,
        ok: bool,
    ) {
        let accounts = snapshot_accounts(invoke_context);
        if let Ok(mut s) = self.snaps.lock() {
            s.push(InstructionSnapshot {
                index,
                ok,
                accounts,
            });
        }
    }
}

/// Every transaction account as the invoke context currently holds it.
#[cfg(feature = "single-run-trace")]
fn snapshot_accounts(
    invoke_context: &solana_program_runtime::invoke_context::InvokeContext,
) -> Vec<(Address, Account)> {
    use solana_account::ReadableAccount;
    let tc = &*invoke_context.transaction_context;
    let n = tc.get_number_of_accounts();
    let mut accounts = Vec::with_capacity(n as usize);
    for i in 0..n {
        let (Ok(key), Ok(acc)) = (
            tc.get_key_of_account_at_index(i),
            tc.accounts().try_borrow(i),
        ) else {
            continue;
        };
        accounts.push((
            Address::from(key.to_bytes()),
            Account {
                lamports: acc.lamports(),
                data: acc.data().to_vec(),
                owner: Address::from(acc.owner().to_bytes()),
                executable: acc.executable(),
                rent_epoch: acc.rent_epoch(),
            },
        ));
    }
    accounts
}

pub(crate) fn replay_result_of(result: &litesvm::types::TransactionResult) -> ReplayResult {
    to_replay_result(result.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn f(name: &str, ty: &str, size: usize) -> crate::decode::Field {
        crate::decode::Field {
            name: name.into(),
            offset: 0,
            ty: ty.into(),
            size,
            value: String::new(),
            editable: true,
            note: None,
        }
    }

    #[test]
    fn rent_sysvar_from_the_loaded_world_replaces_litesvm_defaults() {
        // Mainnet's post-reduction parameters: 6,333 / byte-year, threshold 1.0.
        let mut data = Vec::new();
        data.extend_from_slice(&6_333u64.to_le_bytes());
        data.extend_from_slice(&1.0f64.to_le_bytes());
        data.push(50);
        let rent = parse_rent(&data).unwrap();
        assert_eq!(rent.lamports_per_byte_year, 6_333);
        assert_eq!(
            rent.minimum_balance(165),
            1_855_569,
            "a token account's minimum on mainnet"
        );
        assert!(parse_rent(&[0u8; 5]).is_none());

        let rent_addr = Address::from_str(SYSVAR_RENT).unwrap();
        let loaded = vec![(
            rent_addr,
            Loaded::Data(Account {
                lamports: 1_009_200,
                data: data.clone(),
                owner: Address::from_str("Sysvar1111111111111111111111111111111111111").unwrap(),
                executable: false,
                rent_epoch: 0,
            }),
        )];
        let svm = svm_from_loaded_with(&loaded, &[], 0, false);
        let stored = svm
            .get_account(&rent_addr)
            .expect("rent sysvar account in the world");
        assert_eq!(parse_rent(&stored.data), Some(rent));
        // The runtime-side cache took it too, not just the account bytes.
        assert_eq!(svm.minimum_balance_for_rent_exemption(165), 1_855_569);
        // Without it, LiteSVM's default (3,480 × 2.0) applies.
        let default = svm_from_loaded_with(&[], &[], 0, false);
        let default_rent = parse_rent(&default.get_account(&rent_addr).unwrap().data).unwrap();
        assert_eq!(default_rent.minimum_balance(165), 2_039_280);

        let mut keys = vec!["X".to_string(), SYSVAR_RENT.to_string()];
        with_rent_sysvar(&mut keys);
        assert_eq!(
            keys.len(),
            2,
            "no duplicate when the tx already references it"
        );
        let mut keys = vec!["X".to_string()];
        with_rent_sysvar(&mut keys);
        assert_eq!(keys.last().map(String::as_str), Some(SYSVAR_RENT));
    }

    #[test]
    fn field_values_encode_le_at_exact_width() {
        assert_eq!(
            encode_field_value(&f("c", "u8", 1), 255).unwrap(),
            vec![255]
        );
        assert_eq!(
            encode_field_value(&f("c", "u64", 8), u64::MAX as i128).unwrap(),
            u64::MAX.to_le_bytes().to_vec()
        );
        assert_eq!(
            encode_field_value(&f("c", "i32", 4), -5).unwrap(),
            (-5i32).to_le_bytes().to_vec()
        );
        assert_eq!(
            encode_field_value(&f("c", "i64", 8), i64::MIN as i128).unwrap(),
            i64::MIN.to_le_bytes().to_vec()
        );
        assert_eq!(encode_field_value(&f("c", "bool", 1), 1).unwrap(), vec![1]);
    }

    #[test]
    fn field_values_out_of_range_are_hard_errors_not_truncations() {
        for (ty, size, value) in [
            ("u8", 1usize, 256i128),
            ("u8", 1, -1),
            ("u16", 2, 65_536),
            ("u64", 8, -1),
            ("u64", 8, u64::MAX as i128 + 1),
            ("i8", 1, 128),
            ("i8", 1, -129),
            ("i64", 8, i64::MAX as i128 + 1),
            ("bool", 1, 2),
        ] {
            assert!(
                matches!(
                    encode_field_value(&f("c", ty, size), value),
                    Err(Error::FieldValueOutOfRange { .. })
                ),
                "{ty} should reject {value}"
            );
        }
    }

    #[test]
    fn non_numeric_field_mutations_are_rejected() {
        assert!(matches!(
            encode_field_value(&f("owner", "pubkey", 32), 1),
            Err(Error::NonNumericField { .. })
        ));
        assert!(matches!(
            encode_field_value(&f("x", "coption-u64", 8), 1),
            Err(Error::NonNumericField { .. })
        ));
    }

    /// A real SPL token account layout (165 bytes) with amount 1234 @ offset 64,
    /// decoded through the same path named-field assertions use.
    fn token_decoded() -> (crate::decode::DecodedAccount, Vec<u8>) {
        let mut data = vec![0u8; 165];
        data[64..72].copy_from_slice(&1234u64.to_le_bytes());
        let dec = crate::decode::decode_bytes("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA", &data)
            .expect("token account decodes");
        (dec, data)
    }

    #[test]
    fn field_resolves_by_name_and_dot_segment() {
        let (dec, data) = token_decoded();
        let f = find_field(&dec, "amount").unwrap();
        assert_eq!(f.offset, 64);
        assert_eq!(read_field_int(&data, f).unwrap(), 1234);
        // `vault.amount` reaches the same field by its final dot-segment.
        assert_eq!(find_field(&dec, "vault.amount").unwrap().offset, 64);
    }

    #[test]
    fn feature_toggle_changes_the_effective_feature_set() {
        // The get_sysvar syscall gate is active on mainnet (our replay baseline).
        let feat = Address::from_str("CLCoTADvV64PSrnR6QXty6Fwrt9Xc6EdxSJE4wLRePjq").unwrap();
        let pk = solana_pubkey::Pubkey::new_from_array(feat.to_bytes());

        // Baseline (no toggles) has it active…
        let base = svm_from_loaded_with(&[], &[], 0, false);
        assert!(
            base.get_feature_set().is_active(&pk),
            "gate should be active by default"
        );

        // …and our deactivate toggle actually removes it from the runtime set.
        let off = svm_from_loaded_with(
            &[],
            &[FeatureToggle {
                id: feat,
                active: false,
            }],
            0,
            false,
        );
        assert!(
            !off.get_feature_set().is_active(&pk),
            "toggle should deactivate the gate"
        );
    }

    #[test]
    fn unknown_field_errors_and_lists_candidates() {
        let (dec, _) = token_decoded();
        let e = find_field(&dec, "reserveA").unwrap_err().to_string();
        assert!(e.contains("no field \"reserveA\""), "{e}");
        assert!(e.contains("amount"), "should list available fields: {e}");
    }

    #[test]
    fn non_integer_field_refuses_comparison() {
        let (dec, data) = token_decoded();
        let owner = find_field(&dec, "owner").unwrap();
        assert!(read_field_int(&data, owner)
            .unwrap_err()
            .to_string()
            .contains("pubkey"));
    }

    #[test]
    fn signed_fields_sign_extend() {
        let f = crate::decode::Field {
            name: "start_ts".into(),
            offset: 0,
            ty: "i64".into(),
            size: 8,
            value: String::new(),
            editable: true,
            note: None,
        };
        assert_eq!(read_field_int(&(-42i64).to_le_bytes(), &f).unwrap(), -42);
        assert_eq!(read_field_int(&7i64.to_le_bytes(), &f).unwrap(), 7);
    }

    #[test]
    fn typoed_mutation_is_a_hard_error_not_a_passing_revert() {
        // The v0.1 hole: a mutation targeting an unloaded (typo'd) address was
        // folded into ReplayResult{success:false}, which `expect: revert`
        // scenarios happily accepted. It must be a hard Err now.
        let ctx = ReplayContext {
            signature: String::new(),
            tx: VersionedTransaction::default(),
            loaded: Vec::new(),
            slot: None,
            block_time: None,
            time_travel: TimeTravel::default(),
            idls: HashMap::new(),
            feature_toggles: Vec::new(),
        };
        // (Not the system program — that one exists as a builtin in a fresh SVM.)
        let typo = Mutation::lamports(Address::from([7u8; 32]).to_string(), 0);

        assert!(matches!(
            ctx.run_full(std::slice::from_ref(&typo)),
            Err(Error::MutationTargetMissing(_))
        ));

        // A whole suite fails fast — before any scenario executes.
        let suite = [crate::check::Scenario::new("drain")
            .mutate(typo)
            .check(crate::check::Check::revert())];
        assert!(matches!(
            run_suite(&ctx, None, &suite),
            Err(Error::MutationTargetMissing(_))
        ));
    }

    #[test]
    fn patch_bounds_are_validated_up_front() {
        let addr = Address::from_str("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA").unwrap();
        let ctx = ReplayContext {
            signature: String::new(),
            tx: VersionedTransaction::default(),
            loaded: vec![(
                addr,
                Loaded::Data(Account {
                    lamports: 1,
                    data: vec![0u8; 8],
                    owner: Address::default(),
                    executable: false,
                    rent_epoch: 0,
                }),
            )],
            slot: None,
            block_time: None,
            time_travel: TimeTravel::default(),
            idls: HashMap::new(),
            feature_toggles: Vec::new(),
        };
        // In range: fine. Out of range (offset 4 + 8 bytes > size 8): typed error.
        assert!(ctx
            .validate_mutations(&[Mutation::patch(addr.to_string(), 0, vec![0; 8])])
            .is_ok());
        assert!(matches!(
            ctx.validate_mutations(&[Mutation::patch(addr.to_string(), 4, vec![0; 8])]),
            Err(Error::PatchOutOfRange {
                offset: 4,
                len: 8,
                size: 8,
                ..
            })
        ));
        // usize overflow in offset + len must not panic either.
        assert!(ctx
            .validate_mutations(&[Mutation::patch(addr.to_string(), usize::MAX, vec![0; 8])])
            .is_err());
    }

    #[test]
    fn fixture_roundtrip_preserves_slot_and_clock() {
        // The core fixture-fidelity invariant: freezing a context and reloading it
        // must reproduce the exact slot AND wall-clock the live replay ran at — not
        // drop the block time (which used to happen, so time-gated programs behaved
        // differently after freezing).
        let ctx = ReplayContext {
            signature: "SIG".into(),
            tx: VersionedTransaction::default(),
            loaded: Vec::new(),
            slot: Some(295_000_123),
            block_time: Some(1_787_600_325),
            time_travel: TimeTravel::default(),
            idls: HashMap::new(),
            feature_toggles: Vec::new(),
        };

        let fx = ctx.to_fixture().unwrap();
        assert_eq!(fx.captured_slot, Some(295_000_123));
        assert_eq!(fx.captured_block_time, Some(1_787_600_325));

        // Survives a JSON round-trip (the on-disk format)…
        let json = fx.to_json().unwrap();
        let fx2 = Fixture::from_json(&json).unwrap();
        // …and reloads into a context with the same clock.
        let restored = ReplayContext::from_fixture(&fx2).unwrap();
        assert_eq!(restored.slot, Some(295_000_123));
        assert_eq!(restored.block_time, Some(1_787_600_325));
    }

    #[test]
    fn chrono_like_formats_utc() {
        assert_eq!(chrono_like(0), "1970-01-01 00:00 UTC");
        // 2020-03-16 14:29:00 UTC — Solana mainnet genesis.
        assert_eq!(chrono_like(1_584_368_940), "2020-03-16 14:29 UTC");
        // Pre-epoch timestamps wrap to the previous civil day, not garbage.
        assert_eq!(chrono_like(-1), "1969-12-31 23:59 UTC");
    }

    fn clock() -> Clock {
        Clock {
            slot: 1_000 * SLOTS_PER_EPOCH as u64,
            epoch: 1_000,
            epoch_start_timestamp: 2_000_000_000,
            unix_timestamp: 2_000_000_000,
            leader_schedule_epoch: 1_001,
        }
    }

    #[test]
    fn noop_time_travel_changes_nothing() {
        let tt = TimeTravel::default();
        assert!(tt.is_noop());
        let mut c = clock();
        tt.apply(&mut c);
        assert_eq!(
            (c.slot, c.epoch, c.unix_timestamp),
            (clock().slot, 1_000, 2_000_000_000)
        );
    }

    #[test]
    fn epoch_jump_advances_slot_epoch_and_time_together() {
        let tt = TimeTravel {
            epochs: Some(2),
            ..Default::default()
        };
        let mut c = clock();
        tt.apply(&mut c);
        assert_eq!(c.epoch, 1_002);
        assert_eq!(c.slot, clock().slot + 2 * SLOTS_PER_EPOCH as u64);
        let expected_secs = (2.0 * SLOTS_PER_EPOCH as f64 * SECS_PER_SLOT) as i64;
        assert_eq!(c.unix_timestamp, 2_000_000_000 + expected_secs);
        assert_eq!(c.leader_schedule_epoch, c.epoch + 1);
    }

    #[test]
    fn seconds_jump_moves_slots_in_step() {
        let tt = TimeTravel {
            seconds: Some(40),
            ..Default::default()
        };
        let mut c = clock();
        tt.apply(&mut c);
        assert_eq!(c.unix_timestamp, 2_000_000_040);
        assert_eq!(c.slot, clock().slot + 100); // 40s / 0.4s per slot
    }

    #[test]
    fn extreme_warp_saturates_instead_of_overflowing() {
        // A single absurd jump, and the applied clock, must clamp rather than
        // panic (debug) or wrap (release).
        let tt = TimeTravel {
            epochs: Some(i64::MAX),
            seconds: Some(i64::MAX),
            slots: Some(i64::MAX),
            ..Default::default()
        };
        let mut c = clock();
        tt.apply(&mut c); // must not panic
        assert!(c.unix_timestamp >= clock().unix_timestamp);
    }

    #[test]
    fn absolute_settings_override_relative_jumps() {
        let tt = TimeTravel {
            seconds: Some(1_000_000),
            at_unix_timestamp: Some(3_000_000_000),
            at_epoch: Some(7),
            ..Default::default()
        };
        let mut c = clock();
        tt.apply(&mut c);
        assert_eq!(c.unix_timestamp, 3_000_000_000);
        assert_eq!(c.epoch, 7);
        // The epoch's start can never be after "now".
        assert!(c.epoch_start_timestamp <= c.unix_timestamp);
    }

    #[test]
    fn read_u64_at_handles_bounds() {
        let mut data = vec![0u8; 72];
        data[64..72].copy_from_slice(&42u64.to_le_bytes());
        assert_eq!(read_u64_at(&data, 64), 42);
        assert_eq!(read_u64_at(&data, 70), 0); // out of range → 0, not a panic
    }

    #[test]
    fn reconstructs_closed_token_account_from_meta() {
        let tx = serde_json::json!({
            "meta": {
                "preBalances": [2_039_280],
                "preTokenBalances": [{
                    "accountIndex": 0,
                    "mint": "So11111111111111111111111111111111111111112",
                    "owner": "Vote111111111111111111111111111111111111111",
                    "uiTokenAmount": { "amount": "12345", "decimals": 9 }
                }]
            }
        });
        let keys = vec!["4Nd1mBQtrMJVYVfKf2PJy9NZUZdTAsp7D4xWLs4gDB4T".to_string()];
        let pre = PreState::from_meta(&tx, &keys);
        let acc = pre
            .reconstruct(&keys[0])
            .expect("should rebuild the SPL account");
        assert_eq!(acc.data.len(), 165);
        assert_eq!(read_u64_at(&acc.data, 64), 12345);
        assert_eq!(acc.data[108], 1); // AccountState::Initialized
        assert_eq!(acc.owner.to_string(), SPL_TOKEN_PROGRAM);
    }

    #[test]
    fn never_resurrects_an_account_the_tx_creates() {
        let pre = PreState::default();
        assert!(pre.reconstruct("SomeAddr").is_none());
    }

    #[test]
    fn captures_pre_transaction_lamports_per_account() {
        // `preBalances` is parallel to the resolved account list; `from_meta`
        // must key each pre-tx balance by its address so the reconstruction loop
        // in `build_context` can rewind a still-existing account's lamports.
        let tx = serde_json::json!({
            "meta": { "preBalances": [5_000_000_000u64, 2_039_280u64] }
        });
        let keys = vec![
            "4Nd1mBQtrMJVYVfKf2PJy9NZUZdTAsp7D4xWLs4gDB4T".to_string(),
            "So11111111111111111111111111111111111111112".to_string(),
        ];
        let pre = PreState::from_meta(&tx, &keys);
        assert_eq!(pre.lamports.get(&keys[0]).copied(), Some(5_000_000_000));
        assert_eq!(pre.lamports.get(&keys[1]).copied(), Some(2_039_280));
        assert!(!pre.is_empty());
    }
}

#[cfg(all(test, feature = "single-run-trace"))]
mod single_run_parity_tests {
    use crate::{Fixture, Replay};

    fn check(fixture: &str) {
        let replay = Replay::from_fixture(&Fixture::from_json(fixture).unwrap()).unwrap();
        let plain = replay.run().unwrap();
        let traced = replay.trace(&[]).unwrap();
        assert_eq!(
            traced.result.success, plain.result.success,
            "verdict changed by tracing"
        );
        assert_eq!(
            traced.result.error, plain.result.error,
            "error changed by tracing"
        );
        assert_eq!(
            traced.result.compute_units, plain.result.compute_units,
            "compute changed by tracing"
        );
        // The trace's verdict must also be the transaction's: every step
        // succeeded iff the transaction did, and a failure names a step.
        assert_eq!(traced.failed_step.is_some(), !plain.result.success);
        if let Some(rec) = replay.recorded() {
            assert_eq!(
                traced.result.success, rec.success,
                "verdict differs from the recorded outcome"
            );
        }
    }

    #[test]
    fn traced_success_matches_plain_run() {
        check(include_str!("../tests/fixtures/counter_increment.json"));
    }

    #[test]
    fn traced_revert_matches_plain_run() {
        check(include_str!(
            "../tests/fixtures/vesting_precliff_revert.json"
        ));
    }
}
