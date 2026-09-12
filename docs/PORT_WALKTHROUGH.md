# The svmscope crate, commit by commit

Read this next to `~/Desktop/litesvm/crates/svmscope/src`, in the order below.
Each section says what the module is for, the types and functions a reviewer
will look at, what moved from where in the restructuring, and the question a
reviewer is likely to ask with the answer.

The whole crate in one sentence: a `Scope` fetches a transaction and its world
from a node once, a `ReplayContext` holds that world, a `Replay` runs it in
LiteSVM as many times as you like with mutations, and `Trace`, `Analysis`,
`Diagnosis` and `Profile` are the different ways of looking at a run.

Data flows in one direction, top to bottom of this list. No module depends on
one below it.

---

## 1. `error.rs`, `utils.rs`

**error.rs.** One `Error` enum and a `Result` alias. The variants are the ways
the crate itself can fail: `Rpc`, `MalformedRpcResponse`, `NotFound`,
`InvalidAddress`, `MutationTargetMissing`, `UnknownField` (carries the list of
available field names so the message can say what you probably meant),
`FieldValueOutOfRange`, `InvalidSpec`, `Fixture`, `MissingPayer`. A reverting
transaction is never an `Error`; it is a successful observation with
`success == false`. That rule is stated in the module doc and it shapes every
API in the crate.

**utils.rs.** Three helpers: `resolve_account_keys` (static keys, then ALT
writable, then ALT readonly, the order the runtime uses), `camel_to_snake`
(Anchor method names to Rust names, for discriminators), `hex_decode`
(tolerant of `0x`, spaces, underscores; rejects non-ASCII before slicing so it
cannot panic on a multi-byte character).

**What moved.** `camel_to_snake` came from `program.rs` and `hex_decode` from
`spec.rs`. Both were used by modules lower than the one that defined them,
which created cycles. Now they sit at the bottom.

**Reviewer question.** "Why `#[non_exhaustive]` on `Error`?" So variants can be
added without a breaking release.

---

## 2. `idl_model.rs`, `idl_encode.rs`

**idl_model.rs.** Parses an IDL JSON into `IdlModel`: instructions (`IxDef`
with discriminator, args as `ArgDef`, accounts as a tree of `AccountNode`,
PDA seeds as `SeedDef`), account definitions with discriminators
(`AccountDef::matches`), type definitions (`TypeDef`, struct fields, enum
variants), errors (`ErrorDef`). It accepts three dialects: Anchor 0.29 (types
inline, camelCase), Anchor 0.30 (discriminators explicit, snake_case), and
shank. `IdlType::parse` is the recursive type grammar: primitives, `pubkey`,
`string`, `bytes`, `option`, `vec`, `array`, `defined`.

**idl_encode.rs.** The inverse of decoding: `encode_arguments` takes a method's
`ArgDef`s and a JSON object and produces the Borsh bytes, with typed errors
that name the argument and the expected type. Handles nested structs, enums
with payloads, options, vectors, fixed arrays, `u128`/`i128` from strings.
`int_bytes_u` / `int_bytes_i` are shared with field mutation so the same
integer encoding is used everywhere.

**Reviewer question.** "Why your own IDL parser instead of the Anchor crate?"
Because mainnet IDLs are messy: three dialects, hand-edited files, missing
fields. This parser is tolerant on purpose and takes the JSON `Value`, not a
typed struct, so an unknown field never fails the whole IDL.

---

## 3. `decode.rs`, `idl.rs`, `bundled_idls.rs`, `idls/`

**decode.rs.** `decode(owner, data)` picks a layout by owner: token account,
mint, ALT, stake, nonce. `decode_bytes` is the primitive reader. `infer_layout`
is the last resort: a structural guess (discriminator, pubkeys, u64s) for data
nobody has a layout for. `DecodedAccount` is a list of `Field`s with name,
type, offset, width and value; the offset and width are what let a mutation
set a field by name later.

**idl.rs.** Everything that reads an IDL at runtime. `decode_with_idl` matches
an account by discriminator and walks its type with a total visit budget
(`MAX_WALK_VISITS`), so a nested array of empty structs cannot spin a request
for millions of visits. `decode_ix_args`, `find_ix`, `disc_len`,
`ix_arg_span`, `encode_fixed`, `decode_event`, `error_for_code`. The four
pure parsers for on-chain IDL storage: `anchor_idl_address` and
`idl_from_anchor_account` (the legacy `anchor:idl` seed account with a
zlib payload), `program_metadata_idl_address` and
`idl_from_program_metadata` (the newer Program Metadata program).
`inflate_idl_json` caps the inflated size at 16 MiB against a decompression
bomb. `synthesize_from_elf` recovers instruction names from a stripped Anchor
binary: the dispatcher logs `Instruction: <Name>`, so the names are in rodata,
and a candidate is accepted only when `sha256("global:<snake>")` matches real
data.

**bundled_idls.rs.** Five IDLs compiled into the binary for programs that never
published one. `bundled_idl(program_id)`.

**What moved.** The RPC fetching that used to be in `idl.rs` and `decode.rs`
(`fetch_idl_json`, `describe_accounts`) moved to `rpc.rs`. The parsers stayed
and are now pure functions over bytes. That is why `idl.rs` no longer imports
`solana_client`.

**Reviewer question.** "600 KB of JSON in the repo?" They are the IDLs of
programs the crate would otherwise decode as raw bytes on mainnet. They are in
`include` so they ship with the crate, and they are data, not code.

---

## 4. `cpi_tree.rs`, `ixname.rs`, `compute.rs`, `diffs.rs`

**cpi_tree.rs.** `build_cpi_tree` turns the RPC transaction JSON (top-level
instructions plus `innerInstructions`) into a nested `Vec<CpiEntry>`: program,
name, args, accounts, depth, compute, `introspects`, `discriminator`.
`spans_from_logs` parses `Program X invoke [n]` / `success` / `failed` lines
into `LogSpan`s and survives truncated logs. `attach_compute` zips those spans
onto the tree so each entry gets its consumed CU. `mark_introspection` flags
entries handed the Instructions sysvar. `is_precompile` identifies Ed25519 and
secp256k1, which run outside the VM and produce no log span.

**ixname.rs.** `enrich_with(lookup, program, data, account_indexes, keys)`
names one instruction: native tables for system, token, Token-2022, ATA,
stake, vote, memo; otherwise the IDL from `lookup`. Anchor account groups are
flattened to leaves as `Group · Leaf`. `enrich_offline` does the same from an
already-loaded IDL map, which is what fixtures and the trace use.

**compute.rs.** `cu_from_logs`, `cu_per_program`: consumed CU per program from
the `consumed X of Y` log lines.

**diffs.rs.** `token_diffs` and `account_diffs` from pre/post balances in the
transaction metadata.

**What moved.** `spans_from_logs` and `LogSpan` came from `trace.rs`. Both the
tree and the profiler needed them, and having the tree depend on the trace
was backwards. `ixname::enrich` (the RPC-fetching variant) moved to `rpc.rs`;
`enrich_with` takes a closure so the caller decides where IDLs come from.

**Reviewer question.** "Why not `litesvm-cpi-tree`?" It builds a tree from a
LiteSVM `TransactionMetadata`. This builds one from RPC JSON, which has inner
instructions and log messages in a different shape. Unifying them is real
work and a follow-up.

---

## 5. `analyze.rs`

The consumer-facing shapes and nothing else: `Analysis` (signature, overview,
CPI tree, balance and token changes, compute, decoded accounts, optional
replay), `Overview`, `AccountOverview`, `ProgramInfo`, `SigInfo`,
`SimulationReport` with `Explanation`, `AccountDiff`, `FieldDiff`, and the
preflight shapes `PreflightOverview`, `PreflightIx`, `AccountRole`.
`build_overview` assembles the overview from the tree.

**What moved.** `ReplayResult` came from `replay.rs`. `PreflightOverview` and
friends came from `preflight.rs`. Both were types that lower modules (checks,
mutations, the trace) needed, and pulling them down here removed two cycles.

---

## 6. `mutation.rs`, `check.rs`, `invariant.rs`, `search.rs`

**mutation.rs** (new file). `Mutation` is the enum every what-if is expressed
in: `Lamports`, `Owner`, `Data`, `DataPatch`, `Field` (by IDL name, value as
JSON so big integers survive), `IxArg`, `IxData`, `IxDataReplace`, `SkipIx`,
`MoveIx`. The constructors are the public API (`Mutation::lamports(...)`).
Below it, the assertion vocabulary: `Expect` (success, revert, revert
containing, any), `CmpOp`, `StateCheck` (lamports, lamports delta, token
amount, token delta, u64 at offset, field, field delta, field unchanged),
`AccountAssert`. Plus the field readers `find_field`, `read_field_int`,
`field_bytes`, `read_u64_at`, which turn a `DecodedAccount` and a name into
bytes.

What is deliberately not here: `AccountAssert::eval`. Evaluating an assertion
needs the LiteSVM instance and the replay context, so that `impl` block lives
in `replay.rs`. The type is a leaf; the evaluation is not.

**check.rs.** The builder API: `Check::success()`, `Check::revert()`,
`Check::account("addr").lamports_delta(Cmp::ge(0)).build()`, `Scenario::new`
with `.mutate()` and `.check()`. `Cmp` is the public comparison value.
`AssertOutcome` and `ScenarioOutcome` are the results of running one.

**invariant.rs.** Named checks people reach for: `no_lamport_loss`,
`max_token_loss`, `authority_unchanged`, `monotonic`, `field_constant`.

**search.rs.** `search_threshold`: binary search over a value with a predicate,
returning the `Threshold` where the outcome flips.

**What moved.** All of `mutation.rs` came out of `replay.rs`. `check.rs`
imported `Mutation` from `replay.rs` while `replay.rs` imported `Check` from
`check.rs`. Splitting the vocabulary from the engine is what broke that cycle.

**Reviewer question.** "Why do delta checks read the mutated pre-state?"
Because a scenario that sets a balance to zero and then asserts "no loss" must
compare against zero, not against the original load. The old code compared
against the original and a loss invariant could pass on a transaction that
lost funds. The fix is in `replay.rs` (`run_full_with_pre`) and the test is
in the fixture suite.

---

## 7. `fixture.rs`, `fidelity.rs`

**fixture.rs.** The on-disk format. `Fixture` is version, signature, captured
slot and block time, the transaction bytes, `entries` (each a `Data` account
or a `Program` ELF), the IDL map, and `recorded`: the `OnchainRecord` of what
actually happened (success, error, compute, logs). `from_json` refuses a file
newer than `FIXTURE_VERSION` rather than misreading it.

**fidelity.rs** (new file). `Fidelity` is the replay's overall claim:
`Current` (state as of now), `Reconstructed { slot }`, `Exact { slot }`.
`Provenance` is per account: `Fixture`, `HistoricalArchive`,
`MetadataRewind`, `CurrentRpc`. `AccountProvenance` and `FidelityCertificate`
put those together with `summary()` for humans. `AccountState` is one
account's bytes with the slot they are from.

**What moved.** `OnchainRecord` came from `scope.rs` to `fixture.rs`, because
a fixture stores it and `replay.rs` reads it, and neither should depend on
the client. The fidelity types came from `scope.rs` because `reconstruct.rs`
needs them and must not depend on `scope.rs`.

---

## 8. `replay.rs`, `tests/fixtures/`

The engine. Read it in this order:

- `ReplayContext`: the world. Signature, transaction, `loaded` accounts and
  ELFs, slot, block time, `TimeTravel`, IDL map, feature toggles, `PreState`
  (pre-transaction balances rewound from metadata). `from_fixture` and
  `to_fixture` are the round trip. `add_idl`, `replace_program`,
  `set_time_travel`, `set_feature_toggles` mutate it.
- `fresh_svm_with(tracing)`: builds a `LiteSVM` from `loaded`: sigverify off,
  blockhash check off, unlimited log bytes, feature set rebuilt when toggles
  exist (`with_feature_set` then `with_builtins`, because the set alone does
  not rebuild the runtime environment), then every account and program set.
  `with_rent_sysvar` guarantees the Rent sysvar is in the world.
- `prepare(mutations, tracing) -> Prepared { svm, tx, resolved }`: fresh SVM,
  mutations applied (`apply_mutation`, `tx_for` for instruction-level ones),
  the transaction rewritten if instructions were skipped, moved or edited.
- `run_with_diff`, `run_and_read_account`, `replay_result_of`: one execution
  and what came out of it. `failed_instruction_index` extracts the failing
  index from a `TransactionError`.
- `trace_raw` / `trace_raw_prefixes`: for the trace, replay instruction
  prefixes `0..=k` on fresh SVMs and snapshot accounts after each, keeping
  ComputeBudget instructions in every prefix. `RawStepRun` is one such run.
- `run_suite` and the `impl AccountAssert { eval }` block: evaluate scenarios.
- `TimeTravel`, `FeatureToggle`, `LoadedInfo`, `RentParams`, `PreState`.

The two fixtures are a counter increment (success) and a vesting claim before
the cliff (revert). The tests at the bottom use them.

**What moved out.** Every function that took an `RpcClient` (`fetch_loaded`,
`fetch_transaction`, `build_context`, `build_context_at_slot`,
`resolve_alt_addresses`, `preflight_context`, the ELF cache) went to
`rpc.rs`. `Mutation` and the assertion types went to `mutation.rs`.
`ReplayResult` went to `analyze.rs`. What is left is pure: bytes in, LiteSVM
runs, results out. This file was 3,272 lines and is now 2,027.

**Not here.** svmscope's `single-run-trace` feature, which snapshots state
from one execution through a runtime hook. That needs a change to LiteSVM's
core, so the crate uses prefix replays only.

**Reviewer question.** "Prefix replays are O(n²) in instructions." Yes. For a
40-instruction transaction that is 40 runs of a local VM, well under a second.
The single-run version is the follow-up that needs the hook.

---

## 9. `spec.rs`

The JSON forms of everything in commit 6: `MutationInput`, `AssertInput`,
`ScenarioInput`, `FeatureInput`, `SuiteRequest`, each with an `into_*` that
validates and converts. Unknown expectation strings are an error, not a
vacuous pass. This is what a suite file and the HTTP body both are.

---

## 10. `trace.rs`, `report.rs`

**trace.rs.** The shapes of a stepped run. `Trace`: steps, failed step, tier
and tier note, state slot, drifted accounts, result. `Step`: path (`0`,
`0.1`, `0.1.2`), program, name, args, accounts, `state_known`, `diffs`,
`diffs_since`, `original_index`, `data_hex`, events, return data, error,
`introspects`. `StepDiff` and `StepAccountState` are before/after with
decoded fields. `Trace::diff(other)` aligns two traces by path and reports
where they diverge. `DriftedAccount` is an account that moved since the slot.

**report.rs.** `render_html`: a suite run as one self-contained HTML page.
`rust_regression_test`: a fixture-backed test as Rust source you can paste
into `tests/`.

**What moved out.** `spans_from_logs` and `LogSpan` to `cpi_tree.rs`.

---

## 11. `session.rs`, `submit.rs`, `wire_format_tests.rs`, `tests/offline_fixture.rs`

**session.rs** (new file). The `Replay` type and everything a user does with
one. It holds a `ReplayContext`, the `OnchainRecord`, the `TimeTravel` and
the `Fidelity`. Methods: `from_fixture`, `to_fixture`, `run`, `simulate`,
`trace`, `advance_*` and `warp_to_*`, `set_feature(s)`, `add_idl`,
`account_after`, `find_threshold`, `minimize_mutations`, `replace_program`,
`compare_patch`, `run_suite`, `verify`, `regression_test`, `fidelity`,
`certificate`, `recorded`. `Replayed` is a run's result with the diffs and
`into_report`. `PatchComparison` is two runs side by side.

The trace builder is the biggest piece: `trace` calls `trace_raw` for the
raw prefix runs, then `diffs_of`, `completion_ranks` (pairing log spans with
tree entries in completion order, skipping builtins), `PathBuilder` (paths for
nested CPIs), `account_state`, and `explain_error`, `runtime_error`,
`native_error`, `decode_diffs` to name what happened.

**submit.rs.** `CapturedTransaction`: a signature and its `Replay`.

**wire_format_tests.rs.** Pins the JSON shape of every public type, so the
hosted API and SDK do not drift.

**tests/offline_fixture.rs.** Integration tests: load both fixtures, run,
assert, mutate, check the error types, no network.

**What moved.** All of `Replay`, `Replayed`, `PatchComparison` and the trace
builder came out of `scope.rs`. None of it needs a client, and `scope.rs`
was 2,638 lines mixing the client with the session. The trace parity tests
(traced run equals plain run on verdict, error, compute, final balances)
came from `replay.rs` because they test `Replay`.

**Reviewer question.** "Why is `Replay` in `session.rs` and not `replay.rs`?"
`replay.rs` is the engine (a context and how to run it once). `session.rs` is
the user-facing object that owns a context and runs it many ways. Keeping
them apart keeps `replay.rs` free of the trace builder and the fidelity
bookkeeping.

---

## 12. `diagnose.rs`, `preflight.rs`

**diagnose.rs.** `diagnose_tx`: from a failed transaction's logs and error,
produce a `Diagnosis`: error name via `error_for_code` (IDL), the Anchor
framework table (`framework_error`), the native program table, the failing
instruction and its accounts, and `fix_hint`. `parse_anchor_error`,
`parse_instruction_error`, `parse_failed_program` are the log parsers.

**preflight.rs.** `build_overview(tx, alt, lookup, fee_payer_balance)`: every
instruction described in words (`describe` knows wraps, transfers, approvals,
closes), fee and priority fee (with the runtime default of 200k CU per
instruction capped at 1.4M when no limit was set), whether the payer can pay,
and warnings. `compute_breakdown` splits CU per program from logs.

**What changed.** `build_overview` used to take an `RpcClient` and fetch ALTs,
IDLs and the payer balance itself. Now it takes those as arguments and
`rpc::preflight_overview` does the fetching. Same behaviour, no client here.

---

## 13. `rpc.rs`

New file, and the one to be able to explain best. Every call to a node:

- `fetch_transaction`, `fetch_loaded` (accounts plus ELFs for every key, with
  the upgradeable loader's programdata resolved), `fetch_program_elf` with an
  ELF cache keyed by address and slot, `fetch_account_data`.
- `fetch_idl_json`: the anchor account, then the metadata program, then the
  bundled IDL, then synthesis from the ELF. `idl_lookup` wraps it in a
  memoising closure so an IDL is fetched at most once per analysis.
- `resolve_alt_addresses` for v0 messages.
- `build_context` (current state), `build_context_at_slot` (archive node,
  `archive_honors_slot` probes whether it really serves history),
  `preflight_context` (an unsigned transaction's world).
- `describe_accounts` (batch fetch and decode), `enrich` (name one
  instruction with fetched IDLs), `preflight_overview`.

**Why it exists.** Before, these lived across `replay.rs`, `idl.rs`,
`decode.rs`, `ixname.rs` and `preflight.rs`, so five modules depended on
`solana_client` and the graph had cycles through them. Now one module does.
A feature gate on the client would be a one-line change here.

**Reviewer question.** "Why is `solana-client` not optional?" Because with it
off, everything downstream of `Scope` is dead code and the crate is a fixture
player. That is not worth a feature until there are offline entry points.

---

## 14. `scope.rs`

`Scope` is the entry point. It holds an `RpcClient`, an optional archive
client, and caches (transaction JSON by signature, IDLs by program). Methods:
`analyze` (fetch, tree, names, diffs, accounts), `diagnose`, `replay`
(build a context, wrap in `Replay`, attach the recorded outcome),
`replay_at_slot` and `replay_at` (reconstruction with tier fallback),
`preflight` and `preflight_tx`, `capture`, `account`, `signatures`,
`decode_account`, `program_instructions`, `program_idl`, `account_data`,
`last_write_slot`, `send_and_capture` (submit, wait for confirmation, capture
the pre-state). `status_is_confirmed` and `parse_unsigned` are the helpers,
with their tests.

**What is left after the move.** 1,011 lines from 2,638. Everything here
touches the client or the caches. Everything that did not is in `session.rs`.

---

## 15. `reconstruct.rs`, `scan.rs`, `program.rs`

**reconstruct.rs.** `Reconstructor::reconstruct(address, before_slot)`: walk
the account's write history (`write_history` via `LedgerSource`, which is an
RPC ledger or an archive), replay each later write's transaction with the
account's reconstructed bytes injected, and read the bytes out again, within
a replay budget. `Recon` says what was recovered and how (`Reconstructed`
with provenance).

**scan.rs.** `scan_breaking_points(scope, signature, options)`: for each
account the transaction touches, drain it and see if the outcome changes.
`BreakingPoint` per account.

**program.rs.** `ProgramClient` and `MethodBuilder`: pick a method from an
IDL, set payer, signers, accounts (with PDA derivation from the IDL's seeds),
args as JSON, then `instruction()`, `transaction()`, or `send_and_capture()`.

---

## 16. `profile.rs`, `symbols/`

Behind `profiler`. `Replay::profile` runs with register tracing on and
collects every executed BPF instruction per program frame. `FrameProfile` is
one frame: program, instruction it ran, executed instructions, CU, functions
(`FunctionProfile`), syscalls, folded stacks, `syscall_estimate`. `Profile`
holds the frames plus `attach_names` (frames to instructions through log
spans), `attach_compute`, and the four ways to name functions:
`symbolize` (a `.debug` build of the same binary), `symbolize_from_build`
(another build matched by `Shape`, the normalised instruction stream),
`apply_exact` (bundled `ExactSymbols` when `stripped_sha256` of the on-chain
ELF matches), `symbolize_from_corpus` (bundled `CorpusEntry` shapes of
library code). `builtin_exact` and `builtin_corpus` load the two gzipped
files. `MIN_CORPUS_LEN` is the floor below which a shape is too short to
trust.

---

## 17. Docs, README, changelog, CI

Crate docs in `lib.rs` (the quickstart is a compiled doc example), the crate
README (also a doc test), the Additional Crates section in the root README,
one changelog line, the publish line in the justfile, and one CI step that
runs the crate's tests with the profiler on.

---

## The four questions to be ready for

1. **"Why one PR?"** Aursen asked for one, after two earlier ones. The modules
   are commits so it reads in order.
2. **"Why is the client not a feature?"** See commit 13. Tried, left the
   crate dead offline, not honest.
3. **"Why your own CPI tree and IDL parser?"** Different input (RPC JSON, messy
   mainnet IDLs). Unifying with `litesvm-cpi-tree` is a follow-up.
4. **"Who maintains this?"** You, and a co-maintainer on their side would be
   welcome. Say it plainly.
