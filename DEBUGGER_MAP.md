# svmscope debugger — implementation map

Companion to `DEBUGGER_PLAN.md`. Every claim carries a `file:line` anchor (line numbers as of commit `e39406d`). Registry paths are abbreviated: `REG = ~/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f`.

Versions (Cargo.toml / Cargo.lock): `litesvm 0.15.2` features `["precompiles"]` (Cargo.toml:43), `solana-transaction =4.1.6`, `solana-message =4.4.1`, `solana-instruction =3.4.1` (Cargo.toml:53-61); transitive `solana-transaction-context 4.1.1`, `solana-program-runtime 4.1.1`. Server: `axum 0.8.9`, `tower-http 0.6 (cors)`, `tokio 1.53` (server/Cargo.toml:16-21). CLI has **no clap** — hand-rolled `env::args` (src/main.rs:12-30).

---

## 0. Headline findings (read first — they change the plan)

| # | Finding | Anchor |
|---|---|---|
| 1 | The LiteSVM call is **`svm.send_transaction(self.tx.clone())`**, not `send_transaction_with_config`. `scope.rs:764` is the *RPC client's* submit in `Scope::send_and_capture`. | src/replay.rs:1735; src/scope.rs:760-773 |
| 2 | LiteSVM already returns the **CPI tree**: `TransactionMetadata.inner_instructions: Vec<Vec<InnerInstruction{instruction: CompiledInstruction, stack_height: u8}>>` (outer index = top-level ix), plus `return_data` and `fee`. svmscope's `to_replay_result` **throws these away**. Structure of the tree does not need log parsing; logs are only needed for per-CPI CU and success/failure. | REG/litesvm-0.15.2/src/types.rs:15-23; REG/solana-message-4.4.1/src/inner_instruction.rs:10-16; src/replay.rs:1376-1392 |
| 3 | `LiteSVM::simulate_transaction(tx)` is **read-only** and returns `SimulatedTransactionInfo{meta, post_accounts: Vec<(Address, AccountSharedData)>}` (post state of every *writable* account, including accounts the tx creates). Run all n prefixes against **one** fresh SVM with this instead of rebuilding the SVM (and re-loading every ELF) n times. | REG/litesvm-0.15.2/src/lib.rs:1710-1753, :2033-2057; types.rs:33-36 |
| 4 | sigverify is **off** (`with_sigverify(false)`) and blockhash check is off, so a prefix message needs no re-signing. Only invariant kept by `SanitizedTransaction::try_create`: `signatures.len() == header.num_required_signatures` and `≤ static keys`. Keep the original `signatures` vec and header untouched. Duplicate-signature (`AlreadyProcessed`) is only checked when sigverify is on. | src/replay.rs:279-281; REG/solana-transaction-4.1.6/src/versioned/mod.rs:247-270; REG/litesvm-0.15.2/src/lib.rs:1207-1226, :1619 |
| 5 | LiteSVM 0.15.2 ships an `invocation-inspect-callback` feature: `InvocationInspectCallback::{before_invocation, after_invocation}(svm, tx, program_indices, invoke_context, ..)` — but it fires **once per transaction** (around `process_message`), not per instruction/CPI. LiteSVM vendors `process_message` (top-level loop) in its own `src/message_processor.rs`; CPIs go through agave's `InvokeContext::process_instruction` in `solana-program-runtime`. So Phase B's per-CPI hook is a patch to **litesvm's message_processor.rs** for top-level (redundant with prefix replay) and to **solana-program-runtime** for CPI level. | REG/litesvm-0.15.2/src/lib.rs:1422-1449, :1845-1850, :2153-2171; src/message_processor.rs:28-51; Cargo features lib.rs:290-291 |
| 6 | LiteSVM truncates logs at **10 000 bytes** by default; `svm_from_loaded` never overrides it. A Jupiter route will hit this and the per-step log split breaks. Add `.with_log_bytes_limit(None)`. | REG/litesvm-0.15.2/src/lib.rs:500, :784; src/replay.rs:279-281 |
| 7 | `ReplayResult`, `AccountDiff`, `FieldDiff`, `Explanation`, `IxArg`, `IxAccount`, `CpiEntry`, `AccountRole`, `PreflightIx` derive **only `Serialize`**. `Fixture` derives `Deserialize`, so a `trace` field requires adding `Deserialize` to all of them. | src/replay.rs:26; src/analyze.rs:173,187,204; src/cpi_tree.rs:7,17,29; src/preflight.rs:36,53 |
| 8 | **`static/index.html` is the canonical UI** (served by the Rust server via `include_str!`, and copied to `web/index.html` by `web/build.sh` at Vercel build). `web/index.html` in git is stale (missing the pre-sign overview block). Edit only `static/index.html`. | server/src/main.rs:210-212; web/build.sh:4; `diff web/index.html static/index.html` |
| 9 | The UI has **no client-side routing** — views are toggled with `.hidden`; the only URL parsing is `?api=`. `/debug/<sig>` needs a Vercel rewrite + a Rust route that serves `index.html` + `location.pathname` handling in JS. | static/index.html:1612-1615, 2905-2918 |
| 10 | The failing instruction index is currently only recoverable by parsing `format!("{:?}", TransactionError)` → `"InstructionError(2, Custom(6001))"`. Capture it typed instead. | src/replay.rs:1386 |
| 11 | `ixname::enrich` (IDL-named instructions) needs an `&RpcClient` + a `HashMap<String, Option<Value>>` cache; the replay context holds IDLs as `HashMap<String, Value>` and fixtures have no client. Needs a small refactor for the offline/fixture trace path. | src/ixname.rs:203-210; src/replay.rs:350, :650 |
| 12 | `run_full` (the only thing that calls LiteSVM) is **private** (`fn`, not `pub(crate)`). Put `trace_raw` in `src/replay.rs`, decode in `src/scope.rs` (mirrors `run_with_diff` / `simulate`). | src/replay.rs:1730-1737 |

---

## 1. Execution path today: signature → `Replayed`

### 1.1 Call chain

| Step | Function | Anchor | What it does |
|---|---|---|---|
| 1 | `Scope::replay(input) -> Result<Replay>` | src/scope.rs:241-260 | entry |
| 2 | `Scope::resolve_signature` | :105-123 | 32-byte input = address → latest sig via `getSignaturesForAddress` |
| 3 | `Scope::transaction_json` → `fetch_transaction_json` | :159-162, :126-157 | `getTransaction` (json enc), cached in `tx_cache: Mutex<HashMap<String, Value>>` (:51) |
| 4 | `utils::resolve_account_keys(&tx)` | src/utils.rs:8-21 | static keys + `meta.loadedAddresses.writable` + `.readonly` (canonical runtime order) |
| 5 | `PreState::from_meta(&tx, &keys)` | src/replay.rs:729-764 | pre-balances + pre token amounts + closed-token-account rebuild info |
| 6 | `replay::build_context(client, sig, keys, tx_slot, &pre)` | :811-880 | `slot = client.get_slot()` (NOW, not tx slot — see comment :818-824); `fetch_transaction` :1147 (base64 wire → `bincode::deserialize::<VersionedTransaction>`); `all_keys += ALT table account keys` :827-832; `fetch_loaded` :187-258 (`getMultipleAccounts`; executable → ELF via loader rules :215-241; else `Loaded::Data(Account)`); reconstruct closed accounts :841-848; rewind lamports / token amounts :855-867 |
| 7 | `Scope::preload_idls(&mut ctx)` | src/scope.rs:458-464 | for `ctx.interesting_programs()` (replay.rs:656-669) → `idl_for` (:165-175, cached; `idl::fetch_idl_json` src/idl.rs:31-34) → `ctx.add_idl` (replay.rs:645) |
| 8 | build `Replay { ctx, recorded: Some(OnchainRecord::from_tx_json(&tx)), time_travel, fidelity: Current }` | src/scope.rs:254-259; OnchainRecord :788-834 | |
| 9 | `Replay::run()` = `simulate(&[])` | :1177-1179 | |
| 10 | `Replay::simulate(&[Mutation]) -> Result<Replayed>` | :1182-1197 | calls 11, then `explain_error` :1185-1187, fills `error_name` :1188-1190, `decode_diffs` :1192 |
| 11 | `ReplayContext::run_with_diff(&self, muts) -> (ReplayResult, Vec<RawAccountDiff>)` | src/replay.rs:583-607 | calls 12; then for every `Loaded::Data` in `self.loaded`: `svm.get_account(addr)`; emits `RawAccountDiff` when lamports or data changed. **Accounts created by the tx are not in `loaded` → never diffed** (comment :579-582) |
| 12 | `ReplayContext::run_full(&self, muts) -> (ReplayResult, LiteSVM)` | :1730-1737 | `fresh_svm()` → resolve+apply mutations → **`svm.send_transaction(self.tx.clone())`** :1735 → `to_replay_result` |
| 13 | `ReplayContext::fresh_svm()` | :568-577 | `svm_from_loaded(&self.loaded, &self.feature_toggles, slot)` :276-316 = `LiteSVM::new().with_sigverify(false).with_blockhash_check(false)` (+ feature set rebuild :282-302), `set_account` / `add_program` for each loadable :303-314; then `base_clock` :541-554 and `time_travel.apply` :412-452, `svm.set_sysvar::<Clock>` :575 |
| 14 | `to_replay_result(TransactionResult) -> ReplayResult` | :1376-1392 | keeps `logs`, `compute_units_consumed`, `format!("{:?}", err)`; **drops** `inner_instructions`, `return_data`, `fee`, `signature` |

### 1.2 Exact LiteSVM API used and what it returns

```rust
// REG/litesvm-0.15.2/src/lib.rs:1659
pub fn send_transaction(&mut self, tx: impl Into<VersionedTransaction>) -> TransactionResult
// lib.rs:1710
pub fn simulate_transaction(&self, tx: impl Into<VersionedTransaction>)
    -> Result<SimulatedTransactionInfo, FailedTransactionMetadata>

// types.rs:57
pub type TransactionResult = Result<TransactionMetadata, FailedTransactionMetadata>;
// types.rs:15-23
pub struct TransactionMetadata {
    pub signature: Signature,
    pub logs: Vec<String>,
    pub inner_instructions: InnerInstructionsList,   // Vec<Vec<InnerInstruction>> per top-level ix
    pub compute_units_consumed: u64,
    pub return_data: TransactionReturnData,           // { program_id: Pubkey, data: Vec<u8> }
    pub fee: u64,
}
// types.rs:33-36
pub struct SimulatedTransactionInfo { pub meta: TransactionMetadata, pub post_accounts: Vec<(Address, AccountSharedData)> }
// types.rs:40-43
pub struct FailedTransactionMetadata { pub err: TransactionError, pub meta: TransactionMetadata }
// REG/solana-message-4.4.1/src/inner_instruction.rs:10-16
pub struct InnerInstruction { pub instruction: CompiledInstruction, pub stack_height: u8 }
```

- `send_transaction` commits post accounts into the SVM (`sync_accounts`, lib.rs:1701-1703) → read via `svm.get_account(&Address) -> Option<Account>` (lib.rs:820). Failed txs are not committed.
- `simulate_transaction` computes `post_accounts` = the accounts the message marks writable (lib.rs:2049-2056), never mutates the SVM.
- Other useful API: `get_sysvar/set_sysvar` :963-985, `with_log_bytes_limit(Option<usize>)` :784, `with_compute_budget` :562, `get_transaction(&Signature)` :986 (history), `set_invocation_inspect_callback` :1845 (feature-gated).

### 1.3 Diffs

- Raw: `RawAccountDiff { address, owner, lamports_before, lamports_after, data_before: Vec<u8>, data_after: Vec<u8> }` src/replay.rs:357-364, produced by `run_with_diff` :589-605 by comparing `svm.get_account(addr)` to `self.loaded` (pre-mutation state!— mutations are applied to the SVM only, `loaded` is the un-mutated baseline).
- Decoded: `decode_diffs(raw, idls) -> Vec<AccountDiff>` src/scope.rs:1522-1558: `decode::decode_bytes(owner, bytes)` (built-in SPL/ALT/stake/nonce) `.or_else(idl::decode_with_idl)`; zips `fields` **by position** on before/after and keeps `fb.value != fa.value`; `raw_data_changed = data differs && no field diffs`.

### 1.4 Struct definitions (verbatim fields)

```rust
// src/replay.rs:26-41
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, Default)]
pub struct ReplayResult { pub success: bool, pub error: Option<String>,
    #[serde(skip_serializing_if="Option::is_none")] pub error_name: Option<String>,
    pub logs: Vec<String>, pub compute_units: u64 }

// src/analyze.rs:187-201
#[derive(Debug, Clone, Serialize)]
pub struct AccountDiff { pub address: String, pub owner: String, pub lamports_before: u64,
    pub lamports_after: u64, pub fields: Vec<FieldDiff>, pub raw_data_changed: bool }
// src/analyze.rs:204-215
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FieldDiff { pub name: String, #[serde(rename="type")] pub ty: String, pub before: String, pub after: String }
// src/analyze.rs:173-184
#[derive(Debug, Clone, Serialize)]
pub struct Explanation { pub title: String, pub detail: String,
    #[serde(skip_serializing_if="Option::is_none")] pub program: Option<String>, pub raw: String }
// src/analyze.rs:153-170  (HTTP wire shape of a replay; Replayed::into_report scope.rs:1376-1384)
pub struct SimulationReport { pub replay: ReplayResult, pub clock: Option<String>,
    pub explain: Option<Explanation>, pub diffs: Vec<AccountDiff>, pub preflight: Option<PreflightOverview> }

// src/scope.rs:1361-1372
#[derive(Debug, Serialize)]
pub struct Replayed { pub result: ReplayResult, pub diffs: Vec<AccountDiff>,
    pub clock: Option<String>, pub explain: Option<Explanation> }

// src/scope.rs:1000-1007  (private fields)
pub struct Replay { ctx: ReplayContext, recorded: Option<OnchainRecord>, time_travel: TimeTravel, fidelity: Fidelity }

// src/replay.rs:1170-1219
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mutation {
    Lamports { address: String, value: u64 },
    Data     { address: String, bytes: Vec<u8> },
    DataPatch{ address: String, offset: usize, bytes: Vec<u8> },
    Field    { address: String, field: String, value: i128 },
    Owner    { address: String, owner: String },
}

// src/scope.rs:788-805
pub struct OnchainRecord { pub success: bool, pub error: Option<String>, pub fee: u64,
    pub compute_units: Option<u64>, pub slot: Option<u64>, pub block_time: Option<i64>, pub logs: Vec<String> }
```

Other entry points that build a `ReplayContext` the same way (all end in the same `run_full`): `Scope::replay_at_slot` src/scope.rs:283-343 (`build_context_at_slot` replay.rs:890-947), `Scope::replay_at` :355-401, `Scope::preflight(b64)` :411-413 → `parse_unsigned` :1392-1430 → `preflight_tx` :416-428 → `replay::preflight_context` replay.rs:991-1032, `Replay::from_fixture` scope.rs:1083-1090.

---

## 2. How the transaction message is held

```rust
// src/replay.rs:330-353 (all fields private to replay.rs)
pub(crate) struct ReplayContext {
    signature: String,
    tx: VersionedTransaction,                 // the whole signed tx, cloned into send_transaction at :1735
    loaded: Vec<(Address, Loaded)>,           // Loaded::Data(Account) | Loaded::Program(Vec<u8> ELF)  (:171-174)
    slot: Option<u64>, block_time: Option<i64>,
    time_travel: TimeTravel,
    idls: HashMap<String, serde_json::Value>, // program id -> IDL (:350)
    feature_toggles: Vec<FeatureToggle>,
}
```

- **Account keys**: not stored separately. Static keys = `tx.message.static_account_keys()`. ALT **table** accounts are loaded as `Loaded::Data` (keys appended at replay.rs:827-832 / :900-906 / :1005-1011). ALT-**resolved** addresses are *not* stored; LiteSVM resolves lookups itself during sanitize using its own accounts DB as `AddressLoader` (lib.rs:1211-1217), which is why the clock slot must be ≥ the ALT's last-extended slot (comment replay.rs:335-339). `resolve_alt_addresses(client, &tx) -> (writable, readonly)` replay.rs:956-985 reads the ALT layout (addresses at offset 56, 32 bytes each) **via RPC**; for the debugger write a local twin that reads the same bytes from `self.loaded`.
- **Message layouts** (all fields `pub`):

```rust
// REG/solana-message-4.4.1/src/versions/mod.rs:63-67
pub enum VersionedMessage { Legacy(LegacyMessage), V0(v0::Message), V1(v1::Message) }
// legacy.rs:109-131
pub struct Message { pub header: MessageHeader, pub account_keys: Vec<Address>, pub recent_blockhash: Hash, pub instructions: Vec<CompiledInstruction> }
// versions/v0/mod.rs:86-122
pub struct Message { pub header, pub account_keys, pub recent_blockhash, pub instructions: Vec<CompiledInstruction>, pub address_table_lookups: Vec<MessageAddressTableLookup> }
// versions/v0/mod.rs:58-68
pub struct MessageAddressTableLookup { pub account_key: Address, pub writable_indexes: Vec<u8>, pub readonly_indexes: Vec<u8> }
// compiled_instruction.rs:35-44
pub struct CompiledInstruction { pub program_id_index: u8, pub accounts: Vec<u8>, pub data: Vec<u8> }
```
  Accessors on `VersionedMessage`: `header()` :78, `static_account_keys()` :86, `address_table_lookups()` :94, `is_signer(i)` :104, `is_maybe_writable(i, reserved)` :120, `instructions()` :198, `recent_blockhash()` :179.

- **Where to build a prefix message**: new methods next to `run_full` in src/replay.rs (after :1737):

```rust
/// The transaction with only instructions `keep` (indexes into message.instructions(), in order).
fn tx_with_instructions(&self, keep: &[usize]) -> Result<VersionedTransaction> {
    let mut tx = self.tx.clone();
    let pick = |ixs: &Vec<CompiledInstruction>| keep.iter().map(|&i| ixs[i].clone()).collect::<Vec<_>>();
    match &mut tx.message {
        VersionedMessage::Legacy(m) => m.instructions = pick(&m.instructions),
        VersionedMessage::V0(m)     => m.instructions = pick(&m.instructions),
        _ => return Err(Error::InvalidSpec("unsupported message version".into())),
    }
    Ok(tx) // signatures + header untouched: sanitize only checks signatures.len()==num_required (sigverify off)
}
/// Generalisation of run_full (replay.rs:1730) taking the tx to run.
fn run_tx(&self, tx: VersionedTransaction, mutations: &[Mutation]) -> Result<(TransactionResult, LiteSVM)>
```
  `run_full` becomes `self.run_tx(self.tx.clone(), m)`. Prefix k = `keep = compute_budget_ix_indexes ∪ (0..=k)` (see §10 gotcha 1). Executing "against the same initial state" is automatic: `fresh_svm()` rebuilds from `self.loaded` each call — or, cheaper, build one `fresh_svm()` + apply mutations once and call `svm.simulate_transaction(prefix_k)` for each k (read-only; §0 #3).

- **Signature / sigverify**: `svm_from_loaded` sets `with_sigverify(false)` (replay.rs:280) → LiteSVM takes `execute_transaction_no_verify` (lib.rs:1675-1679) → `sanitize_transaction_no_verify_inner` (lib.rs:1207-1226) → `SanitizedTransaction::try_create(tx, MessageHash::Compute, Some(false), &accounts, &reserved)` → `SanitizedVersionedTransaction::try_from` → `VersionedTransaction::sanitize` → `sanitize_signatures_inner` (solana-transaction versioned/mod.rs:255-270): requires `num_required_signatures == signatures.len()` and `signatures.len() <= static_account_keys.len()`. Placeholder signatures are fine (`parse_unsigned` already relies on this, scope.rs:1413-1419). No re-signing needed for prefixes.

---

## 3. Log parsing that exists

| What | Anchor | Notes |
|---|---|---|
| `CuUsage { program: String, cu: u64 }` | src/compute.rs:17-23 | per-program aggregate |
| `cu_per_program(tx_json)` | src/compute.rs:52-148 | parses `"Program <id> invoke [n]"` (:74-77, counts invocations) and `"Program <id> consumed N of M compute units"` (:79-90, sums per program); rejects forged `Program log:` lines (`w[1].ends_with(':')`); fixed builtin costs System/ComputeBudget = 150 (:28-31); remainder of `meta.computeUnitsConsumed` attributed to unpriced natives (:114-134); sorted desc. **No depth, no per-invocation rows.** |
| `cu_from_logs(logs, total)` | :43-48 | wraps logs into a fake tx JSON → `cu_per_program` |
| `preflight::compute_breakdown(logs, total_cu)` | src/preflight.rs:142-144 | public re-export of the above (lib.rs:159) |
| `explain_error` — failing program | src/scope.rs:1441-1451 | last line `Program <id> failed…`, validated as an `Address` |
| `explain_error` — Anchor error line | :1454-1473 | `Error Code: X.` / `Error Message: …` |
| `diagnose::parse_failed_program` | src/diagnose.rs:161-169 | `Program <id> failed` |
| `diagnose::parse_anchor_error` | :172-187 | `(name, number, message)` |
| `diagnose::parse_instruction_error` | :147-158 | from `meta.err` JSON `{"InstructionError":[idx,{"Custom":code}]}` — on-chain only |
| `diagnose::framework_error(code)` | :194-236 | Anchor framework code table (2000s/3000s) — reusable per step |
| UI `renderLogs` | static/index.html:1928-1966 | reconstructs depth in JS from `invoke [n]` / `success` lines; indents by depth |
| `cpi_tree::build_cpi_tree(tx_json)` | src/cpi_tree.rs:58-131 | flat list with `stack_height` from **getTransaction `innerInstructions`** (on-chain only); `CpiEntry` :29-54 |

**Nothing in Rust reconstructs nesting from logs.** For the replay you don't have to: `meta.inner_instructions[i]` is the exact list of CPIs under top-level ix `i` with `stack_height` (2 = direct CPI), same shape `build_cpi_tree` consumes from RPC. Agave log grammar you need for CU/outcome per node (all produced by the runtime, in pre-order):

```
Program <id> invoke [<depth>]          depth 1 = top level
Program log: …  | Program data: … | Program return: <id> <b64>
Program <id> consumed <n> of <m> compute units     (BPF programs only; builtins emit none — compute.rs:3-11)
Program <id> success
Program <id> failed: <reason>
Log truncated                                     (when the 10 000-byte limit trips — §0 #6)
```

---

## 4. Decoding that exists

### 4.1 Instructions

```rust
// src/ixname.rs:203-210
pub(crate) fn enrich(client: &RpcClient, idl_cache: &mut HashMap<String, Option<Value>>,
    program: &str, data: &[u8], account_indexes: &[usize], account_keys: &[String])
    -> (Option<String> /*name*/, Vec<IxArg>, Vec<IxAccount>)
```
- Native tables: token :22-44, system :47-60, compute budget :63-71, ATA/memo :229-237; positional account names :108-141; native args :144-198 (amount/lamports/units/decimals).
- Anchor: `idl::find_ix(idl, data) -> Option<IxDef>` src/idl.rs:231-237 (8-byte discriminator match) → `idl::decode_ix_args(&ix, data) -> Vec<(name, ty, value)>` :243-266 (fixed-size scalars only; stops at first variable-length arg, value `""`); account names from `ix.accounts[i].name` titleized :260-264; extras named `Remaining Account #n` :278-281.
- Callers: `Scope::analyze` src/scope.rs:189-204 (locks `idl_cache`), `preflight::build_overview` src/preflight.rs:287-301.
- Types: `IxAccount { name: Option<String>, address: String }` src/cpi_tree.rs:8-14; `IxArg { name, ty (json "type"), value: String }` :17-26; `CpiEntry { index, program, stack_height, name, accounts, args, data(skip), account_indexes(skip) }` :29-54; `PreflightIx { index, program, name, args, accounts }` src/preflight.rs:53-66.
- Account roles: `AccountRole { address, signer, writable, program, fee_payer }` src/preflight.rs:36-50; computed from header + ALT ordering at :381-416 (static: writable-signers, ro-signers, writable-unsigned, ro-unsigned; then ALT writable, ALT readonly). Reuse this block for per-step roles: role of `ix.accounts[j]` = `roles[idx]`.
- `Scope::program_instructions` / `idl::instructions(&idl) -> Vec<IdlInstruction>` src/scope.rs:578-587, src/idl.rs:181-202 (builder-side listing).

### 4.2 Accounts

- `decode::decode_bytes(owner, data) -> Option<DecodedAccount>` src/decode.rs:214-245: SPL Token account (165) / Mint (82) / Token-2022 with extensions (type byte @165) / ALT / Stake / Nonce. `infer_layout(data)` :438-488 (heuristic `pubkey@N`, `u64@N`).
- `idl::decode_with_idl(idl, data) -> Option<DecodedAccount>` src/idl.rs:618-645: match account discriminator → `types[name]` → `walk_fields` :379-611 (structs inline with dotted names, enums, `[T;N]`, `string`, `option`, `vec` ≤32 elems; stops at unknown types).
- `DecodedAccount { type_name, fields: Vec<Field> }`, `Field { name, offset, ty ("type"), size, value: String, editable: bool, note }` src/decode.rs:20-50.
- `Scope::decode_account(addr, user_idl)` src/scope.rs:550-574 (RPC; user IDL wins). `ReplayContext::decode_pre(addr)` src/replay.rs:680-696 (offline, from `loaded` + `idls`). `find_field(dec, name)` :1524-1549 (exact or unambiguous last dot-segment), `read_field_int` :1554-1577.
- `describe_accounts(client, keys) -> Vec<AccountInfo>` src/decode.rs:493-557 (what `analyze.accounts` and the UI editor use).

### 4.3 Errors

```rust
// src/scope.rs:1435-1438
fn explain_error(r: &ReplayResult, idls: &HashMap<String, serde_json::Value>) -> Option<Explanation>
```
Inputs: `r.error` (Debug string of `TransactionError`), `r.logs`. Order: failing program = last `Program X failed` line (:1441-1451) → Anchor `Error Message:` line (:1454-1473) → `Custom(code)` + `idl::error_for_code(idl, code)` (src/idl.rs:106-116, `IdlError{code,name,msg}`) (:1476-1494) → canned fallbacks (:1497-1511). Called only on failure (scope.rs:1185-1187). For a per-step error call it with that prefix's `ReplayResult` — it will naturally find the step's failing program because the failing step is always the last invoke in a failed prefix run.

---

## 5. Mutations and counterfactuals

| Item | Anchor |
|---|---|
| `Mutation::{lamports, data, owner, patch, field}` ctors | src/replay.rs:1223-1273; `address()` :1275-1283 |
| JSON shape `MutationInput {kind: lamports\|data\|field}` → `into_mutation` | src/spec.rs:56-136 (`value: i64` for field; hex bytes for data) |
| `resolve_mutation` (Field → DataPatch via `decode_pre` + `find_field` + `encode_field_value`) | src/replay.rs:1709-1726; `encode_field_value` :1290-1325 |
| `apply_mutation(&mut LiteSVM, &Mutation)` — `get_account` → edit → `set_account` | :1331-1370 |
| `validate_mutations` (fail-fast; bounds) | :1742-1775 |
| `run_full` applies mutations after `fresh_svm()`, before send | :1730-1737 |
| `Replay::simulate(&[Mutation]) -> Replayed` | src/scope.rs:1182-1197 |
| `Replay::account_after(&[Mutation], addr) -> Option<AccountState>` → `run_and_read_account` | src/scope.rs:1206-1217; src/replay.rs:612-622 |
| `Replay::find_threshold(lo, hi, mutate: Fn(u64)->Vec<Mutation>) -> Option<Threshold>` → `search::search_threshold` | src/scope.rs:1239-1246; src/search.rs:34-69; `Threshold{flips_at, low_success, high_success, evaluations}` :15-25 |
| `Replay::minimize_mutations(&[Mutation]) -> Vec<Mutation>` (greedy delta-debugging on `result.success`) | src/scope.rs:1255-1273 |
| `Replay::compare_patch` / `replace_program` (ELF A/B) | :1293-1309, :1280-1286 |
| `scan::scan_breaking_points` (auto threshold scan over fields) | src/scan.rs:75; `BreakingPoint` :27 |

"Edit at step k → re-run from 0" is `Replay::trace(&mutations)`; `find_threshold` / `minimize_mutations` take a success predicate today — give `Trace` a `fn outcome(&self) -> bool` (or expose `failed_step: Option<usize>`) and add `trace`-based twins, or generalise both helpers over a `Fn(&[Mutation]) -> Result<bool>`.

---

## 6. Fixtures

```rust
// src/fixture.rs:52-82
pub struct Fixture {
    #[serde(default = "default_version")] pub version: u32,          // FIXTURE_VERSION = 2 (:20)
    pub signature: String,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub captured_slot: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub captured_block_time: Option<i64>,
    pub tx_b64: String,                                                // bincode(VersionedTransaction), base64
    pub entries: Vec<FixtureEntry>,                                    // Data{address,owner,lamports,data_b64} | Program{address,elf_b64} (:30-49)
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")] pub idls: BTreeMap<String, Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub recorded: Option<OnchainRecord>,
}
```
- Write: `ReplayContext::to_fixture` src/replay.rs:1042-1075 (slot/block_time from the context, `recorded: None`) → `Replay::to_fixture` src/scope.rs:1337-1341 (adds `recorded`) → `Fixture::to_json` src/fixture.rs:86-89 (pretty JSON). `Scope::capture(sig)` src/scope.rs:451-453.
- Read: `Fixture::from_json` src/fixture.rs:93-104 (errors if `version > FIXTURE_VERSION`; unknown fields are ignored — no `deny_unknown_fields`) → `Replay::from_fixture` src/scope.rs:1083-1090 → `ReplayContext::from_fixture` src/replay.rs:1079-1143 (restores tx, accounts, ELFs, idls; `fidelity: Current`, `recorded` from file).
- Surfaces: CLI `freeze`/`upgrade` src/main.rs:79-98 / :64-75; server `GET /freeze/{sig}` server/src/main.rs:825-843; suites reference fixtures by path (src/spec.rs:484-505, main.rs:206-220); tests `tests/offline_fixture.rs` + `tests/fixtures/{counter_increment,vesting_precliff_revert}.json` (keys: version, signature, captured_slot, captured_block_time, tx_b64, entries, idls, recorded).
- **Where `trace` goes**: after `recorded` at src/fixture.rs:81 — `#[serde(default, skip_serializing_if = "Option::is_none")] pub trace: Option<crate::trace::Trace>`. Optional field keeps v2 readable by older builds; bump `FIXTURE_VERSION` to 3 only if you want old builds to refuse the file. Requires `Deserialize` on everything inside `Trace` (§0 #7). Fill it in a new `Replay::to_fixture_with_trace()` or in the CLI/server after `trace()`.

---

## 7. CLI (`src/main.rs`)

- No clap. `args: Vec<String>`; `args[1]` is either a subcommand word or a signature (:12-16). `flag(name)` closure :19-24 reads `--cluster/--rpc` → `svmscope::resolve_rpc_url` :25-29 (src/lib.rs:185-206) → `Scope::new` :30.
- Subcommand dispatch is a chain of `if signature == "test" | "report" | "idl" | "upgrade" | "freeze"` blocks :34-98, each returning early. Default path (analyze + replay + `--mutate addr:lamports`) :100-171. `--json` :100-110 prints `Analysis` via `serde_json::to_string_pretty`.
- Printing helpers: tree :115-126 (`#idx name (program)`, CPIs as `└─ [depth]` indented by `"    ".repeat(stack_height-2)`), `print_replay(label, &ReplayResult)` :290-302 (status line + every log line). Suite runner `run_tests` :254-287, HTML report `run_report` :234-250, shared loader `load_and_run_suite` :176-231 (shows how a fixture path vs signature is chosen — copy this for `debug <sig|fixture.json>`).
- Usage string to extend: :16.
- **Add**: `if signature == "debug" { return run_debug(&scope, &args); }` before :100. `run_debug`: input = `args[2]` (sig / `*.json` fixture / base64 → route like `runInput` does in the UI: 64-byte b58 = sig, file exists = fixture, else `Scope::preflight`), `--json` → `serde_json::to_string_pretty(&trace)`, `--mutate` reuse :143-162 parsing, `--fixture-out path` → write fixture with trace. Text output: one row per step `path  name(program)  cu  ✓/✗`, CPI rows indented by depth, failing row followed by `explain.title — detail`, then that step's `diffs` as `addr field: before → after`.

---

## 8. Server (`server/src/main.rs`, axum 0.8)

- Router :1027-1052 (path params use `/{name}` syntax). Layers order: `cache_layer` :937-983 → `rate_limit` :886-909 → `CorsLayer::permissive()` :1025. Bind `HOST`/`PORT` :1054-1082, `into_make_service_with_connect_info` for peer IP.
- Handler pattern (copy `replay_report_handler` :506-538): `Json<Req>` or `Path<String> + Query<ClusterQuery>` (:59-63) → `rpc_for(cluster, rpc)` :166-190 → `tokio::task::spawn_blocking(move || -> Result<T, svmscope::Error> { … })` → `.map_err(task error 500)` → `result.map(Json).map_err(lib_err)`. `lib_err` :216-232 maps `TransactionNotFound|NoSignatures|AccountNotFound`→404, `Rpc|MalformedRpcResponse`→502 with a generic message (never echo RPC URLs/keys), else 400. Per-request caps `cap()` :263-271 (256 mutations, 64 scenarios :260-261). Body limit is axum's default 2 MB (comment :259).
- Existing routes:

| Route | Handler | Request | Response |
|---|---|---|---|
| `GET /` | `index` :210 | | `static/index.html` via `include_str!` |
| `GET /api` | `api_index` :847-863 | | `{name, version, custom_rpc, endpoints{}}` — UI reads `custom_rpc` |
| `GET /analyze/{sig}?cluster&rpc` | :235-255 | | `Analysis` (src/analyze.rs:10-33) |
| `POST /simulate` | :274-307 | `SimRequest{signature, mutations: Vec<MutationInput>, time_travel, features, cluster, rpc}` :194-207 | `ReplayResult` |
| `POST /simulate_suite` | :310-359 | `spec::SuiteRequest` | `Vec<ScenarioOutcome>` |
| `POST /preflight` | :382-410 | `PreflightRequest{transaction(b64), mutations, time_travel, features, cluster, rpc}` :363-378 | `ReplayResult` |
| `POST /preflight_report` | :456-502 | same | `SimulationReport` (+`preflight` overview, compute via `compute_breakdown` :485-488) |
| `POST /replay_report` | :506-538 | `SimRequest` | `SimulationReport` |
| `GET /instructions/{program}` · `POST /idl_instructions` · `POST /decode_account` | :588-604 · :580-584 · :556-577 | `IdlRequest` :542-552 | IDL listings / `AccountInfo` |
| `GET /account/{addr}` · `GET /signatures/{addr}` | :413-431 · :434-452 | | `AccountOverview` · `Vec<SigInfo>` |
| `GET /replay/{sig}` | :607-627 | | `ReplayResult` |
| `GET /replay_at_slot/{sig}` | :648-689 | env `SVMSCOPE_ARCHIVE_URL` | `ReplayAtSlotResponse` :632-644 |
| `GET /counterfactual/{sig}?account&lo&hi` · `GET /scan/{sig}` · `GET /diagnose/{sig}` | :721-770 · :775-806 · :810-822 | | `CounterfactualResponse` :704-716 · `Vec<BreakingPoint>` · `Diagnosis` |
| `GET /freeze/{sig}` | :825-843 | | `Fixture` JSON |
| `GET /stats?token=` | :996-1006 | env `SVMSCOPE_STATS_TOKEN` | private usage (server/src/stats.rs) |

- Cache: GET only, prefixes `/analyze /account /signatures /replay` (:939-943), key = full URI, TTL 120 s, 500 entries (server/src/guard.rs:28-30). Rate limit: 40 req/min per client (guard.rs:74-75), only for paths in `endpoint_label` :912-933 (also drives `stats::record`). Client id = rightmost `X-Forwarded-For` :873-882.
- RPC handling: default `SVMSCOPE_RPC_URL`/`RPC_URL` :27-31; per-cluster `SVMSCOPE_RPC_URL_{MAINNET,DEVNET,TESTNET}` :46-55; caller `?rpc=` only if `SVMSCOPE_ALLOW_CUSTOM_RPC=1` :133-138 and passes SSRF vetting :96-123; `cluster` limited to named clusters :146-160.
- Keepalive/prewarm: **no server-side keepalive**. The UI fires `GET /api` on page load to wake the Render free container (static/index.html:1617-1633); `render.yaml` `healthCheckPath: /api`; Dockerfile builds `-p svmscope-server` with `static/` copied in (Dockerfile:20-25). `fly.toml` also present.
- **Where new routes go** (in the `Router::new()` chain :1030-1047):
  - `.route("/trace", post(trace_handler))` — body `TraceRequest { signature: Option<String>, transaction: Option<String> /*b64*/, mutations, time_travel, features, cluster, rpc }` → `Json<Trace>`.
  - `.route("/trace/{signature}", get(trace_get_handler))` — cacheable; add `"/trace"` to the cache prefix list :940-943 **and** to `endpoint_label` :914-931 (note `starts_with` ordering: put `/trace` before nothing conflicting).
  - `.route("/debug/{signature}", get(index))` — serves the SPA so `svmscope.onrender.com/debug/<sig>` deep-links work when the Rust server hosts the UI. Keep JSON at `/trace/…` to avoid the HTML/JSON clash on `/debug/…`.
  - Stored traces ("no re-fetch"): nothing persistent exists besides the 120 s memory cache and the stats file. v1 options: (a) recompute on `GET /trace/{sig}` (cache absorbs share bursts), (b) `SVMSCOPE_TRACE_DIR` + write `Fixture{trace}` JSON keyed by signature, read it back in `trace_get_handler` before hitting RPC. Add the entry to `api_index` :854-861.

---

## 9. Web UI (`static/index.html`, 3681 lines; no build step)

- Layout: CSS :10-1250, body :1252-1585, `<script src="config.js">` :1589 (sets `window.SVMSCOPE_API`; `web/config.js` = `https://svmscope.onrender.com`), JS :1590-3679. Vercel: `web/vercel.json` (`cleanUrls: true`, no rewrites), `web/build.sh` copies `../static/index.html`.
- Server calls: `const API = ?api= || window.SVMSCOPE_API || ''` :1612-1614; `api(path, init) = fetch(API + path, init)` :1615; cluster: `clusterParams()` :1640-1646 (`{cluster}` or `{rpc}`), `clusterQ()` :1647-1650 for GETs; POST bodies spread `...clusterParams()` (e.g. :1990, :2846, :3279). Globals `analysis, currentSig, mutations, mutableAccounts` :1607; `timeTravel` / `activeFeatures` (set by `setTimeTravel` :3210, `toggleFeature` :3468).
- Routing: none. Sections are `#hero #status #txlist #sim #out` toggled via `.hidden` — `showSim(on)` :2905-2918, `runInput()` :2684-2712 (sig → `analyze()` :2797-2834; address → `showAddress` :2716; base64 → preflight), brand click resets :3513-3519. Keyboard: `/` focuses search :3655-3662 (pattern for `j`/`k`).
- Render functions reusable for the debugger:
  - **Tree**: `renderTree(entries)` :1843-1866 — rows `.tnode` with `.ix` (#idx), `.rail` (`│   └── ` by `stack_height`), `progChip(program)`, `.ixname`, `.depth`, click toggles `#ixd-<i>` detail; `renderIxDetail(e, i)` :1870-1884 (args rows `.ix-row .ix-k/.ix-ty/.ix-v`, accounts rows with `copyPill`). CSS `.tree` :621, `.tnode*` :622-637, `.ixdetail` :639-645, `.ix-row` :654.
  - **Diff table**: inside `renderReport(rep, el)` :3289-3321 — `.diffs > .dcard > .dhead(copyPill+progPill+lamports Δ) + .drow(.fname .f-off | .fv .old → .new)` :3303-3317; CSS :1233-1244. Extract that map into `renderAccountDiffs(diffs)`.
  - **Logs**: `renderLogs(logs)` :1928-1966 → `.logs > .lg-line` with depth indent; CSS :780-795. Add `data-i` per line and a `.lg-line.on` highlight class for the selected step's range.
  - **Error banner**: `.explain` block :3296-3302 (title/detail/meta), CSS :1223-1231; verdict `.verdict.ok|bad` :2872-2873 / CSS :1019-1027; `.err-box` :771; before/after `.compare` :2875-2880 / CSS :1029-1033.
  - Helpers: `progChip` :1750, `progPill` :1756, `copyPill` :1746, `wireCopy` :2671, `fmtSol` :1728, `fmt` :1594, `esc` :1592, `short` :1593, `PROGRAMS` registry :1653-1676, `mintInfo` :1692.
  - **Editing**: field editors `buildEditorBody(a)` :2551, `encodeField(field, val)` :1713 (hex for u64/u8/bool/pubkey), `collectAllEdits()` :2618 → mutation objects `{kind:'data', address, offset, bytes_hex}` / `{kind:'field', address, field, value}` / `{kind:'lamports', address, lamports}` — same JSON `POST /trace` should accept (`spec::MutationInput`).
  - Share: `freezeFixture()` :2420 hits `GET /freeze/{sig}` and downloads JSON; reuse for "save trace".
- **Adding `/debug/<sig>`**: (1) `web/vercel.json` add `"rewrites": [{"source": "/debug/:sig", "destination": "/index.html"}]`; (2) Rust route `/debug/{signature}` → `index` (§8); (3) new `<div id="debug" class="hidden">` after `#out` (:1574) with `.grid`-like 3-column layout: `#dbgTree` (reuse `.tree/.tnode` markup with a `.step.fail` marker), `#dbgDetail` (ix args/accounts via `renderIxDetail`-style rows + `renderAccountDiffs`), `#dbgLogs` (`renderLogs` with highlight); banner above using `.explain`; (4) `showDebug(sig)` → `api('/trace/'+sig+clusterQ())` (or POST `/trace` with mutations) → `renderTrace(trace)`; select step → update detail + scroll logs to `step.logs[0]`; `j/k` handler in the global keydown; (5) on load: `if (location.pathname.startsWith('/debug/')) showDebug(decodeURIComponent(location.pathname.slice(7)))`; `history.pushState` when opened from the analyze page (button next to "Replay now" in `renderReplayPanel` :2000-2006). OG image: server can't render images today; simplest is a Vercel edge/OG route or a static PNG per outcome (later).

---

## 10. Proposed new code, mapped

### 10.1 `src/trace.rs` (new) — types

```rust
use crate::{AccountDiff, Explanation, IxAccount, IxArg, ReplayResult};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Trace {
    pub signature: String,
    pub steps: Vec<Step>,                 // pre-order: "0", "0.0", "0.1", "1", …
    pub result: ReplayResult,             // full-transaction run (prefix n-1)
    pub explain: Option<Explanation>,     // from Replay::simulate
    pub failed_step: Option<usize>,       // index into steps
    pub clock: Option<String>,            // ReplayContext::describe_clock (replay.rs:558)
    pub fidelity: String,                 // Fidelity::label() (scope.rs:860)
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Step {
    pub path: String, pub depth: u8, pub index: usize /*top-level ix index*/,
    pub program: String, pub name: Option<String>, pub args: Vec<IxArg>,
    pub accounts: Vec<StepAccount>,       // { address, name: Option<String>, signer, writable }
    pub cu_consumed: Option<u64>, pub logs: (usize, usize),
    pub diffs: Vec<AccountDiff>,          // top-level only in Phase A; empty for CPIs
    pub state_known: bool,                // false for CPI rows until Phase B
    pub error: Option<StepError>,         // { raw: String, explain: Option<Explanation> }
    pub return_data: Option<(String, Vec<u8>)>,
    pub success: bool,
}
pub struct TraceDiff { pub steps: Vec<StepDiff> /* path, before: Option<StepSummary>, after: Option<StepSummary>, changed: bool */, pub outcome_changed: bool }
impl Trace { pub fn diff(&self, other: &Trace) -> TraceDiff }
```
Register in `src/lib.rs`: `mod trace;` (:120-146) and `pub use trace::{Trace, Step, StepError, TraceDiff};` (:148-172). Add `serde::Deserialize` to the structs listed in §0 #7.

### 10.2 `src/replay.rs` — raw trace primitive (private-field access lives here)

Add after `run_full` (:1737):

```rust
pub(crate) struct RawStepRun {          // one prefix run
    pub keep: Vec<usize>,               // instruction indexes executed
    pub result: TransactionResult,      // full litesvm meta (logs, inner_instructions, return_data, cu, err)
    pub post: Vec<(Address, Account)>,  // every message account after the run (writable ones)
}
pub(crate) fn trace_raw(&self, mutations: &[Mutation]) -> Result<Vec<RawStepRun>> {
    let mut svm = self.fresh_svm();                              // :568 (once)
    for m in mutations { apply_mutation(&mut svm, &self.resolve_mutation(m)?)?; }   // :1331 / :1709
    let n = self.tx.message.instructions().len();
    let cb: Vec<usize> = /* indexes whose program is ComputeBudget111… */;
    (0..n).map(|k| {
        let keep = cb ∪ (0..=k) in order;
        let tx = self.tx_with_instructions(&keep)?;
        let sim = svm.simulate_transaction(tx);                 // read-only, REG litesvm lib.rs:1710
        // post accounts: Ok(info) => info.post_accounts (AccountSharedData -> Account via .into()),
        //                Err(_)   => none (failed prefix has no committed post state)
    }).collect()
}
```
Also: `pub(crate) fn message_account_keys(&self) -> Vec<String>` = static keys + ALT-resolved from `self.loaded` (port of `resolve_alt_addresses` :956-985 reading `Loaded::Data` bytes at offset 56), and `pub(crate) fn pre_account_owned(&self, addr) -> Option<Account>` (wrap `pre_account` :625). Extend `to_replay_result` :1376 with `failed_instruction: Option<u8>` from `TransactionError::InstructionError(i, _)` (add the field to `ReplayResult` with `skip_serializing_if`). Add `.with_log_bytes_limit(None)` at :279-281.

### 10.3 `src/scope.rs` — `Replay::trace`

```rust
impl Replay {
    pub fn trace(&self, mutations: &[Mutation]) -> Result<Trace>   // next to simulate (:1182)
}
```
Steps: `runs = self.ctx.trace_raw(mutations)?`; `keys = self.ctx.message_account_keys()`; roles from the header (port preflight.rs:381-416 into a `pub(crate) fn account_roles(tx: &VersionedTransaction, keys: &[String]) -> Vec<AccountRole>` in preflight.rs so both callers share it); for each top-level `k`: `meta = runs[k].result` (either arm) → `top = message.instructions()[k]`, `cpis = meta.inner_instructions[k]` (or `[]` when the prefix failed before it); build the pre-order node list `[top] ++ cpis` with path from a stack keyed on `stack_height`; decode each node with `ixname::enrich` (see gotcha 11); per-step logs: `logs_k = meta.logs`, `prev = runs[k-1].meta.logs.len()`; walk `logs_k[prev..]` with a small invoke/consumed/success/failed stack machine to assign `(start,end)`, `cu_consumed`, `success` to each node in order; per-step diffs: compare `runs[k].post` vs `runs[k-1].post` (k=0: vs `self.ctx.pre_account_owned` / absent) → `RawAccountDiff` → `decode_diffs(raw, self.ctx.idl_map())` (:1522); error: `explain_error(&replay_result_k, idls)` (:1435) on the first failing prefix; `result` = last run through `to_replay_result` + `explain`. `failed_step` = index of the node whose `Program X failed` line was last in the failing run (== the `InstructionError(i, _)` index for top level).

### 10.4 CPI tree from logs vs inner_instructions

Use `inner_instructions` for **structure** (exact program/accounts/data, `stack_height`) and logs for **CU/outcome**. Alignment rule: runtime emits exactly one `Program <id> invoke [d]` per node in pre-order, so zipping invoke lines with the pre-order node list is 1:1; sanity-check `id == node.program` and `d == node.depth` and fall back to a log-only tree (parse invoke/success/failed/consumed with a stack) on mismatch or truncation. Builtins (System, ComputeBudget, loaders) emit no `consumed` line → `cu_consumed = None` (or 150 for System/ComputeBudget per compute.rs:28-31).

### 10.5 `Trace::diff`

Compare by `path`: outcome flip (`success`), `error.raw`, `cu_consumed`, and `diffs` (normalise to `(address, field.name, before, after)` sets). Steps only present on one side (the other trace failed earlier) → `before: None` / `after: None`. This is what "edit and re-run" renders and what a trace-aware `minimize_mutations` can use (predicate `trace.failed_step != baseline.failed_step`).

### 10.6 CLI `debug` — `src/main.rs` (§7). Server `POST /trace`, `GET /trace/{sig}`, `GET /debug/{sig}` — `server/src/main.rs` (§8). UI — `static/index.html` + `web/vercel.json` (§9).

### 10.7 Gotchas

1. **ComputeBudget instructions must be in every prefix.** Limits are computed from the whole message before execution (`get_compute_budget_limits`, REG litesvm lib.rs:2062-2069 → `process_compute_budget_instructions`); a prefix without them runs at the default 200k/ix cap and can fail with `ComputeBudgetExceeded` where the real tx passed. Include all CB instructions in every prefix (order preserved); never duplicate them (duplicates are a `TransactionError`). Detect by program id `ComputeBudget111111111111111111111111111111` (src/preflight.rs:19, src/ixname.rs:14).
2. **Fee & rent are per prefix run.** Each prefix charges `fee = lamports_per_signature × sigs` to the payer, so `run0 vs pre-state` on the payer includes the fee; `run_k vs run_{k-1}` cancels it. `check_accounts_rent` runs at the end of every prefix (REG litesvm lib.rs:1451, :1466-1481): a prefix that leaves an account below rent-exempt minimum mid-transaction (create-then-fund patterns) fails even though the full tx passes. If `runs[k]` fails but `runs[n-1]` succeeds, flag the step as `prefix_artifact` rather than a real failure, and take the real failing step from the full run's `InstructionError` index.
3. **Precompiles read all instruction datas** (`process_precompile(program_id, data, message.instructions_iter().map(|ix| ix.data))`, REG litesvm src/message_processor.rs:35-40) and programs may read the Instructions sysvar; a prefix hides later instructions. Fine for the common "precompile ix precedes consumer ix" shape; note it in docs.
4. **ALT**: tables are loaded as data accounts (replay.rs:827-832); LiteSVM resolves them at sanitize using its accounts DB; `slot` must be ≥ the table's last extension (comment :335-339). For diffs, enumerate ALT-resolved keys locally (§10.2) so writable ALT accounts are diffed. `post_accounts` from `simulate_transaction` already covers them (it's indexed over the sanitized message's full key list, lib.rs:2049-2056).
5. **Accounts created by the tx** are absent from `loaded` (:579-582) — diffs must come from `post_accounts` (§0 #3) or `svm.get_account` over `message_account_keys()`, treating `None → Some` as created and `Some → None` as closed.
6. **Sysvars / time travel**: `fresh_svm` seeds Clock from `slot`/`block_time` + `TimeTravel` (:568-577); all prefixes share the same clock. `Replay::set_time_travel` (scope.rs:1139) and feature toggles (:1158-1165) apply unchanged. Store `describe_clock()` in the trace. `replay_at_slot` traces need `SVMSCOPE_ARCHIVE_URL` for exact state (server main.rs:659-663); free tier = `Fidelity::Reconstructed` — surface `fidelity` in the trace/UI so drift isn't mistaken for a bug (see `driftHint` static/index.html:2028-2041).
7. **Log truncation**: set `with_log_bytes_limit(None)` (§0 #6) or the "logs of run k start with logs of run k-1" assumption breaks; also guard against a `Log truncated` line.
8. **Determinism assumption**: prefix k's logs must equal prefix k-1's logs plus step k's. Assert `logs_k[..prev] == logs_{k-1}` in debug builds; on mismatch fall back to splitting `logs_k` at the k-th top-level `invoke [1]`.
9. **Failing index typed**: capture `TransactionError::InstructionError(i, e)` before `format!("{:?}")` (replay.rs:1386); the string form is `InstructionError(2, Custom(6001))`.
10. **Deserialize derives** (§0 #7) before touching `Fixture`.
11. **`ixname::enrich` needs RPC + `HashMap<String, Option<Value>>`**: for `Replay::trace` build the cache from `self.ctx.idl_map()` as `Some(v)` and pass `scope.client()`; for fixtures (no `Scope`) either refactor `enrich` to take `idl_for: &mut dyn FnMut(&str) -> Option<Value>` (both callers scope.rs:192 and preflight.rs:294 adapt trivially) or add `enrich_offline`. Offline decoding then covers every program whose IDL was captured (`preload_idls` captures all loaded programs + data-account owners, scope.rs:458-464).
12. **`run_full` is private** — trace_raw lives in replay.rs; `Replay` (scope.rs) only sees `pub(crate)` methods.
13. **Per-CPI CU from logs**: BPF programs emit one `consumed` line per invocation (nested included) just before their `success`/`failed`; attribute with the stack machine; a nested program's CU is *inclusive* of its own CPIs' CU (runtime semantics) — display as-is or subtract children for "self" CU.
14. **`return_data`** in `TransactionMetadata` is the last `set_return_data` of the whole prefix — attribute it to step k only if `return_data.program_id == step.program` (or a CPI under it).
15. **Cost**: `fresh_svm` re-runs `add_program` (ELF load/verify) for every program (:303-314); doing that n times is the slow path. One SVM + n `simulate_transaction` calls is ~n × execution cost only. Server: still gate with a cap on `n` (e.g. 64 instructions) alongside `MAX_MUTATIONS_PER_REQUEST` (server main.rs:260).
16. **Route clash**: `/debug/{sig}` serving HTML vs JSON — keep JSON on `/trace/…` (§8).
17. **Edit-at-step-k semantics (v1)**: mutating the *initial* state only reproduces "edit the before-state of step k" if steps `< k` don't write that field. Allow edits only on fields whose value is unchanged between pre-state and `runs[k-1].post`, or warn; true resume-at-k is Phase B.
18. **Phase B reality check**: the existing `invocation-inspect-callback` (REG litesvm lib.rs:1422-1449) wraps the whole message; top-level per-instruction hooks would go in litesvm's vendored `process_message` loop (src/message_processor.rs:28-51, ~5 lines) but add nothing over prefix replay. CPI-level state requires hooking `InvokeContext::process_instruction`/`native_invoke` in `solana-program-runtime 4.1.1` (agave), i.e. a fork of agave's runtime crate plus a litesvm feature to thread the callback — larger than "a small feature-gated patch to LiteSVM". Alternative cheaper Phase B: after each CPI the runtime records the `InstructionContext` in the trace (that's how `inner_instructions` are built, lib.rs:2043 `inner_instructions_list_from_instruction_trace`), but account snapshots are not kept; a patch that snapshots `transaction_context` accounts inside `InvokeContext::process_instruction`'s post path is the minimal agave change.
19. **`spec::MutationInput::Field.value` is `i64`** (src/spec.rs:83) while `Mutation::Field.value` is `i128` — fine for the wire, but u64 fields ≥ 2^63 can't be set via JSON today.
20. **UI globals**: `renderTree` writes into fixed ids (`#tree`, `#treeCount`) — clone it into `renderStepTree(container, steps)` rather than reusing directly.
