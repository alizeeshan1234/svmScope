//! Stage 3 — Deterministic replay.
//!
//! Loads the transaction's accounts AND programs into an in-process SVM
//! (LiteSVM) so the transaction can be re-executed locally.

use crate::fixture::{Fixture, FixtureEntry};
use base64::Engine;
use litesvm::types::TransactionResult;
use solana_clock::Clock;
use litesvm::LiteSVM;
use serde_json::json;
use solana_account::Account;
use solana_address::Address;
use solana_client::rpc_client::RpcClient;
use solana_client::rpc_request::RpcRequest;
use solana_transaction::versioned::VersionedTransaction;
use std::collections::HashMap;
use std::str::FromStr;

const NATIVE_LOADER: &str = "NativeLoader1111111111111111111111111111111";
const BPF_LOADER_2: &str = "BPFLoader2111111111111111111111111111111111";
const BPF_LOADER_UPGRADEABLE: &str = "BPFLoaderUpgradeab1e11111111111111111111111";

#[derive(serde::Serialize)]
pub struct ReplayResult {
    pub success: bool,
    pub error: Option<String>,
    pub logs: Vec<String>,
    pub compute_units: u64
}

fn b64_decode(s: &str) -> Vec<u8> {
    base64::engine::general_purpose::STANDARD
        .decode(s)
        .unwrap_or_default()
}

/// Fetch a single account's raw data via getAccountInfo.
fn fetch_account_data(client: &RpcClient, address: &str) -> Option<Vec<u8>> {
    let resp: serde_json::Value = client
        .send(
            RpcRequest::GetAccountInfo,
            json!([address, { "encoding": "base64" }]),
        )
        .ok()?;
    let data_b64 = resp["value"]["data"][0].as_str()?;
    Some(b64_decode(data_b64))
}

/// A ready-to-load account: either raw data, or a program's ELF bytecode.
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
fn fetch_loaded(client: &RpcClient, account_keys: &[String]) -> Result<LoadedAccounts, String> {
    let resp: serde_json::Value = client
        .send(
            RpcRequest::GetMultipleAccounts,
            json!([account_keys, { "encoding": "base64" }]),
        )
        .map_err(|e| format!("getMultipleAccounts failed: {e}"))?;

    let accounts = resp["value"]
        .as_array()
        .ok_or("unexpected getMultipleAccounts response (no value array)")?;
    let mut out: Vec<(Address, Loaded)> = Vec::new();
    let mut existing: HashMap<String, ()> = HashMap::new();

    for (i, acc) in accounts.iter().enumerate() {
        if acc.is_null() {
            continue; // account doesn't exist (created during the tx, etc.)
        }
        existing.insert(account_keys[i].clone(), ());
        let Ok(address) = Address::from_str(&account_keys[i]) else { continue };
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
                    fetch_account_data(client, &pd_addr.to_string())
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

        let Ok(owner_addr) = Address::from_str(owner) else { continue };
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

/// Build a fresh LiteSVM from already-fetched loadables (no network).
fn svm_from_loaded(loaded: &[(Address, Loaded)]) -> LiteSVM {
    // Disable sigverify + blockhash check: we're replaying an already-signed
    // transaction, so its original blockhash won't be valid in a fresh SVM.
    let mut svm = LiteSVM::new()
        .with_sigverify(false)
        .with_blockhash_check(false);
    for (addr, l) in loaded {
        match l {
            Loaded::Data(a) => {
                if let Err(e) = svm.set_account(*addr, a.clone()) {
                    eprintln!("warn: skipped account {addr}: {e:?}");
                }
            }
            Loaded::Program(elf) => {
                if let Err(e) = svm.add_program(*addr, elf) {
                    eprintln!("warn: skipped program {addr}: {e:?}");
                }
            }
        }
    }
    svm
}

/// The reconstructed world a transaction ran in, fetched once. Run any number of
/// scenarios against it with [`ReplayContext::run`] — each gets a pristine SVM.
pub struct ReplayContext {
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
}

/// An account that the transaction changed, with raw before/after bytes. The
/// caller turns these into named field diffs using the account's layout.
pub struct RawAccountDiff {
    pub address: String,
    pub owner: String,
    pub lamports_before: u64,
    pub lamports_after: u64,
    pub data_before: Vec<u8>,
    pub data_after: Vec<u8>,
}

/// Move the SVM's clock forward (or to an absolute point) so time-gated program
/// logic can be tested without waiting: unstake after an epoch, claim after a
/// vesting cliff, withdraw after a cooldown, settle after an auction ends.
///
/// Relative fields add to the transaction's own clock; absolute fields replace it.
/// Slots and epochs are kept consistent with each other where possible
/// (Solana's ~432,000 slots per epoch, ~400ms per slot).
#[derive(Default, Clone, serde::Deserialize)]
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
    pub fn is_noop(&self) -> bool {
        self.epochs.is_none() && self.slots.is_none() && self.seconds.is_none()
            && self.at_unix_timestamp.is_none() && self.at_slot.is_none() && self.at_epoch.is_none()
    }

    /// Apply this warp to a clock, advancing the related fields together so a
    /// program that reads epoch *and* timestamp sees a consistent world.
    fn apply(&self, clock: &mut Clock) {
        let mut slot_delta: i64 = 0;
        if let Some(e) = self.epochs {
            slot_delta += e * SLOTS_PER_EPOCH;
        }
        if let Some(s) = self.slots {
            slot_delta += s;
        }
        if slot_delta != 0 {
            clock.slot = clock.slot.saturating_add_signed(slot_delta);
            clock.epoch = clock.epoch.saturating_add_signed(slot_delta / SLOTS_PER_EPOCH);
            clock.unix_timestamp += (slot_delta as f64 * SECS_PER_SLOT) as i64;
        }
        if let Some(secs) = self.seconds {
            clock.unix_timestamp += secs;
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
    pub fn describe(&self, clock: &Clock) -> String {
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
    format!("{y:04}-{m:02}-{d:02} {:02}:{:02} UTC", secs / 3600, (secs % 3600) / 60)
}

impl ReplayContext {
    /// Warp the clock for subsequent runs (see [`TimeTravel`]).
    pub fn set_time_travel(&mut self, tt: TimeTravel) {
        self.time_travel = tt;
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
        clock.epoch_start_timestamp = clock.unix_timestamp
            - ((slot % SLOTS_PER_EPOCH as u64) as f64 * SECS_PER_SLOT) as i64;
        clock.leader_schedule_epoch = clock.epoch + 1;
    }

    /// A human description of the clock this context replays with, e.g.
    /// "slot 487,817,148 · epoch 1128 · 2026-09-01 04:12 UTC".
    pub fn describe_clock(&self) -> String {
        let svm = LiteSVM::new();
        let mut clock = svm.get_sysvar::<Clock>();
        self.base_clock(&mut clock);
        self.time_travel.apply(&mut clock);
        self.time_travel.describe(&clock)
    }

    /// A pristine SVM loaded with the reconstructed state, its clock advanced to
    /// the transaction's slot (and then by any requested time travel).
    fn fresh_svm(&self) -> LiteSVM {
        let mut svm = svm_from_loaded(&self.loaded);
        let mut clock = svm.get_sysvar::<Clock>();
        self.base_clock(&mut clock);
        if !self.time_travel.is_noop() {
            self.time_travel.apply(&mut clock);
        }
        svm.set_sysvar::<Clock>(&clock);
        svm
    }

    /// Replay the transaction after applying `mutations` to a fresh copy of the
    /// state. Repeatable and side-effect-free across calls.
    pub fn run(&self, mutations: &[Mutation]) -> ReplayResult {
        self.run_full(mutations).0
    }

    /// Replay and report **what changed** — every touched account's lamports and
    /// raw data, before and after. The caller decodes the bytes into named fields.
    pub fn run_with_diff(&self, mutations: &[Mutation]) -> (ReplayResult, Vec<RawAccountDiff>) {
        let (result, svm) = self.run_full(mutations);
        let mut diffs = Vec::new();
        for (addr, l) in &self.loaded {
            let Loaded::Data(before) = l else { continue };
            let Some(after) = svm.get_account(addr) else { continue };
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
        (result, diffs)
    }

    /// The pre-transaction state of an account (for delta assertions).
    fn pre_account(&self, address: &str) -> Option<&Account> {
        let addr = Address::from_str(address).ok()?;
        self.loaded.iter().find_map(|(a, l)| match l {
            Loaded::Data(acc) if *a == addr => Some(acc),
            _ => None,
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

/// Pre-transaction token-account details, enough to rebuild a closed one.
struct TokenInfo {
    mint: String,
    owner: String, // the token authority (SPL "owner" field), not the program
    amount: u64,
}

#[derive(Default)]
pub struct PreState {
    /// account address -> raw SPL token amount before the transaction.
    token_amounts: HashMap<String, u64>,
    /// account address -> its lamports before the transaction.
    lamports: HashMap<String, u64>,
    /// token account address -> details, to reconstruct it if it's since closed.
    token_info: HashMap<String, TokenInfo>,
}

impl PreState {
    pub fn from_meta(tx: &serde_json::Value, account_keys: &[String]) -> PreState {
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
                let (Some(addr), Some(amt)) = (account_keys.get(idx), amt) else { continue };
                ps.token_amounts.insert(addr.clone(), amt);
                if let (Some(mint), Some(owner)) = (e["mint"].as_str(), e["owner"].as_str()) {
                    ps.token_info.insert(
                        addr.clone(),
                        TokenInfo { mint: mint.to_string(), owner: owner.to_string(), amount: amt },
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
/// `slot` makes ALT resolution match the real world; `pre_state` restores
/// pre-transaction token balances so swaps replay faithfully instead of slipping.
pub fn build_context(
    client: &RpcClient,
    signature: &str,
    account_keys: &[String],
    slot: Option<u64>,
    pre_state: &PreState,
) -> Result<ReplayContext, String> {
    // The transaction's real block time, so date-based logic sees the actual moment.
    let block_time = slot.and_then(|s| client.get_block_time(s).ok());
    let tx = fetch_transaction(client, signature)?;
    let mut all_keys = account_keys.to_vec();
    if let Some(lookups) = tx.message.address_table_lookups() {
        for l in lookups {
            all_keys.push(l.account_key.to_string());
        }
    }
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

        // Rewind still-existing token accounts to their pre-transaction balance.
        for (addr, l) in loaded.iter_mut() {
            if let Loaded::Data(acc) = l {
                if let Some(&amt) = pre_state.token_amounts.get(&addr.to_string()) {
                    if acc.data.len() >= 72 {
                        acc.data[64..72].copy_from_slice(&amt.to_le_bytes());
                    }
                }
            }
        }
    }

    Ok(ReplayContext { tx, loaded, slot, block_time, time_travel: TimeTravel::default() })
}

fn b64_encode(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

/// Resolve an (unsigned) transaction's Address Lookup Table references to concrete
/// addresses — writable first, then readonly, the order the runtime resolves them.
/// An ALT account stores its address list at offset 56, 32 bytes each.
fn resolve_alt_addresses(client: &RpcClient, tx: &VersionedTransaction) -> (Vec<String>, Vec<String>) {
    let mut writable = Vec::new();
    let mut readonly = Vec::new();
    if let Some(lookups) = tx.message.address_table_lookups() {
        for l in lookups {
            let Some(data) = fetch_account_data(client, &l.account_key.to_string()) else { continue };
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
pub fn preflight_context(
    client: &RpcClient,
    tx: VersionedTransaction,
) -> Result<ReplayContext, String> {
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
    let (loaded, _existing) = fetch_loaded(client, &all_keys)?;
    let slot = client.get_slot().ok();
    // Real wall-clock time for that slot — a pre-flight simulation should run
    // "now", and any date-based program logic depends on this being accurate.
    let block_time = slot.and_then(|s| client.get_block_time(s).ok());
    Ok(ReplayContext { tx, loaded, slot, block_time, time_travel: TimeTravel::default() })
}

impl ReplayContext {
    /// Freeze this context into a portable, self-contained [`Fixture`] — the tx
    /// wire bytes plus every account/program, so it can replay offline forever.
    pub fn to_fixture(&self, signature: &str, captured_slot: Option<u64>) -> Fixture {
        let tx_bytes = bincode::serialize(&self.tx).expect("serialize tx");
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
        Fixture {
            signature: signature.to_string(),
            captured_slot,
            tx_b64: b64_encode(&tx_bytes),
            entries,
        }
    }

    /// Rebuild a replay context from a frozen fixture — **no network**. This is
    /// what makes fixture-backed suites deterministic and CI-safe.
    pub fn from_fixture(fx: &Fixture) -> Result<ReplayContext, String> {
        let tx_bytes = base64::engine::general_purpose::STANDARD
            .decode(&fx.tx_b64)
            .map_err(|e| format!("bad tx base64: {e}"))?;
        let tx: VersionedTransaction =
            bincode::deserialize(&tx_bytes).map_err(|e| format!("deserialize tx: {e}"))?;

        let mut loaded = Vec::with_capacity(fx.entries.len());
        for e in &fx.entries {
            match e {
                FixtureEntry::Data { address, owner, lamports, data_b64 } => {
                    let addr = Address::from_str(address).map_err(|_| format!("bad address {address}"))?;
                    let owner_addr = Address::from_str(owner).map_err(|_| format!("bad owner {owner}"))?;
                    let data = base64::engine::general_purpose::STANDARD
                        .decode(data_b64)
                        .map_err(|e| format!("bad data base64 for {address}: {e}"))?;
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
                    let addr = Address::from_str(address).map_err(|_| format!("bad address {address}"))?;
                    let elf = base64::engine::general_purpose::STANDARD
                        .decode(elf_b64)
                        .map_err(|e| format!("bad elf base64 for {address}: {e}"))?;
                    loaded.push((addr, Loaded::Program(elf)));
                }
            }
        }
        Ok(ReplayContext { tx, loaded, slot: fx.captured_slot, block_time: None, time_travel: TimeTravel::default() })
    }
}

/// Fetch the raw transaction (base64 wire bytes) and deserialize it.
fn fetch_transaction(client: &RpcClient, signature: &str) -> Result<VersionedTransaction, String> {
    let resp: serde_json::Value = client
        .send(
            RpcRequest::GetTransaction,
            json!([signature, { "encoding": "base64", "maxSupportedTransactionVersion": 0 }]),
        )
        .map_err(|e| format!("getTransaction failed: {e}"))?;
    let tx_b64 = resp["transaction"][0]
        .as_str()
        .ok_or_else(|| format!("no base64 transaction in response for {signature}"))?;
    let bytes = b64_decode(tx_b64);
    bincode::deserialize::<VersionedTransaction>(&bytes)
        .map_err(|e| format!("could not deserialize transaction {signature}: {e}"))
}

/// A change to apply to an account before replaying.
///
/// `Data` replaces an account's bytes wholesale; `DataPatch` overwrites a slice
/// at an offset (how you'd flip a token balance or an oracle price in place).
#[allow(dead_code)]
pub enum Mutation {
    Lamports { address: String, value: u64 },
    Data { address: String, bytes: Vec<u8> },
    DataPatch { address: String, offset: usize, bytes: Vec<u8> },
}

/// Apply one mutation to the SVM, or explain why it can't be applied.
fn apply_mutation(svm: &mut LiteSVM, m: &Mutation) -> Result<(), String> {
    let (address, addr) = match m {
        Mutation::Lamports { address, .. }
        | Mutation::Data { address, .. }
        | Mutation::DataPatch { address, .. } => (
            address,
            Address::from_str(address).map_err(|_| format!("invalid address: {address}"))?,
        ),
    };
    let mut account = svm
        .get_account(&addr)
        .ok_or_else(|| format!("mutation target not loaded in SVM: {address}"))?;

    match m {
        Mutation::Lamports { value, .. } => account.lamports = *value,
        Mutation::Data { bytes, .. } => account.data = bytes.clone(),
        Mutation::DataPatch { offset, bytes, .. } => {
            let end = offset + bytes.len();
            if end > account.data.len() {
                return Err(format!(
                    "patch out of range for {address}: offset {offset} + {} bytes > account size {}",
                    bytes.len(),
                    account.data.len()
                ));
            }
            account.data[*offset..end].copy_from_slice(bytes);
        }
    }
    svm.set_account(addr, account)
        .map_err(|e| format!("set_account failed for {address}: {e:?}"))
}

/// Convert a raw transaction result into a `ReplayResult`.
///
/// This does NOT print — it just extracts the data. The caller (CLI or a UI)
/// decides how to display it.
fn to_replay_result(result: TransactionResult) -> ReplayResult {
    match result {
        Ok(meta) => ReplayResult {
            success: true,
            error: None,
            logs: meta.logs,
            compute_units: meta.compute_units_consumed,
        },
        Err(failed) => ReplayResult {
            success: false,
            error: Some(format!("{:?}", failed.err)),
            logs: failed.meta.logs,
            compute_units: failed.meta.compute_units_consumed,
        },
    }
}

/// A replay that never ran because state couldn't be reconstructed (RPC error,
/// missing transaction) — reported as a failed result instead of a panic.
fn failed_result(error: String) -> ReplayResult {
    ReplayResult { success: false, error: Some(error), logs: vec![], compute_units: 0 }
}

/// Replay the transaction against the reconstructed state.
pub fn replay_transaction(
    client: &RpcClient,
    signature: &str,
    account_keys: &[String],
    slot: Option<u64>,
    pre_state: &PreState,
) -> ReplayResult {
    match build_context(client, signature, account_keys, slot, pre_state) {
        Ok(ctx) => ctx.run(&[]),
        Err(e) => failed_result(e),
    }
}

/// Replay the transaction after applying the given mutations.
pub fn mutate_and_replay(
    client: &RpcClient,
    signature: &str,
    account_keys: &[String],
    mutations: &[Mutation],
    slot: Option<u64>,
    pre_state: &PreState,
) -> ReplayResult {
    match build_context(client, signature, account_keys, slot, pre_state) {
        Ok(ctx) => ctx.run(mutations),
        Err(e) => failed_result(e),
    }
}

// ---------------------------------------------------------------------------
// Scenario testing: assert expected outcomes across a suite of what-ifs.
// ---------------------------------------------------------------------------

/// The outcome a scenario asserts.
pub enum Expect {
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
    fn matches(&self, r: &ReplayResult) -> bool {
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

    fn describe(&self) -> String {
        match self {
            Expect::Success => "succeeds".into(),
            Expect::Revert => "reverts".into(),
            Expect::RevertContains(s) => format!("reverts with \"{s}\""),
            Expect::Any => "any outcome".into(),
        }
    }
}

/// A comparison operator for numeric state assertions.
#[derive(Clone, Copy)]
pub enum CmpOp {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

impl CmpOp {
    fn test(self, a: u64, b: u64) -> bool {
        self.test_i128(a as i128, b as i128)
    }
    fn test_i128(self, a: i128, b: i128) -> bool {
        match self {
            CmpOp::Eq => a == b,
            CmpOp::Ne => a != b,
            CmpOp::Lt => a < b,
            CmpOp::Le => a <= b,
            CmpOp::Gt => a > b,
            CmpOp::Ge => a >= b,
        }
    }
    pub fn symbol(self) -> &'static str {
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
pub enum StateCheck {
    /// The account's lamports satisfy `op value`.
    Lamports { op: CmpOp, value: u64 },
    /// The little-endian u64 at `offset` satisfies `op value`
    /// (e.g. an SPL token amount at offset 64).
    U64At { offset: usize, op: CmpOp, value: u64 },
    /// The *change* in lamports (post - pre) satisfies `op value` (may be negative)
    /// — e.g. "the fee vault gained ≥ N": `LamportsDelta { Ge, N }`.
    LamportsDelta { op: CmpOp, value: i128 },
    /// The *change* in SPL token amount (u64 @ 64, post - pre) satisfies `op value`.
    TokenDelta { op: CmpOp, value: i128 },
}

/// A post-replay assertion targeting one account.
pub struct AccountAssert {
    pub address: String,
    pub check: StateCheck,
}

/// Read a little-endian u64 at `offset`, or 0 if out of range.
fn read_u64_at(data: &[u8], offset: usize) -> u64 {
    match data.get(offset..offset + 8) {
        Some(s) => u64::from_le_bytes(s.try_into().unwrap()),
        None => 0,
    }
}

impl AccountAssert {
    fn describe(&self) -> String {
        let a = short(&self.address);
        match &self.check {
            StateCheck::Lamports { op, value } => format!("{a} lamports {} {value}", op.symbol()),
            StateCheck::U64At { offset, op, value } => format!("{a} u64@{offset} {} {value}", op.symbol()),
            StateCheck::LamportsDelta { op, value } => format!("{a} lamports Δ {} {value}", op.symbol()),
            StateCheck::TokenDelta { op, value } => format!("{a} token Δ {} {value}", op.symbol()),
        }
    }

    /// Evaluate against the post-replay `svm`; `ctx` supplies pre-transaction values
    /// for the delta checks.
    fn eval(&self, svm: &LiteSVM, ctx: &ReplayContext) -> Result<bool, String> {
        let addr = Address::from_str(&self.address).map_err(|_| format!("bad address {}", self.address))?;
        let acc = svm.get_account(&addr);
        match &self.check {
            StateCheck::Lamports { op, value } => {
                Ok(op.test(acc.map(|a| a.lamports).unwrap_or(0), *value))
            }
            StateCheck::U64At { offset, op, value } => {
                let acc = acc.ok_or_else(|| format!("account not found: {}", self.address))?;
                if offset + 8 > acc.data.len() {
                    return Err(format!("u64@{offset} out of range (len {})", acc.data.len()));
                }
                Ok(op.test(read_u64_at(&acc.data, *offset), *value))
            }
            StateCheck::LamportsDelta { op, value } => {
                let pre = ctx.pre_account(&self.address).map(|a| a.lamports).unwrap_or(0) as i128;
                let post = acc.map(|a| a.lamports).unwrap_or(0) as i128;
                Ok(op.test_i128(post - pre, *value))
            }
            StateCheck::TokenDelta { op, value } => {
                let pre = ctx.pre_account(&self.address).map(|a| read_u64_at(&a.data, 64)).unwrap_or(0) as i128;
                let post = acc.map(|a| read_u64_at(&a.data, 64)).unwrap_or(0) as i128;
                Ok(op.test_i128(post - pre, *value))
            }
        }
    }
}

fn short(s: &str) -> String {
    if s.len() > 12 {
        format!("{}…{}", &s[..4], &s[s.len() - 4..])
    } else {
        s.to_string()
    }
}

/// One named test case: mutations, the transaction-level outcome it asserts, and
/// optional post-replay state assertions.
pub struct ScenarioSpec {
    pub name: String,
    pub mutations: Vec<Mutation>,
    pub expect: Expect,
    pub asserts: Vec<AccountAssert>,
}

/// The result of one post-replay assertion.
#[derive(serde::Serialize)]
pub struct AssertOutcome {
    pub description: String,
    pub pass: bool,
}

/// The result of running one scenario.
#[derive(serde::Serialize)]
pub struct ScenarioOutcome {
    pub name: String,
    /// Human description of the transaction-level expectation.
    pub expect: String,
    /// Whether everything asserted (outcome + all state checks) held.
    pub pass: bool,
    pub actual: ReplayResult,
    pub asserts: Vec<AssertOutcome>,
}

impl ReplayContext {
    /// Run mutations and keep the post-replay SVM so state can be inspected.
    fn run_full(&self, mutations: &[Mutation]) -> (ReplayResult, LiteSVM) {
        let mut svm = self.fresh_svm();
        for m in mutations {
            if let Err(e) = apply_mutation(&mut svm, m) {
                return (
                    ReplayResult { success: false, error: Some(e), logs: vec![], compute_units: 0 },
                    svm,
                );
            }
        }
        let result = to_replay_result(svm.send_transaction(self.tx.clone()));
        (result, svm)
    }
}

/// Run a suite of scenarios against one pre-fetched context and report pass/fail.
pub fn run_suite(ctx: &ReplayContext, scenarios: &[ScenarioSpec]) -> Vec<ScenarioOutcome> {
    scenarios
        .iter()
        .map(|s| {
            let (actual, svm) = ctx.run_full(&s.mutations);
            let outcome_pass = s.expect.matches(&actual);

            let asserts: Vec<AssertOutcome> = s
                .asserts
                .iter()
                .map(|a| AssertOutcome {
                    description: a.describe(),
                    // A failed eval (missing account, bad offset) counts as a failed assertion.
                    pass: a.eval(&svm, ctx).unwrap_or(false),
                })
                .collect();

            let pass = outcome_pass && asserts.iter().all(|a| a.pass);
            ScenarioOutcome {
                name: s.name.clone(),
                expect: s.expect.describe(),
                pass,
                actual,
                asserts,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!((c.slot, c.epoch, c.unix_timestamp), (clock().slot, 1_000, 2_000_000_000));
    }

    #[test]
    fn epoch_jump_advances_slot_epoch_and_time_together() {
        let tt = TimeTravel { epochs: Some(2), ..Default::default() };
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
        let tt = TimeTravel { seconds: Some(40), ..Default::default() };
        let mut c = clock();
        tt.apply(&mut c);
        assert_eq!(c.unix_timestamp, 2_000_000_040);
        assert_eq!(c.slot, clock().slot + 100); // 40s / 0.4s per slot
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
        let acc = pre.reconstruct(&keys[0]).expect("should rebuild the SPL account");
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
}