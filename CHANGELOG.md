# Changelog

All notable changes to svmscope are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/); versions follow
[Semantic Versioning](https://semver.org/).

## [Unreleased]

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

[Unreleased]: https://github.com/alizeeshan1234/svmScope/compare/v0.2.2...HEAD
[0.2.2]: https://crates.io/crates/svmscope/0.2.2
[0.2.1]: https://crates.io/crates/svmscope/0.2.1
[0.2.0]: https://crates.io/crates/svmscope/0.2.0
