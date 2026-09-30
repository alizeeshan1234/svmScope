# Bundle replay — everything from zero (2026-09-29)

## 0. What we are building

Give svmscope an ordered set of transactions that depend on each other — a
Jito bundle, a run of consecutive transactions in one block, or a list of
signatures across slots — and:

1. replay them in order on one SVM from the state the first one started in,
2. prove that replay matches what the chain recorded for every transaction,
3. let the user change something in any transaction (a balance, a field, an
   instruction argument) and re-run from that transaction onward,
4. show, for every later transaction, what changed versus the unmutated run
   and through which accounts the change travelled.

Why "prove" first: the audience is searchers and DeFi teams. If the baseline
does not match the chain, every ripple effect is fiction and they will say so.

## 1. Read before writing

Jito
- https://docs.jito.wtf/ — bundles (max 5 txs, atomic, one slot, fixed order),
  tip accounts, block-engine RPC (`getBundleStatuses`).
- Explorer API, verified 2026-09-29, no auth, JSON:
  - `GET https://bundles.jito.wtf/api/v1/bundles/transaction/{signature}`
    → `[{"bundle_id":"…"}]`; 404 `{"error":"Bundle not found"}` when the
    transaction was not in a bundle.
  - `GET https://bundles.jito.wtf/api/v1/bundles/bundle/{bundle_id}`
    → `[{"bundleId","slot","validator","tippers":[…],"landedTipLamports",
    "landedCu","blockIndex","timestamp","txSignatures":[…]}]`;
    `txSignatures` is the bundle order; 404 when unknown.
  - `GET https://bundles.jito.wtf/api/v1/bundles/recent?limit=N`
    → recent bundles with `transactions[]`; use it to find 2–5 tx test bundles.
- github.com/jito-labs/searcher-examples — how bundles are built and tipped.

LiteSVM (docs.rs/litesvm 0.16): `send_transaction(tx) -> TransactionResult`
commits state; `get_account(&addr)`; `set_account`. Keep the SVM, run the
next transaction: that is the entire mechanism.

Solana: `getBlock` returns transactions in recorded order; the block's
committed state equals applying them in that order. So "consecutive
transactions in a block" replay exactly on one SVM. Jito's `blockIndex` is a
position in that order.

svmscope, in this order
- `src/replay.rs`: `ReplayContext` (~585: `tx`, `loaded: Vec<(Address, Loaded)>`,
  `slot`, `block_time`, `time_travel`, `idls`, `feature_toggles`);
  `prepare(mutations, tracing) -> Prepared { svm, tx, resolved }`;
  `run_full_with_pre` (returns the post-run `LiteSVM`);
  `loaded_state_of(&svm) -> HashMap<Address, Account>` (snapshot);
  `set_loaded_data(address, Option<Account>)` (replace/add/remove a loaded
  account); `with_transaction(tx)` (same world, another tx);
  `run_with_diff(mutations) -> (ReplayResult, Vec<RawAccountDiff>)`;
  `trace_raw(mutations)` (prefix replays, each from a fresh SVM built from
  `loaded`). `ReplayResult { success, error, error_name, logs, compute_units }`.
- `src/scope.rs`: `Scope::replay_at(input, slot) -> Replay` (~910: exact tier
  with an archive, else free tier reconstruction; `PreState::from_meta`
  rewinds the tx's own pre-balances); `resolve_signature`; `transaction_json`;
  `OnchainRecord::from_tx_json`; `Replay { ctx, recorded, time_travel,
  fidelity, provenance, … }`; `Replay::run() -> Replayed`;
  `run_transaction(tx)` (~3391); `trace(mutations) -> Trace` (~3434);
  `certificate()`.
- `src/lift.rs` ~540: precedent for running a second transaction on the
  first one's world.
- `src/trace.rs`: `Trace`/`Step` — the per-transaction model the UI stacks.
- `src/records_github.rs`: the blocking `reqwest` client pattern to copy.
- `server/src/depwatch.rs` `lift_handler`: the handler pattern to copy
  (`spawn_blocking`, `check_scope(rpc)`, error mapping).

## 2. Vocabulary and data

- `BundleInput`: `BundleId(String)` | `Signature(String)` (look up its bundle)
  | `Block { slot, from_index, to_index }` | `Signatures(Vec<String>)`.
- `Mode`: `Consecutive` (one slot, exact) | `CrossSlot` (carry-forward +
  per-slot rebuild + divergence flags). Bundle and Block inputs are always
  Consecutive; a signature list is Consecutive if every tx has the same slot,
  else CrossSlot.
- `BundleMeta { id: Option<String>, slot, block_index: Option<u64>,
  signatures: Vec<String>, tippers, tip_lamports }`.
- `Step { index, signature, slot, tx_json, record: OnchainRecord,
  keys: Vec<String> /* every account the tx names, lookup tables resolved */ }`.
- `Bundle { mode, meta, steps: Vec<Step>, world: ReplayContext /* merged,
  pre-bundle state */, per_step_worlds: Vec<ReplayContext> /* only CrossSlot */ }`.
- `StepReport { index, signature, result: ReplayResult, diff: Vec<AccountDiff>,
  matches_chain: bool, chain_success: bool, chain_error: Option<String>,
  compute_chain: Option<u64>, carried: Vec<String> /* accounts taken from the
  cascade */, diverged: Vec<Diverged> /* CrossSlot only */ }`.
- `Edge { from_step, to_step, account, role: Writable|Readonly }`.
- `BundleReport { mode, meta, exact: bool, first_drift: Option<usize>,
  steps: Vec<StepReport>, edges: Vec<Edge> }`.
- `StepMutations = Vec<(usize /* step */, Vec<Mutation>)>`.

## 3. Files, in writing order

### 3.1 `src/jito.rs` (new, ~80 lines)
- `pub struct JitoClient { client: reqwest::blocking::Client }` built like
  `GithubQueue::new` (user agent `svmscope-bundles`, 30 s timeout).
- `pub fn bundle_of_signature(&self, sig: &str) -> Result<Option<String>>`:
  GET `/bundles/transaction/{sig}`; 404 → `Ok(None)`; else first element's
  `bundle_id`.
- `pub fn bundle(&self, id: &str) -> Result<Option<BundleMeta>>`:
  GET `/bundles/bundle/{id}`; 404 → `Ok(None)`; map `txSignatures`, `slot`,
  `blockIndex`, `tippers`, `landedTipLamports`.
- Errors through `crate::error::Error` (add a variant like
  `Error::Upstream(String)` if none fits; check `src/error.rs` first).
- Test (ignored, network): `bundle_of_signature` on a signature taken from
  `/bundles/recent`, then `bundle(id)` returns ≥1 signature.

### 3.2 `src/bundle.rs` (new) — resolve and build
- `impl Scope { pub fn bundle(&self, input: BundleInput) -> Result<Bundle> }`
  1. Resolve to `(Vec<String> signatures, Option<BundleMeta>)`:
     BundleId → `jito.bundle`; Signature → `bundle_of_signature` then
     `bundle` (error "not in a bundle" if None; suggest the Signatures input);
     Block → `getBlock(slot)` with `maxSupportedTransactionVersion: 1`, take
     positions `from..=to`, keep their signatures; Signatures → as given
     (dedupe, keep order).
  2. For each signature: `self.transaction_json(sig)`, `OnchainRecord::from_tx_json`,
     slot from `tx["slot"]`, keys via `utils::resolve_account_keys` (same as
     `replay_at`). Build `Step`.
  3. Mode: all slots equal → Consecutive, else CrossSlot. Consecutive with a
     bundle meta: assert `signatures` order matches `txSignatures`.
  4. Worlds: for each step call `self.replay_at(sig, step.slot)` and take its
     `ctx` (make a `pub(crate)` accessor on `Replay` if needed). Consecutive:
     merge into one `ReplayContext` = step 0's ctx, then for each later ctx and
     each `(address, loaded)` not already present, push it (first wins). Keep
     the merged ctx as `world`. CrossSlot: keep every ctx in `per_step_worlds`.
  Why first-wins is right for one slot: an account both tx1 and tx2 touch is
  in tx1's set at pre-bundle state; an account only tx2 touches was not
  written by tx1, so tx2's pre-state is the pre-bundle state.

### 3.3 `src/bundle.rs` — the sequential run
- `impl Bundle { pub fn run(&self, mutations: &StepMutations) -> Result<BundleReport> }`
  1. `ctx = self.world.with_transaction(step0.tx)`; loop over steps:
     a. `muts` = mutations for this step (empty if none).
     b. `let Prepared { mut svm, tx, .. } = ctx.prepare(&muts, false)?`
        (mutations apply to the cascaded world; instruction mutations rewrite
        this step's tx).
     c. `let pre = ctx.loaded_state_of(&svm)`; `let res = svm.send_transaction(tx)`;
        map with the existing `to_replay_result`.
     d. Diff: reuse the logic in `run_with_diff` (factor a helper that takes
        `(&ctx, &pre_svm_state, &svm)` if needed) → `Vec<AccountDiff>` via the
        existing decode path (`diffs_of` in scope.rs shows how raw diffs become
        named diffs).
     e. Snapshot for the next step: `next_loaded = ctx.loaded_state_of(&svm)`
        PLUS every address named by later steps that `svm.get_account` now
        returns but `loaded` lacks (accounts created by this step — a fresh
        ATA, a new position). Build `next_ctx = ctx.with_transaction(next.tx)`
        and `set_loaded_data` for each snapshot entry. CrossSlot: start from
        `per_step_worlds[k+1]` instead and overwrite only the *carried* set
        (accounts written by earlier steps: union of their diffs); record the
        carried addresses; for each carried address compare the carried bytes
        with the rebuilt world's bytes → if different, push `Diverged`.
     f. `matches_chain` = `res.success == record.success`, outcome only; keep
        chain compute and both error strings for the report. Do *not* compare
        the error strings: LiteSVM formats errors as text and the RPC returns
        JSON, so identical failures compare unequal (nothing else in the crate
        compares them either — see `Check::matches_onchain` and the fidelity
        sweep).
  2. `exact` = every step matches; `first_drift` = first index that does not.
  3. Edges: for step i, for each changed address in its diff, for each later
     step j whose `keys` contain it → `Edge { i, j, account, role }` (role from
     the message header: writable vs readonly index).
  4. Performance: cache the baseline snapshots on the `Bundle` (`RefCell` or
     compute once in `new`) so a mutation at step k restarts from snapshot k.
- `pub fn trace(&self, step: usize, mutations: &StepMutations) -> Result<Trace>`:
  run to step k−1 (cached), build `Replay` for step k from the cascaded ctx
  (`Replay { ctx, recorded: Some(record), fidelity: … }`), call
  `Replay::trace(&muts_k)`. This is what the debugger panels consume.

### 3.4 Exports and CLI
- `src/lib.rs`: `mod jito; mod bundle; pub use bundle::{Bundle, BundleInput, BundleReport, StepReport, Edge, Mode};`
- `src/main.rs`: `svmscope bundle <bundle-id|signature> [--json]` printing
  steps (✓/✗ vs chain, CU) and edges as `tx1 → tx3 via <account>`.

### 3.5 Server (`server/src/main.rs`)
- `GET /bundle/{id_or_sig}?cluster=` → `BundleReport` (baseline). Copy the
  `lift_handler` shape: `spawn_blocking`, `check_scope(rpc)`, `lib_err`.
- `POST /bundle` body `{ bundle: <id|sig|{signatures:[…]}|{slot,from,to}>,
  mutations: [{ step, mutations: [MutationInput…] }] }` → `BundleReport`;
  `POST /bundle/trace` same body plus `step` → `Trace`.
- `endpoint_label`: add `"/bundle"` → `"bundle"` (so usage is counted);
  `api_index`: three lines; cache layer: GET `/bundle/` is cacheable.

### 3.6 UI (after the engine is proven)
- New page `/bundle`: input box (bundle id, signature, or list), a vertical
  stack of the existing debugger panels, one per step, each with its own
  "what your changes did" table; above them the cascade table (edges) and the
  exact/drift banner; mutations attach to a step and re-run from there.

## 4. Tests

- Unit (no network): world merge is first-wins; edges from synthetic diffs
  and key lists; mode detection; Jito JSON parsing from fixture strings.
- Integration, `#[ignore = "network"]` like `tests/send_and_capture_localnet.rs`:
  pick a bundle with 2–5 transactions from `/bundles/recent`, pin its id in
  the test, build, run baseline, assert `exact == true` and every
  `matches_chain`. This is the proof. Then mutate step 0 (a balance) and
  assert step 1's outcome or diff changed and an edge exists between them.
  **Pick the bundle for sharing, not for size** (2026-09-30): most bundles
  hold one transaction, and a two-transaction bundle can easily have two
  transactions that touch nothing in common — ours produced zero edges and a
  mutation at step 0 that no later step could feel. Rank candidates by
  accounts written by one transaction and named writable by a later one. The
  pinned five-transaction bundle
  `71e899723de972d9e5b286ed1e1a6b77a8f60d6029324124c98eeca0fe100912`
  (slot 451735996) has fifteen such accounts and is the real proof.
- Compare a step inside the bundle against the same step alone using the
  worlds from **one** build (`bundle.worlds[k].prepare`), never a second
  `replay_at` over the network: the free tier reads current accounts and
  rewinds them, so a world built minutes later has genuinely moved, and the
  compute units differ in either direction for reasons the merge had no part
  in. This cost an hour before the cause was clear.
- Fixture later: extend the fixture format to hold N transactions so the
  proof runs offline in CI.

## 5. Gotchas to handle as you go
- Accounts created mid-bundle (ATAs, positions): step 3.3.1.e covers them;
  test with a bundle whose second tx uses an ATA the first creates.
- Address lookup tables: `keys` must include resolved lookup addresses for
  every step (`replay_at` already does this per tx; the merge keeps them).
- Same fee payer across steps: balance cascade is automatic on one SVM.
- Blockhash checks are already off in replay; keep them off.
- Signatures: LiteSVM does not reject a reused signature across different
  transactions; nothing to do.
- Compute-budget instructions are per transaction; nothing to do.
- Cap steps (say 16) and total instructions for traces (the debugger caps at 64/tx).
- CrossSlot divergence compares bytes and lamports; report which.

## 5b. Measured, 2026-09-30 (3.1 through 3.6 done)

Everything in section 3 is written: `src/jito.rs`, the builder, the
sequential run, `Bundle::trace`, the baseline cache, `svmscope bundle`, the
three server routes and the bundle page.

Two things changed from the plan as written. The CLI takes one positional
argument and reads the input variant from its shape: a comma means a list,
64 hex characters means a bundle id, anything else is a signature. And the
page lives at `/bundles`, not `/bundle`, because the API owns
`/bundle/{target}` the way `/trace/{signature}` is owned while its page is
`/debug`.

Replaying one slot back to back is *more* faithful than replaying one
transaction historically, and by a wide margin. On the pinned
five-transaction bundle every step matched the chain's outcome **and its
exact compute units** — 750, then 140521 four times, to the unit. The
single-transaction sweep of 2026-09-15 put outcome match at 86% and
identical compute at 31%. Nothing is rewound between steps of one slot,
which is the whole reason: the drift in a single replay comes from
reconstructing state, and a bundle after step 0 does not reconstruct.

That cuts the other way too: bundle exactness compounds per-transaction
drift at step 0 only. The two-transaction bundle `05a6d998…` is *not* exact,
because its step 0 alone fails with `Custom(6017)` where the chain
succeeded; step 1 matched exactly, compute included. Expect a bundle to be
exact roughly as often as its first transaction is.

## 6. Done when
- CLI: `svmscope bundle <id>` on three real bundles prints exact baselines.
- The ignored test passes on a pinned bundle.
- `POST /bundle` with a balance mutation at step 0 flips a later step in a
  real sandwich/arb bundle, and the edge table names the pool that carried it.
- README section + CHANGELOG line, in Ali's words.
