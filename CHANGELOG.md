# Changelog

All notable changes to svmscope are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/); versions follow
[Semantic Versioning](https://semver.org/).

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
