# Changelog

All notable changes to svmscope are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/); versions follow
[Semantic Versioning](https://semver.org/).

## [0.4.0] — 2026-09-05

### Added

- **Replay at the transaction's own slot** — `Scope::replay_at_slot` (and
  `replay_at(input, slot)` for a slot you choose). Two fidelity tiers, both
  labelled on the result: *reconstructed* (free, default) loads current
  accounts, rewinds SOL and SPL-token balances to their recorded
  pre-transaction values and anchors the clock to the slot; *exact* (opt-in via
  `Scope::with_archive`) loads accounts and program ELFs at the true slot from
  an archive endpoint that actually honours the historical `slot` parameter.
  Exact loads fetch at S-1 and patch the recorded pre-balances. Every replay
  carries a `Fidelity` label so a drifted run is never mistaken for an exact
  one; `Replay::certificate` returns a per-account provenance report
  (`FidelityCertificate`, `AccountProvenance`).
- **Step debugger** — `Replay::trace(&[Mutation])` unrolls a transaction into
  pre-order `Step`s (top-level instructions and CPIs) with the decoded
  instruction, touched accounts with decoded fields and roles, compute, its
  slice of the logs, Anchor events (`DecodedEvent`), return data and, for
  top-level steps, the exact before → after diff of every changed account.
  The failing step is pinpointed with its error decoded. `Mutation::IxArg` /
  `Mutation::IxData` edit an instruction argument via IDL re-encode, so a value
  can be changed at any step and the trace re-run (`TraceDiff` compares the two
  runs step by step). CLI: `svmscope debug <signature> [--json] [--now]`.
- **Diagnose** — `Scope::diagnose` / `Diagnosis`: why a transaction failed and
  how to fix it, leading with the meaning rather than the raw code. Curated
  Anchor framework error map (exact names, no IDL needed), runtime and native
  System / Token / ATA error names, a swap-context heuristic for unnamed
  custom errors, codes shown in decimal and hex, and an honest
  "custom error N, no IDL" fallback.
- **Plain-English read-out** — a deterministic narrator (`Explanation`) that
  describes what a transaction did and where it can break. No model, no API.
- **Breaking-point scan** — `scan_breaking_points` / `ScanOptions` /
  `BreakingPoint`: auto-discover every numeric threshold, discrete break
  (closed or frozen accounts, authority changes, owner reassignment) and
  schema-driven field edit that flips the outcome.
- **Counterfactual search** — `Replay::find_threshold` (`Threshold`) finds the
  balance or field value at which the outcome flips; `minimize_mutations`
  finds the smallest set of changes that does.
- **Invariant templates** — `Invariant`: named security properties evaluated
  over replay state.
- **Patch Lab** — `Replay::replace_program` and `compare_patch`
  (`PatchComparison`) replay the same incident against the original and a
  patched program ELF.
- **Regression-test generation** — freeze an incident into a fixture plus a
  scenario suite (`Replay::to_fixture`, `run_suite`, `verify`) so it becomes a
  permanent, hermetic CI test.
- **Pre-flight** — `Scope::preflight` / `preflight_tx` / `preflight_overview`
  and `parse_unsigned_b64`: inspect an unsigned wallet message before signing,
  with per-program compute (`compute_breakdown`) and account roles
  (`AccountRole`, `PreflightIx`, `PreflightOverview`).
- **Instruction names** — the remaining Token instructions (Token-2022
  extensions, `GetAccountDataSize`, `InitializeImmutableOwner`) and Anchor
  `emit_cpi` rows named "Emit Event"; runtime `InstructionError` names.
- **Caches** — process-wide ELF cache keyed by (programdata, upgrade slot) and
  a 10-minute IDL cache; a 64-instruction cap on traces.
- **`Scope::add_idl`** — register a program's IDL for every API on the scope
  (`analyze`, `diagnose`, `preflight_overview`, by-signature replays and
  traces), so local or private deployments with no on-chain IDL are still
  fully named. `program_with_idl` registers its IDL automatically.
- **`field_unchanged` check** — `AccountCheck::field_unchanged(name)` and the
  JSON assert kind `"field_unchanged"` compare a named field byte-for-byte
  before and after, for every field type. `Invariant::authority_unchanged` and
  `Invariant::field_constant` are built on it.

### Changed

- **LiteSVM 0.16 / Agave 4.2** — `litesvm` bumped from 0.15.2 to 0.16 and
  `solana-client` from 4.1.1 to 4.2.1 to match. Build, clippy and the full test
  suite pass unchanged; a live mainnet replay reproduces on-chain compute.
- **`spec::AssertInput::value` is now `Option<i64>`** so the `field_unchanged`
  kind can omit it. Every other kind still requires it and errors with
  `assert kind "…" needs a "value"` when missing; existing JSON suites are
  unaffected.
- Install snippets now say `svmscope = "0.4"`.

### Fixed

- Enable LiteSVM's `precompiles` feature so transactions that invoke the
  ed25519 / secp256k1 signature-verify precompiles replay instead of failing
  with `InvalidProgramForExecution`.
- The archive check behind the *exact* fidelity tier probed the endpoint at the
  transaction's own slot, so a plain non-archival RPC passed whenever that slot
  was the current tip (a transaction from the latest finalized slot, or any
  localnet). It now probes ~10k slots earlier, where only a real archive can
  answer, so current state is never labelled `exact@slot`.
- `Invariant::authority_unchanged` rejected pubkey fields ("only integer/bool
  fields can be asserted") — the one kind of field an authority ever is.

### Server and web (not part of the crate)

- `POST /trace`, `GET /trace/{sig}` (6-hour in-memory store for share links),
  `GET /debug/{sig}`; the `/debug/<sig>` step-debugger UI with a Debugger tab,
  plain-English story with Simple / Expert modes, per-wallet net effects, state
  inspector, overrides panel, events, fund flow, watch panel with per-step
  timeline, run-to breakpoints and deep links; auto-diagnose card for failed
  transactions; "Scan for all breaking points"; the counterfactual box.

## [0.3.1] — 2026-08-29

Docs-only: the README/lib install snippets said `svmscope = "0.2"`, which
caret-resolves to 0.2.x and would never install 0.3. Corrected to `"0.3"`.
No code changes.

## [0.3.0] — 2026-08-29

### Added

- **Named-field mutations** — `Mutation::field(addr, "reserve_a", value)`, the
  mutation-side twin of the Check DSL's named-field asserts. Resolved through
  the account's decoded layout (SPL layouts or the owner program's IDL); an
  unknown field errors listing the real names, an ambiguous short name errors
  with candidates, and a value that doesn't fit the field's type is a hard
  `FieldValueOutOfRange`, never a silent truncation. Also available in JSON
  suites as `{"kind":"field","address":…,"field":…,"value":…}` (additive —
  existing suites are unaffected).
- **Wire-format snapshot tests** pinning the exact JSON shapes of every
  response type the web UI and TS SDK consume, so internal refactors cannot
  silently break the HTTP API contract.

### Changed

- **Internal: typed IDL model.** IDLs are parsed once into typed structs; the
  account decoder, Borsh argument encoder, instruction builder, lister, and
  error lookup all consume the model instead of walking `serde_json::Value`.
  No public API or wire-format change (guarded by the snapshot tests and the
  external exact-byte suite); unlocks Codama/Shank support cleanly.
- **The `server` cargo feature is removed** — the HTTP API server now lives in
  its own unpublished workspace crate (`server/`, `svmscope-server`), so the
  library has no features at all and consumers never see axum/tokio in any
  configuration. Run it with `cargo run -p svmscope-server`. The server's HTTP
  paths and JSON are unchanged; only the build command moved (Dockerfile/CI
  updated). If you depended on `features = ["server"]`, build the new crate
  from the repo instead.
- Declared `rust-version = "1.91"` (MSRV, verified against the full test suite).
- README: demo all four `Mutation` constructors; link the
  [svmscope_example](https://github.com/alizeeshan1234/svmscope_example)
  reference-consumer repo.

## [0.2.2] — 2026-08-28

README-only re-release so crates.io/docs.rs pick up the cleaned README
(badges, install/version fixes, License section, removal of hosting-specific
deployment details). No code changes from 0.2.1.

## [0.2.1] — 2026-08-28

### Fixed

- **`Check::matches_onchain()` used alone on a transaction whose recorded
  outcome was a revert** failed the scenario even when the replay faithfully
  reproduced the revert: a scenario with no explicit outcome check implicitly
  expects success, contradicting the faithful failure. A scenario carrying
  `matches_onchain` now suppresses the implicit-success default — the on-chain
  match *is* the outcome assertion. Guarded by a committed reverting fixture.
- **`advance_slots` / `advance_epochs` / `advance_seconds`** accumulated with
  non-saturating addition, so repeated extreme jumps on one `Replay` could
  overflow-panic in debug builds. Now saturating, matching the saturating
  clock application.

## [0.2.0] — 2026-08-28

First crates.io release.

### Added

- **Decode**: full CPI tree with instructions, arguments, and accounts named
  from on-chain Anchor IDLs or built-in native layouts; balance and token
  diffs; per-program compute units; failures explained by name
  (`SlippageToleranceExceeded`, not `Custom(6001)`).
- **Replay**: reconstruct a transaction's entire world (accounts, program
  ELFs, clock) and re-execute it locally in LiteSVM — no validator.
- **Mutate & time-travel**: `Mutation::{lamports, data, patch}`, feature-gate
  toggles, and coherent clock warps by slots/epochs/seconds or to an absolute
  point.
- **Check DSL**: mollusk-style declarative assertions on outcomes and state,
  including named account fields resolved through SPL layouts or IDLs.
- **Fixtures v2**: freeze transaction + accounts + ELFs + IDLs + the recorded
  on-chain outcome into one JSON file; replay offline, deterministically, in
  CI, with `Check::matches_onchain()` as the fidelity primitive.
- **Build & submit**: an IDL-driven `ProgramClient`/`MethodBuilder` that
  constructs an Anchor instruction (full Borsh argument encoding), signs,
  submits, waits for it to land, and returns a `CapturedTransaction` whose
  replay holds the exact pre-transaction world.
- CLI (`analyze`/`freeze`/`test`/`report`/`upgrade`), optional HTTP `server`
  feature, and three runnable examples.

### Security / hardening

Shipped after a five-area audit with regression tests for every finding:
typed errors on every malformed input (no reachable panics), bounded
recursion/allocation on untrusted on-chain IDLs (depth caps, element caps,
zlib decompression cap), corrected native account layouts (address lookup
table authority byte, SPL multisig, stake lockup), spoof-resistant compute
attribution, and a hard no-silent-pass guarantee: a typo'd mutation or assert
address is an error or failed check, never a vacuous pass.

[0.3.1]: https://crates.io/crates/svmscope/0.3.1
[0.3.0]: https://crates.io/crates/svmscope/0.3.0
[0.2.2]: https://crates.io/crates/svmscope/0.2.2
[0.2.1]: https://crates.io/crates/svmscope/0.2.1
[0.2.0]: https://crates.io/crates/svmscope/0.2.0
