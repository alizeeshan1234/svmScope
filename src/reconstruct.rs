//! Historical state reconstruction — rebuild an account's state at a past slot
//! by replaying its own write history forward, instead of buying it from a paid
//! archive. This is the free, self-owned path to "replay at any slot".
//!
//! The raw ledger (which transactions touched an account, and their contents)
//! comes from a [`LedgerSource`]: a standard Solana RPC for recent history, or
//! **Old Faithful** — the free, open, Solana-Foundation-funded archive of the
//! whole chain — for deep history. Both expose the same `getSignaturesForAddress`
//! / `getTransaction` methods, so the engine is source-agnostic; only the reach
//! differs. That is what lets this be built and tested against an ordinary RPC
//! today, and pointed at Old Faithful for full history later.

use crate::error::{Error, Result};
use crate::records::{StateStore, Version};
use crate::replay::Mutation;
use crate::scope::{AccountState, Scope};
use serde_json::{json, Value};
use solana_client::rpc_client::RpcClient;
use solana_client::rpc_request::RpcRequest;
use std::collections::HashMap;

/// One transaction that touched an account, and where it landed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WriteRef {
    /// The transaction signature.
    pub signature: String,
    /// The slot it landed in.
    pub slot: u64,
    /// Whether it failed (a failed transaction changes no account data, only fees).
    pub failed: bool,
}

/// A source of raw ledger data for reconstruction. A standard RPC implements it
/// for whatever history it retains; an Old Faithful node implements it for all
/// history. The reconstruction engine is written against this trait, not against
/// any one provider.
pub trait LedgerSource {
    /// Signatures that touched `address`, newest-first, paging back from the
    /// `before` signature when given. At most `limit` returned.
    fn signatures_for_address(
        &self,
        address: &str,
        before: Option<&str>,
        limit: usize,
    ) -> Result<Vec<WriteRef>>;

    /// The full `getTransaction` JSON for a signature.
    fn transaction(&self, signature: &str) -> Result<Value>;
}

/// A [`LedgerSource`] backed by a standard Solana JSON-RPC endpoint. Its reach is
/// whatever history that endpoint keeps; swap in an Old Faithful source for the
/// full chain without changing the reconstruction engine.
pub struct RpcLedger {
    client: RpcClient,
}

impl RpcLedger {
    /// A ledger source over the given RPC endpoint.
    pub fn new(url: impl Into<String>) -> RpcLedger {
        RpcLedger {
            client: RpcClient::new(url.into()),
        }
    }
}

impl LedgerSource for RpcLedger {
    fn signatures_for_address(
        &self,
        address: &str,
        before: Option<&str>,
        limit: usize,
    ) -> Result<Vec<WriteRef>> {
        let mut opts = json!({ "limit": limit });
        if let Some(b) = before {
            opts["before"] = json!(b);
        }
        let resp: Value = self
            .client
            .send(RpcRequest::GetSignaturesForAddress, json!([address, opts]))
            .map_err(Error::rpc)?;
        let arr = resp.as_array().ok_or_else(|| {
            Error::MalformedRpcResponse("getSignaturesForAddress: not an array".into())
        })?;
        Ok(arr
            .iter()
            .filter_map(|s| {
                Some(WriteRef {
                    signature: s["signature"].as_str()?.to_string(),
                    slot: s["slot"].as_u64()?,
                    failed: !s["err"].is_null(),
                })
            })
            .collect())
    }

    fn transaction(&self, signature: &str) -> Result<Value> {
        let resp: Value = self
            .client
            .send(
                RpcRequest::GetTransaction,
                json!([
                    signature,
                    { "encoding": "json", "commitment": "confirmed", "maxSupportedTransactionVersion": 0 }
                ]),
            )
            .map_err(Error::rpc)?;
        if resp.is_null() {
            return Err(Error::TransactionNotFound(signature.to_string()));
        }
        Ok(resp)
    }
}

/// Whether `address` was a *writable* account in the transaction `tx`
/// (`getTransaction` JSON, `json` encoding). `getSignaturesForAddress` lists
/// every transaction that mentioned an account, reads included; a read cannot
/// change the account, so replaying it is wasted budget. `None` when the
/// transaction does not name the account or the message is not in the
/// expected shape (the caller treats that as "might be a write").
pub(crate) fn account_is_writable(tx: &Value, address: &str) -> Option<bool> {
    let message = &tx["transaction"]["message"];
    let keys = message["accountKeys"].as_array()?;
    if let Some(i) = keys.iter().position(|k| k.as_str() == Some(address)) {
        let header = &message["header"];
        let signed = header["numRequiredSignatures"].as_u64()? as usize;
        let ro_signed = header["numReadonlySignedAccounts"].as_u64()? as usize;
        let ro_unsigned = header["numReadonlyUnsignedAccounts"].as_u64()? as usize;
        // Static keys are laid out: [writable signers][readonly signers]
        // [writable non-signers][readonly non-signers].
        let writable = if i < signed {
            i < signed.saturating_sub(ro_signed)
        } else {
            i < keys.len().saturating_sub(ro_unsigned)
        };
        return Some(writable);
    }
    // v0: addresses loaded from lookup tables are listed by role.
    let loaded = &tx["meta"]["loadedAddresses"];
    let in_list = |role: &str| {
        loaded[role]
            .as_array()
            .is_some_and(|a| a.iter().any(|k| k.as_str() == Some(address)))
    };
    if in_list("writable") {
        return Some(true);
    }
    if in_list("readonly") {
        return Some(false);
    }
    None
}

/// Whether the transaction `signature` could have written `address`: its
/// message marks the account writable, or the message could not be inspected
/// (unknown is treated as a possible write, never silently dropped).
fn is_write(src: &dyn LedgerSource, signature: &str, address: &str) -> bool {
    match src.transaction(signature) {
        Ok(tx) => account_is_writable(&tx, address).unwrap_or(true),
        Err(_) => true,
    }
}

/// A [`LedgerSource`] that counts every call and refuses past a cap. The exact
/// engine wraps its source in one so the *whole* dependency cone — signature
/// pages, writability probes, everything, not only replays — is bounded. Past
/// the cap every call fails with [`Error::ReconstructBudget`], which callers
/// turn into "keep current state, mark inexact".
struct BudgetedLedger<'a> {
    inner: &'a dyn LedgerSource,
    calls: std::cell::Cell<usize>,
    cap: usize,
}

impl<'a> BudgetedLedger<'a> {
    fn charge(&self) -> Result<()> {
        let n = self.calls.get() + 1;
        self.calls.set(n);
        if n > self.cap {
            return Err(Error::ReconstructBudget(self.cap));
        }
        Ok(())
    }
}

impl LedgerSource for BudgetedLedger<'_> {
    fn signatures_for_address(
        &self,
        address: &str,
        before: Option<&str>,
        limit: usize,
    ) -> Result<Vec<WriteRef>> {
        self.charge()?;
        self.inner.signatures_for_address(address, before, limit)
    }
    fn transaction(&self, signature: &str) -> Result<Value> {
        self.charge()?;
        self.inner.transaction(signature)
    }
}

/// The successful transactions that touched `address` strictly before `slot`,
/// **oldest-first** — the ordered write history to replay when reconstructing the
/// account's state going into `slot`. Pages back through the ledger source until
/// it gathers `max` writes, exhausts history, or hits `max_pages` (a guard so a
/// very active account can't page unboundedly on a first pass).
pub fn write_history(
    src: &dyn LedgerSource,
    address: &str,
    before_slot: u64,
    max: usize,
    max_pages: usize,
) -> Result<Vec<WriteRef>> {
    Ok(history_walk(src, address, before_slot, Cut::Slot, max, max_pages)?.mentions)
}

/// The outcome of walking an account's signature history back to `before_slot`.
struct HistoryWalk {
    /// Successful mentions before the slot, oldest-first, at most `max`.
    mentions: Vec<WriteRef>,
    /// Whether the walk actually got back to the slot. `false` means the page
    /// cap ran out while every page was still after the slot — a hot account —
    /// and an empty `mentions` must NOT be read as "the account did not exist".
    reached_slot: bool,
    /// Whether the walk ran off the end of the account's history: every
    /// mention that ever existed was seen. Only then does "no write found"
    /// mean the account did not exist at the slot.
    reached_end: bool,
}

/// Where "before" ends inside the target slot. A block holds many
/// transactions in order; the state going *into* transaction T includes every
/// write earlier in T's own slot. `Slot` cuts at the slot boundary (state at
/// the start of the slot); `Transaction(sig)` cuts just before that
/// transaction, keeping same-slot writes that precede it.
#[derive(Clone, Copy)]
pub enum Cut<'a> {
    /// Everything strictly before the slot.
    Slot,
    /// Everything before this transaction, including earlier same-slot writes.
    Transaction(&'a str),
}

fn history_walk(
    src: &dyn LedgerSource,
    address: &str,
    before_slot: u64,
    cut: Cut<'_>,
    max: usize,
    max_pages: usize,
) -> Result<HistoryWalk> {
    history_walk_after(src, address, before_slot, cut, None, max, max_pages)
}

/// [`history_walk`] with a lower bound: mentions at or before `after_slot`
/// are not collected and end the walk (a recorded version at that slot is
/// the known state the rebuild starts from, so nothing older is needed).
fn history_walk_after(
    src: &dyn LedgerSource,
    address: &str,
    before_slot: u64,
    cut: Cut<'_>,
    after_slot: Option<u64>,
    max: usize,
    max_pages: usize,
) -> Result<HistoryWalk> {
    // Always page at the source's maximum: a hot account has thousands of
    // mentions *after* the slot that must be walked past before the first one
    // before it, and small pages would never get there.
    const PAGE: usize = 1000;
    let mut out: Vec<WriteRef> = Vec::new();
    let mut before: Option<String> = None;
    let mut reached_slot = false;
    let mut reached_end = false;
    // With a transaction cut, same-slot entries count only once the walk
    // (newest-first) has passed the target transaction itself.
    let mut passed_target = false;

    for _ in 0..max_pages {
        let batch = src.signatures_for_address(address, before.as_deref(), PAGE)?;
        let Some(last) = batch.last() else {
            reached_slot = true; // history exhausted: nothing older exists
            reached_end = true;
            break;
        };
        before = Some(last.signature.clone());
        let mut hit_floor = false;
        for w in &batch {
            if let Cut::Transaction(sig) = cut {
                if w.signature == sig {
                    passed_target = true;
                    continue; // the target itself is never part of its pre-state
                }
            }
            if after_slot.is_some_and(|floor| w.slot <= floor) {
                // Everything from here back is covered by the known state.
                hit_floor = true;
                break;
            }
            let same_slot_ok = matches!(cut, Cut::Transaction(_)) && passed_target;
            let in_range = w.slot < before_slot || (w.slot == before_slot && same_slot_ok);
            if in_range {
                reached_slot = true;
                if !w.failed {
                    out.push(w.clone());
                    if out.len() >= max {
                        break;
                    }
                }
            }
        }
        if hit_floor {
            reached_slot = true;
            reached_end = true; // the floor stands in for the start of history
            break;
        }
        if batch.len() < PAGE {
            reached_slot = true;
            reached_end = true;
            break;
        }
        if out.len() >= max {
            break;
        }
    }

    // The source returns newest-first; the replay wants oldest-first.
    out.reverse();
    Ok(HistoryWalk {
        mentions: out,
        reached_slot,
        reached_end,
    })
}

/// The result of reconstructing an account's state at a slot.
#[derive(Debug, Clone)]
pub struct Reconstructed {
    /// The account reconstructed.
    pub address: String,
    /// The slot the state is reconstructed as-of (state going *into* this slot).
    pub before_slot: u64,
    /// The reconstructed state, or `None` if the account did not exist at the slot.
    pub state: Option<AccountState>,
    /// How many writes were successfully replayed into the reconstruction.
    pub writes_replayed: usize,
    /// How many writes in range were skipped (couldn't be loaded/replayed) — a
    /// direct measure of how approximate the result is.
    pub writes_skipped: usize,
    /// How many transactions in range only *read* the account and were not
    /// replayed (they cannot change it; skipping them costs nothing).
    pub reads_ignored: usize,
}

/// Reconstruct `address`'s state going into `before_slot` by replaying its write
/// history forward with LiteSVM — the free alternative to buying the state from
/// an archive.
///
/// Each write is replayed with the account's running reconstructed state injected
/// (via a data + lamports mutation), then the account is read out again to carry
/// forward. Co-accounts are loaded at current state (best-effort), so this is
/// **exact** where an update depends on the account itself plus the instruction
/// data, and **approximate** where it depends on deeply-coupled co-account state
/// — `writes_skipped` reports how much couldn't be replayed.
///
/// For an exact result the history must reach back to the account's creation;
/// raise `max_writes` / `max_pages` for accounts with long histories.
pub fn reconstruct_account(
    scope: &Scope,
    ledger: &dyn LedgerSource,
    address: &str,
    before_slot: u64,
    cut: Cut<'_>,
    max_writes: usize,
    max_pages: usize,
) -> Result<Reconstructed> {
    reconstruct_account_from(
        scope,
        ledger,
        None,
        address,
        before_slot,
        cut,
        None,
        max_writes,
        max_pages,
    )
}

/// [`reconstruct_account`] starting from a known earlier version instead of
/// from the account's creation, and with recorded co-account state injected
/// into every replayed write when a [`StateStore`] is available.
///
/// `start` is the newest recorded version at or before the target; only the
/// writes after it are replayed. Without one the whole history is walked.
#[allow(clippy::too_many_arguments)]
pub fn reconstruct_account_from(
    scope: &Scope,
    ledger: &dyn LedgerSource,
    store: Option<&dyn StateStore>,
    address: &str,
    before_slot: u64,
    cut: Cut<'_>,
    start: Option<Version>,
    max_writes: usize,
    max_pages: usize,
) -> Result<Reconstructed> {
    // The whole pre-cut history is needed unless a recorded version supplies
    // a floor: a rebuild starts at the account's creation, so the walk must
    // reach the end of history, not just collect a few recent mentions
    // (which, for an account read by every transaction of a busy program,
    // are all reads).
    let floor = start.as_ref().map(|v| v.slot);
    let walk = history_walk_after(
        ledger,
        address,
        before_slot,
        cut,
        floor,
        usize::MAX,
        max_pages,
    )?;
    if !walk.reached_slot {
        // A hot account: more mentions since the slot than the page cap can
        // walk past. Nothing can be rebuilt from here; say so instead of
        // replaying an arbitrary suffix of the history.
        return Err(Error::ReconstructBudget(max_pages));
    }
    let history = walk.mentions;
    let exhausted = walk.reached_end;
    if !exhausted {
        // The walk hit the page cap before the account's creation: any rebuild
        // would start from an arbitrary midpoint of its history.
        return Err(Error::ReconstructBudget(max_pages));
    }
    if history.len() > PROBES_PER_ACCOUNT {
        // Telling reads from writes costs one transaction fetch per mention;
        // an account mentioned this often before the slot is a hot reader's
        // dependency (a config, a multisig) whose rebuild is not worth it here.
        return Err(Error::ReconstructBudget(PROBES_PER_ACCOUNT));
    }

    let mut state: Option<AccountState> = start.and_then(|v| v.state);
    let mut replayed = 0usize;
    let mut skipped = 0usize;
    let mut reads = 0usize;

    for w in &history {
        // A transaction that only read the account leaves it untouched: don't
        // spend a replay on it. The fetched transaction also names the
        // co-accounts whose recorded state can be injected below.
        let tx = ledger.transaction(&w.signature).ok();
        let writable = tx
            .as_ref()
            .and_then(|t| account_is_writable(t, address))
            .unwrap_or(true);
        if !writable {
            reads += 1;
            continue;
        }
        if replayed + skipped >= max_writes {
            // More writes than the replay budget allows: the result would be a
            // prefix of the history, not the state at the slot.
            return Err(Error::ReconstructBudget(max_writes));
        }
        let replay = match scope.replay(&w.signature) {
            Ok(r) => r,
            Err(_) => {
                skipped += 1;
                continue;
            }
        };
        // Inject the account's running reconstructed state before replaying this
        // write; the very first write starts from whatever the account was
        // (empty at creation).
        let mut muts: Vec<Mutation> = match &state {
            Some(s) => vec![
                Mutation::data(address.to_string(), s.data.clone()),
                Mutation::lamports(address.to_string(), s.lamports),
            ],
            None => Vec::new(),
        };
        // Co-accounts of this write at *its* slot, where a recording has them:
        // the multisig whose counter the write checked, the mint it read. A
        // version observed at or before the write's slot is the state the
        // write saw (the last write to that co-account before this slot).
        if let (Some(store), Some(tx)) = (store, tx.as_ref()) {
            for b in crate::utils::resolve_account_keys(tx) {
                if b == address || is_infra(&b) {
                    continue;
                }
                if let Ok(Some(Version { state: Some(s), .. })) =
                    store.latest_at_or_before(&b, w.slot.saturating_sub(1))
                {
                    muts.push(Mutation::data(b.clone(), s.data));
                    muts.push(Mutation::lamports(b, s.lamports));
                }
            }
        }
        match replay.account_after_success(&muts, address) {
            Ok(after) => {
                state = after; // Some = new state; None = closed by this write
                replayed += 1;
            }
            // A failed run (wrong co-account state, unloadable program…) says
            // nothing about the account; the write is skipped and the result
            // is reported as approximate.
            Err(_) => skipped += 1,
        }
    }

    if replayed == 0 && skipped == 0 && !exhausted {
        // Every inspected mention was a read and the walk stopped at its cap
        // before the start of history: the creating write is older than what
        // was looked at. "Not found here" is not "did not exist"; keep current
        // state and say the history was not fully walked.
        return Err(Error::ReconstructBudget(max_writes));
    }

    Ok(Reconstructed {
        address: address.to_string(),
        before_slot,
        state,
        writes_replayed: replayed,
        writes_skipped: skipped,
        reads_ignored: reads,
    })
}

// --- exact recursive reconstruction ----------------------------------------

/// A reconstructed account plus whether it was reconstructed *exactly*.
#[derive(Debug, Clone)]
pub struct Recon {
    /// The reconstructed state, or `None` if the account did not exist at the slot.
    pub state: Option<AccountState>,
    /// `true` only if every input in the dependency cone was itself reconstructed
    /// exactly within budget — no co-account fell back to current state.
    pub exact: bool,
}

/// Well-known infrastructure accounts (programs, sysvars) that are static or
/// runtime-provided: never the coupled economic state we reconstruct, and
/// recursing into them only wastes budget. `scope.replay` loads them correctly
/// as-is (current program ELF / runtime sysvars).
pub(crate) fn is_infra(address: &str) -> bool {
    address.starts_with("Sysvar")
        || matches!(
            address,
            "11111111111111111111111111111111"                 // System
                | "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA" // SPL Token
                | "TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb" // Token-2022
                | "ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL" // Associated Token
                | "ComputeBudget111111111111111111111111111111"
                | "NativeLoader1111111111111111111111111111111"
                | "BPFLoader2111111111111111111111111111111111"
                | "BPFLoaderUpgradeab1e11111111111111111111111"
        )
}

/// How many pre-slot mentions `reconstruct_account` is willing to probe for
/// writability (one transaction fetch each) before declaring the account too
/// hot to rebuild from history.
const PROBES_PER_ACCOUNT: usize = 120;

/// How many recent mentions `last_write` inspects (one transaction fetch each)
/// before giving up on finding a write among them. Bounds the RPC cost of a
/// heavily-read account.
const LAST_WRITE_CANDIDATES: usize = 8;

/// The single most-recent successful *write* to `address` strictly before
/// `slot`. Recent mentions are inspected newest-first and reads are passed
/// over, so a heavily-read account's last write is found even when hundreds
/// of reads came after it.
fn last_write(
    src: &dyn LedgerSource,
    address: &str,
    slot: u64,
    max_pages: usize,
) -> Result<LastWrite> {
    let walk = history_walk(
        src,
        address,
        slot,
        Cut::Slot,
        LAST_WRITE_CANDIDATES,
        max_pages,
    )?;
    if !walk.reached_slot {
        return Ok(LastWrite::Unknown);
    }
    let exhausted = walk.reached_end;
    Ok(walk
        .mentions
        .into_iter()
        .rev()
        .find(|w| is_write(src, &w.signature, address))
        .map_or(
            if exhausted {
                LastWrite::None
            } else {
                LastWrite::Unknown
            },
            LastWrite::At,
        ))
}

/// What the backward search for an account's last write found.
enum LastWrite {
    /// A write, the most recent one before the slot.
    At(WriteRef),
    /// No write before the slot within the inspected mentions: the account did
    /// not exist yet, or every recent mention was a read.
    None,
    /// The walk ran out of pages before getting back to the slot (a hot
    /// account): nothing can be concluded and current state must be kept.
    Unknown,
}

/// Recursive, memoized dependency-cone reconstruction — the exact engine. To get
/// an account's state at a slot it finds the last write, recursively reconstructs
/// **that write's inputs at that write's slot**, replays the write against those
/// exact inputs, and reads the account back out. Because every recursion targets
/// a strictly-earlier slot the dependency graph is a DAG, so it terminates;
/// memoization shares sub-results; a replay budget bounds the cone.
pub struct Reconstructor<'a> {
    scope: &'a Scope,
    ledger: BudgetedLedger<'a>,
    memo: HashMap<(String, u64), Recon>,
    budget: usize,
    max_pages: usize,
    replays: usize,
}

/// RPC calls allowed per unit of replay budget. A replay needs a few pages
/// and a handful of probes around it; a cone that spends far more than this
/// per replay is walking hot accounts it will never rebuild.
const CALLS_PER_REPLAY: usize = 25;

impl<'a> Reconstructor<'a> {
    /// A reconstructor over `scope` (for replay) and `ledger` (for history),
    /// allowed at most `budget` transaction replays before falling back to
    /// current state for the remaining cone (keeping it tractable, and honest).
    pub fn new(scope: &'a Scope, ledger: &'a dyn LedgerSource, budget: usize) -> Self {
        Reconstructor {
            scope,
            ledger: BudgetedLedger {
                inner: ledger,
                calls: std::cell::Cell::new(0),
                cap: budget
                    .saturating_mul(CALLS_PER_REPLAY)
                    .max(CALLS_PER_REPLAY),
            },
            memo: HashMap::new(),
            budget,
            max_pages: 20,
            replays: 0,
        }
    }

    /// How many transaction replays the reconstruction actually performed.
    pub fn replays(&self) -> usize {
        self.replays
    }

    /// RPC calls the engine has made so far (pages, probes, transactions).
    pub fn calls(&self) -> usize {
        self.ledger.calls.get()
    }

    /// Reconstruct `address`'s exact state going into `before_slot`.
    pub fn reconstruct(&mut self, address: &str, before_slot: u64) -> Result<Recon> {
        let key = (address.to_string(), before_slot);
        if let Some(r) = self.memo.get(&key) {
            return Ok(r.clone());
        }

        let recon = self.compute(address, before_slot)?;
        self.memo.insert(key, recon.clone());
        Ok(recon)
    }

    /// Whether any transaction mentioned `address` at or after `slot` (one
    /// signature page). `true` on an empty or failed page, so an unknown
    /// history is treated as possibly changed rather than silently trusted.
    fn mentioned_since(&self, address: &str, slot: u64) -> Result<bool> {
        let page = self.ledger.signatures_for_address(address, None, 1)?;
        Ok(page.first().is_none_or(|w| w.slot >= slot))
    }

    fn compute(&mut self, address: &str, before_slot: u64) -> Result<Recon> {
        match self.compute_inner(address, before_slot) {
            Err(Error::ReconstructBudget(_)) => {
                // The cone spent its RPC allowance: keep current state, say so.
                let state = self.scope.account_data(address)?;
                Ok(Recon {
                    state,
                    exact: false,
                })
            }
            other => other,
        }
    }

    fn compute_inner(&mut self, address: &str, before_slot: u64) -> Result<Recon> {
        // Out of budget: fall back to current state, honestly marked inexact —
        // and do it *before* spending any RPC calls on this account's history.
        // Finding a last write costs up to `LAST_WRITE_CANDIDATES` transaction
        // fetches; an exhausted cone would otherwise still pay that for every
        // remaining co-account.
        if self.replays >= self.budget {
            let state = self.scope.account_data(address)?;
            return Ok(Recon {
                state,
                exact: false,
            });
        }

        // The last write before the slot; none means the account didn't exist yet.
        let last = match last_write(&self.ledger, address, before_slot, self.max_pages)? {
            LastWrite::At(w) => w,
            LastWrite::None => {
                return Ok(Recon {
                    state: None,
                    exact: true,
                })
            }
            LastWrite::Unknown => {
                // Too many mentions since the slot to walk back with plain
                // paging: keep current state and say so.
                let state = self.scope.account_data(address)?;
                return Ok(Recon {
                    state,
                    exact: false,
                });
            }
        };

        // Reconstruct every input of the last write at that write's slot, then
        // replay the write against those exact inputs.
        let tx = self.ledger.transaction(&last.signature)?;
        let keys = crate::utils::resolve_account_keys(&tx);
        let mut muts: Vec<Mutation> = Vec::new();
        let mut cone_exact = true;

        for b in &keys {
            if is_infra(b) {
                continue; // static program / runtime sysvar — loaded correctly as-is
            }
            if b == address {
                // The account itself: its state going into this write is what the
                // recursion below computes; never short-circuit it.
            } else if !self.mentioned_since(b, last.slot)? {
                // Untouched since this write landed: current bytes are the bytes
                // it saw. One signature page instead of a recursive rebuild.
                continue;
            }
            let rec_b = self.reconstruct(b, last.slot)?;
            cone_exact &= rec_b.exact;
            if let Some(s) = rec_b.state {
                muts.push(Mutation::data(b.clone(), s.data));
                muts.push(Mutation::lamports(b.clone(), s.lamports));
            }
        }

        self.replays += 1;
        let replay = match self.scope.replay(&last.signature) {
            Ok(r) => r,
            Err(_) => {
                let state = self.scope.account_data(address)?;
                return Ok(Recon {
                    state,
                    exact: false,
                });
            }
        };
        let state = replay.account_after(&muts, address)?;
        Ok(Recon {
            state,
            exact: cone_exact,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A canned ledger for testing the walker without a network — `sigs` are
    /// newest-first, as a real `getSignaturesForAddress` returns them.
    struct MockLedger {
        sigs: Vec<WriteRef>,
    }

    impl LedgerSource for MockLedger {
        fn signatures_for_address(
            &self,
            _address: &str,
            before: Option<&str>,
            limit: usize,
        ) -> Result<Vec<WriteRef>> {
            let start = match before {
                None => 0,
                Some(b) => self
                    .sigs
                    .iter()
                    .position(|w| w.signature == b)
                    .map(|i| i + 1)
                    .unwrap_or(self.sigs.len()),
            };
            Ok(self.sigs[start..].iter().take(limit).cloned().collect())
        }
        fn transaction(&self, _signature: &str) -> Result<Value> {
            Ok(json!({}))
        }
    }

    fn w(sig: &str, slot: u64, failed: bool) -> WriteRef {
        WriteRef {
            signature: sig.to_string(),
            slot,
            failed,
        }
    }

    #[test]
    fn write_history_keeps_pre_slot_successes_oldest_first() {
        // newest-first, spanning the target slot 100, with a failed tx mixed in.
        let ledger = MockLedger {
            sigs: vec![
                w("s120", 120, false), // after the slot → excluded
                w("s090", 90, false),
                w("s080", 80, true), // failed → excluded
                w("s070", 70, false),
                w("s060", 60, false),
            ],
        };
        let hist = write_history(&ledger, "Acc", 100, 100, 10).unwrap();
        let sigs: Vec<&str> = hist.iter().map(|w| w.signature.as_str()).collect();
        // oldest-first, only successful writes strictly before slot 100.
        assert_eq!(sigs, vec!["s060", "s070", "s090"]);
    }

    #[test]
    fn infra_accounts_are_recognised_and_skipped() {
        assert!(is_infra("11111111111111111111111111111111")); // System
        assert!(is_infra("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA")); // SPL Token
        assert!(is_infra("SysvarC1ock11111111111111111111111111111111")); // a sysvar
        assert!(is_infra("ComputeBudget111111111111111111111111111111"));
        // A normal PDA / data account is NOT infra — it gets reconstructed.
        assert!(!is_infra("Cd2zEXTrYoV4UcDxZJumwZsz4A1bSZfRuZpEu5RJDDVk"));
    }

    #[test]
    fn last_write_returns_the_most_recent_before_the_slot() {
        let ledger = MockLedger {
            sigs: vec![
                w("s150", 150, false),
                w("s099", 99, false), // most recent before 100
                w("s050", 50, false),
            ],
        };
        let LastWrite::At(lw) = last_write(&ledger, "Acc", 100, 10).unwrap() else {
            panic!("expected a write before slot 100");
        };
        assert_eq!(lw.signature, "s099");
        assert!(matches!(
            last_write(&ledger, "Acc", 40, 10).unwrap(),
            LastWrite::None
        ));
    }

    fn tx_with_keys(keys: &[&str], signed: u64, ro_signed: u64, ro_unsigned: u64) -> Value {
        json!({
            "transaction": { "message": {
                "accountKeys": keys,
                "header": {
                    "numRequiredSignatures": signed,
                    "numReadonlySignedAccounts": ro_signed,
                    "numReadonlyUnsignedAccounts": ro_unsigned
                }
            }},
            "meta": {}
        })
    }

    #[test]
    fn writability_follows_the_message_header_layout() {
        // [w-signer][ro-signer][w][w][ro][ro]
        let tx = tx_with_keys(&["S1", "S2", "A", "B", "R1", "R2"], 2, 1, 2);
        assert_eq!(account_is_writable(&tx, "S1"), Some(true));
        assert_eq!(account_is_writable(&tx, "S2"), Some(false));
        assert_eq!(account_is_writable(&tx, "A"), Some(true));
        assert_eq!(account_is_writable(&tx, "B"), Some(true));
        assert_eq!(account_is_writable(&tx, "R1"), Some(false));
        assert_eq!(account_is_writable(&tx, "R2"), Some(false));
        // Not in the transaction at all.
        assert_eq!(account_is_writable(&tx, "Zzz"), None);
    }

    #[test]
    fn writability_of_lookup_table_addresses_comes_from_loaded_addresses() {
        let mut tx = tx_with_keys(&["S1", "P"], 1, 0, 1);
        tx["meta"]["loadedAddresses"] = json!({ "writable": ["W"], "readonly": ["R"] });
        assert_eq!(account_is_writable(&tx, "W"), Some(true));
        assert_eq!(account_is_writable(&tx, "R"), Some(false));
        assert_eq!(account_is_writable(&tx, "Q"), None);
    }

    #[test]
    fn malformed_transaction_is_unknown_not_a_read() {
        assert_eq!(account_is_writable(&json!({}), "A"), None);
    }

    #[test]
    fn transaction_cut_keeps_same_slot_writes_that_precede_the_target() {
        // Newest-first: s3 (slot 100, after target), TARGET (slot 100),
        // s1 (slot 100, before target in the block), s0 (slot 90).
        let ledger = MockLedger {
            sigs: vec![
                w("s3", 100, false),
                w("TARGET", 100, false),
                w("s1", 100, false),
                w("s0", 90, false),
            ],
        };
        let at_slot = history_walk(&ledger, "Acc", 100, Cut::Slot, 10, 10).unwrap();
        assert_eq!(
            at_slot
                .mentions
                .iter()
                .map(|w| w.signature.as_str())
                .collect::<Vec<_>>(),
            ["s0"]
        );
        let before_tx =
            history_walk(&ledger, "Acc", 100, Cut::Transaction("TARGET"), 10, 10).unwrap();
        assert_eq!(
            before_tx
                .mentions
                .iter()
                .map(|w| w.signature.as_str())
                .collect::<Vec<_>>(),
            ["s0", "s1"]
        );
        assert!(before_tx.reached_slot);
    }

    #[test]
    fn a_capped_walk_over_reads_does_not_mean_the_account_never_existed() {
        // 2000 mentions before the slot, all reads (the mock's transaction has
        // no keys, so writability is unknown → treated as a possible write;
        // simulate "all reads" by giving the walk a cap smaller than history).
        let ledger = MockLedger {
            sigs: (0..2000)
                .map(|i| w(&format!("s{i:04}"), 5000 - i, false))
                .collect(),
        };
        let walk = history_walk(&ledger, "Acc", 10_000, Cut::Slot, 5, 1).unwrap();
        assert!(walk.reached_slot);
        assert!(
            !walk.reached_end,
            "one page of 1000 cannot exhaust 2000 mentions"
        );
        let full = history_walk(&ledger, "Acc", 10_000, Cut::Slot, 5000, 10).unwrap();
        assert!(full.reached_end);
    }

    #[test]
    fn last_write_is_unknown_when_pages_run_out_before_the_slot() {
        // 3000 mentions, all after slot 100, and only two pages allowed: the
        // walk never reaches the slot and must not claim the account is absent.
        let ledger = MockLedger {
            sigs: (0..3000)
                .map(|i| w(&format!("s{i:04}"), 5000 - i, false))
                .collect(),
        };
        assert!(matches!(
            last_write(&ledger, "Acc", 100, 2).unwrap(),
            LastWrite::Unknown
        ));
        // With enough pages it reaches the end of history: no write before 100.
        assert!(matches!(
            last_write(&ledger, "Acc", 100, 10).unwrap(),
            LastWrite::None
        ));
    }

    #[test]
    fn write_history_respects_the_max_cap() {
        let ledger = MockLedger {
            sigs: (0..50)
                .map(|i| w(&format!("s{i:02}"), 50 - i, false))
                .collect(),
        };
        let hist = write_history(&ledger, "Acc", 1000, 5, 10).unwrap();
        assert_eq!(hist.len(), 5);
        // still oldest-first
        assert!(hist[0].slot < hist[hist.len() - 1].slot);
    }
}
