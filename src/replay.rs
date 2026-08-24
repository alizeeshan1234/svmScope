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
        .expect("bad base64")
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

/// Fetch each account's on-chain state and resolve program ELFs into loadables.
///
/// This does all the network I/O; building a fresh SVM from the result is then
/// free, which is what lets a whole scenario suite run without re-fetching.
/// Returns the loadables plus the set of addresses that actually exist on-chain
/// (non-null) — including programs whose ELF we couldn't resolve. The caller uses
/// that set to tell a genuinely-closed account (safe to reconstruct) apart from a
/// program that merely failed to load (must NOT be reconstructed as data).
fn fetch_loaded(
    client: &RpcClient,
    account_keys: &[String],
) -> (Vec<(Address, Loaded)>, HashMap<String, ()>) {
    let resp: serde_json::Value = client
        .send(
            RpcRequest::GetMultipleAccounts,
            json!([account_keys, { "encoding": "base64" }]),
        )
        .expect("getMultipleAccounts failed");

    let accounts = resp["value"].as_array().expect("value should be an array");
    let mut out: Vec<(Address, Loaded)> = Vec::new();
    let mut existing: HashMap<String, ()> = HashMap::new();

    for (i, acc) in accounts.iter().enumerate() {
        if acc.is_null() {
            continue; // account doesn't exist (created during the tx, etc.)
        }
        existing.insert(account_keys[i].clone(), ());
        let address = Address::from_str(&account_keys[i]).expect("bad account address");
        let owner = acc["owner"].as_str().unwrap();
        let executable = acc["executable"].as_bool().unwrap();

        if executable {
            // Resolve the program's ELF bytecode based on which loader owns it.
            let elf: Option<Vec<u8>> = if owner == NATIVE_LOADER {
                None // native programs are built into LiteSVM
            } else if owner == BPF_LOADER_2 {
                Some(b64_decode(acc["data"][0].as_str().unwrap()))
            } else if owner == BPF_LOADER_UPGRADEABLE {
                // Upgradeable: program account is a pointer; bytes 4..36 = the
                // programdata address, ELF starts at offset 45 inside it.
                let prog = b64_decode(acc["data"][0].as_str().unwrap());
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

        let owner_addr = Address::from_str(owner).expect("bad owner address");
        out.push((
            address,
            Loaded::Data(Account {
                lamports: acc["lamports"].as_u64().unwrap(),
                data: b64_decode(acc["data"][0].as_str().unwrap()),
                owner: owner_addr,
                executable: false,
                rent_epoch: 0,
            }),
        ));
    }
    (out, existing)
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
                svm.set_account(*addr, a.clone()).expect("set_account failed");
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
}

impl ReplayContext {
    /// A pristine SVM loaded with the reconstructed state, its clock advanced to
    /// the transaction's slot.
    fn fresh_svm(&self) -> LiteSVM {
        let mut svm = svm_from_loaded(&self.loaded);
        if let Some(slot) = self.slot {
            let mut clock = svm.get_sysvar::<Clock>();
            clock.slot = slot;
            svm.set_sysvar::<Clock>(&clock);
        }
        svm
    }

    /// Replay the transaction after applying `mutations` to a fresh copy of the
    /// state. Repeatable and side-effect-free across calls.
    pub fn run(&self, mutations: &[Mutation]) -> ReplayResult {
        self.run_full(mutations).0
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
) -> ReplayContext {
    let tx = fetch_transaction(client, signature);
    let mut all_keys = account_keys.to_vec();
    if let Some(lookups) = tx.message.address_table_lookups() {
        for l in lookups {
            all_keys.push(l.account_key.to_string());
        }
    }
    let (mut loaded, existing) = fetch_loaded(client, &all_keys);

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

    ReplayContext { tx, loaded, slot }
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
pub fn preflight_context(client: &RpcClient, tx: VersionedTransaction) -> ReplayContext {
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
    let (loaded, _existing) = fetch_loaded(client, &all_keys);
    let slot = client.get_slot().ok();
    ReplayContext { tx, loaded, slot }
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
        Ok(ReplayContext { tx, loaded, slot: fx.captured_slot })
    }
}

/// Fetch the raw transaction (base64 wire bytes) and deserialize it.
fn fetch_transaction(client: &RpcClient, signature: &str) -> VersionedTransaction {
    let resp: serde_json::Value = client
        .send(
            RpcRequest::GetTransaction,
            json!([signature, { "encoding": "base64", "maxSupportedTransactionVersion": 0 }]),
        )
        .expect("getTransaction failed");
    let tx_b64 = resp["transaction"][0]
        .as_str()
        .expect("no base64 transaction in response");
    let bytes = b64_decode(tx_b64);
    bincode::deserialize::<VersionedTransaction>(&bytes).expect("failed to deserialize transaction")
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

/// Replay the transaction against the reconstructed state.
pub fn replay_transaction(
    client: &RpcClient,
    signature: &str,
    account_keys: &[String],
    slot: Option<u64>,
    pre_state: &PreState,
) -> ReplayResult {
    build_context(client, signature, account_keys, slot, pre_state).run(&[])
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
    build_context(client, signature, account_keys, slot, pre_state).run(mutations)
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
}

/// A post-replay assertion targeting one account.
pub struct AccountAssert {
    pub address: String,
    pub check: StateCheck,
}

impl AccountAssert {
    fn describe(&self) -> String {
        match &self.check {
            StateCheck::Lamports { op, value } => {
                format!("{} lamports {} {}", short(&self.address), op.symbol(), value)
            }
            StateCheck::U64At { offset, op, value } => {
                format!("{} u64@{} {} {}", short(&self.address), offset, op.symbol(), value)
            }
        }
    }

    fn eval(&self, svm: &LiteSVM) -> Result<bool, String> {
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
                let mut b = [0u8; 8];
                b.copy_from_slice(&acc.data[*offset..offset + 8]);
                Ok(op.test(u64::from_le_bytes(b), *value))
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
                    pass: a.eval(&svm).unwrap_or(false),
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