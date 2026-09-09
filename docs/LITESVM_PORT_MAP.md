# Porting svmscope into the LiteSVM workspace

Working map for the single PR agreed with Aursen on 2026-09-08. Ali writes
the code; this document says what moves, what must change first, and in what
order the commits land so each one compiles, tests and reads on its own.

## Terms

- One PR against `LiteSVM/litesvm` master (currently `7dbb802`), one commit
  per module, readable top to bottom. Aursen: size is fine, quality is the gate.
- No features added to the `litesvm` core crate. Everything lives in one
  sibling crate under `crates/`.
- Consequence: the single-run step debugger stays out. It needs the
  `after_instruction` hook in `message_processor.rs` plus the vendored
  `solana-program-runtime`. The ported trace runs in prefix-replay mode, which
  the published svmscope crate already uses. The hook is a separate
  conversation with Aursen after this PR.
- Every commit message, the PR body and every review reply are written by
  Ali. No AI attribution anywhere.

## What moves and what stays

| Stays in svmscope (downstream app) | Moves into the workspace crate |
|---|---|
| `server/`, `web/`, `sdk/` | everything in `src/` below |
| `src/main.rs` CLI (see open question 1) | |
| `single-run-trace` feature, `vendor/`, `[patch.crates-io]` | |
| `report.rs` HTML reports, `submit.rs`, `search.rs`? (see table) | |

Engine modules, with line counts as of `fe75bc2`:

| Module | Lines | Role | Network |
|---|---|---|---|
| `error.rs` | 228 | error type + runtime error names | no |
| `utils.rs` | 50 | small helpers | no |
| `idl_model.rs` | 732 | IDL JSON model (Anchor 0.29/0.30, shank) | no |
| `idl.rs` | 963 | walk an IDL, decode accounts and args | 9 RPC sites (IDL and ELF fetch) |
| `idl_encode.rs` | 535 | encode args from JSON by IDL type | no |
| `decode.rs` | 597 | native account layouts, decode by owner | 3 RPC sites (IDL fetch) |
| `ixname.rs` | 596 | name instructions, accounts, args | 3 RPC sites |
| `cpi_tree.rs` | 252 | CPI tree from logs and inner ixs | no |
| `compute.rs` | 240 | CU per program frame | no |
| `diffs.rs` | 190 | balance and token diffs | no |
| `program.rs` | 518 | ProgramClient, MethodBuilder, PDA derivation | 3 RPC sites |
| `bundled_idls.rs` | 73 | IDLs shipped in the binary | no |
| `replay.rs` | 3272 | Mutation, TimeTravel, FeatureToggle, load + run | 36 RPC sites |
| `scope.rs` | 2638 | `Scope` façade, `Replay`, fidelity, OnchainRecord | 14 RPC sites |
| `reconstruct.rs` | 474 | state at the original slot, provenance | 3 RPC sites |
| `analyze.rs` | 215 | Analysis assembly | no |
| `preflight.rs` | 432 | unsigned tx overview | 2 RPC sites |
| `diagnose.rs` | 478 | failure explained | no |
| `trace.rs` | 499 | Step, Trace, TraceDiff, log spans | no |
| `fixture.rs` | 116 | Fixture JSON format | no |
| `check.rs` | 274 | Check, Scenario, assertions | no |
| `spec.rs` | 583 | Check DSL parser | no |
| `invariant.rs` | 133 | invariants over scenarios | no |
| `scan.rs` | 276 | drain-scan for breaking accounts | no |
| `search.rs` | 104 | threshold binary search | no |
| `profile.rs` | 1828 | compute profiler (register tracing) | no |
| `wire_format_tests.rs` | 495 | JSON shape tests | no |

Total moving: about 14.5k lines before untangling.

## Dependency cycles that block one-commit-per-module

The current module graph is not a DAG. These edges must be cut before the
commit sequence below can compile stepwise. Each is a small mechanical move.

| Edge | Item | Fix |
|---|---|---|
| `error -> replay` | `ReplayResult` | error naming takes the fields it needs (error string, logs) instead of the whole result; or move that fn to `analyze`. |
| `idl -> replay` | `fetch_program_elf` | move to `rpc.rs` (feature `rpc`); `idl` takes ELF bytes as input. |
| `idl -> program`, `idl_model -> program` | `camel_to_snake` | move to `utils.rs`. |
| `idl_encode -> spec` | `hex_decode` | move to `utils.rs`. |
| `replay <-> scope` | `OnchainRecord` | `OnchainRecord` is the recorded outcome; it belongs in `fixture.rs`. Then `replay -> fixture`, `scope -> fixture`, no cycle. |
| `replay <-> check` | `Mutation`, `AccountAssert`, `CmpOp`, `Expect`, `StateCheck` one way; `Check`, `CheckKind`, `Scenario` the other | new `mutation.rs` holds `Mutation` and the assert value types. `check` depends on it. `replay` depends on both. |
| `spec -> replay` | `Mutation` | resolved by `mutation.rs`. |
| `cpi_tree -> trace`, `profile -> trace` | `spans_from_logs` | log-span parsing belongs in `cpi_tree.rs`. `trace -> cpi_tree` only. |
| `analyze -> preflight` | `PreflightOverview` | move the type into `analyze.rs`; `preflight` builds it. |
| `reconstruct -> scope` | `Scope`, `AccountState` | `AccountState`, `Provenance`, `Fidelity`, `FidelityCertificate` move into `reconstruct.rs`. `reconstruct` takes `&RpcClient` and inputs; `Scope` calls it, not the reverse. |
| `replay` RPC fetchers (lines 61–256, 1302) | `fetch_*`, `archive_honors_slot` | move to `rpc.rs` behind feature `rpc`. `replay.rs` becomes pure: takes `LoadedAccounts` + ELFs. |
| `scope.rs` `Replay` struct (line 1079 onwards) | time travel, features, run, simulate, trace, verify | none of it needs RPC except `from_fixture` (offline) and `replace_program`. Move `Replay` + `Replayed` into `replay.rs` or a `session.rs`. `scope.rs` shrinks to the RPC façade and goes behind `rpc`. |

After these moves the crate has a clean layering:

```
utils, error
  └─ idl_model ─ idl ─ idl_encode ─ decode ─ ixname
  └─ cpi_tree ─ compute ─ diffs
  └─ mutation ─ check ─ spec ─ invariant
  └─ fixture (Fixture, OnchainRecord)
  └─ replay (load, run, Replay session)        needs everything above
  └─ trace, analyze, preflight, diagnose        pure, over replay output
  └─ scan, search                              over replay
  └─ profile                                   feature `profiler`
  └─ program (PDA, MethodBuilder)              partly rpc
  └─ rpc, reconstruct, scope                   feature `rpc`
```

## Crate shape

- Path `crates/svmscope`, name `svmscope`, version `0.16.0` via
  `version.workspace = true`, license/edition/repository from workspace.
- Features:
  - default: none. Pure decode, replay from fixtures, checks, trace.
  - `rpc`: `solana-client`, everything that touches mainnet
    (`rpc.rs`, `reconstruct.rs`, `scope.rs`, IDL/ELF fetch, `program.rs`
    send paths).
  - `profiler`: `litesvm/register-tracing`, `solana-program-runtime`,
    `solana-transaction-context`, `rustc-demangle`.
  - `cli` (only if the CLI comes along): `clap`, binary `scope`.
- Workspace deps already present and to be reused at workspace versions:
  `serde`, `serde_json`, `sha2`, `solana-account`, `solana-address`,
  `solana-clock`, `solana-instruction`, `solana-message`, `solana-signer`,
  `solana-transaction`, `solana-transaction-error`,
  `solana-program-runtime`, `solana-transaction-context`, `litesvm`,
  `litesvm-cpi-tree`, `base64` (workspace has 0.22, svmscope uses 0.23:
  downgrade), `hex`.
- New workspace deps this crate adds: `bincode 1.3.3`, `bs58`, `flate2`,
  `thiserror 2`, `solana-blake3-hasher`, `solana-reward-info`,
  `solana-pubkey`, `solana-client` (rpc only), `rustc-demangle` (profiler
  only). Expect Aursen to ask about each one. `bincode` and `flate2` are
  worth a sentence in the PR body.
- Drop svmscope's exact `=` pins. The workspace uses ranges.
- MSRV: workspace declares 1.89, svmscope needs 1.93. Run
  `cargo +1.89.0 check -p svmscope (the crate)` early. If it fails, find the
  construct and either rewrite it or raise it with Aursen before opening.
- `litesvm-cpi-tree` already exists in the workspace. Decide per module
  whether `cpi_tree.rs` is replaced by it or wraps it. Using the workspace
  crate is the better story; a second CPI tree in the same workspace will be
  the first review comment.

## Commit order

Each commit: compiles alone, `cargo test -p svmscope (the crate)` green offline,
`cargo clippy --all-targets -- -D warnings` clean, `cargo +nightly fmt`
clean, docs on every `pub` item.

| # | Commit | Modules | Tests it must carry |
|---|---|---|---|
| 1 | crate skeleton + error type | `Cargo.toml`, `lib.rs`, `error.rs`, `utils.rs` | error-name table round trips |
| 2 | IDL model and walker | `idl_model.rs`, `idl.rs` (no fetch) | parse Anchor 0.29 + 0.30 + shank IDLs from `tests/idls/`; walk budget test |
| 3 | IDL encoding | `idl_encode.rs` | encode/decode round trip per type; big-int strings |
| 4 | account decoding | `decode.rs`, `bundled_idls.rs` | SPL token, mint, ALT, stake, nonce layouts; IDL account by discriminator |
| 5 | instruction naming | `ixname.rs` | native programs table; Anchor group flattening |
| 6 | CPI tree, compute, diffs | `cpi_tree.rs`, `compute.rs`, `diffs.rs` | nested spans, truncation, precompile skip, CU attribution |
| 7 | mutations and checks | `mutation.rs`, `check.rs`, `spec.rs`, `invariant.rs` | DSL parser cases; delta/unchanged vs mutated pre-state |
| 8 | fixtures | `fixture.rs` | version gate; load `tests/fixtures/*.json` |
| 9 | replay engine | `replay.rs` (pure) | fixture replays reproduce recorded outcome; time travel; feature toggles; mutation application |
| 10 | trace | `trace.rs` | prefix-replay trace over a fixture; step diffs; TraceDiff |
| 11 | analysis, preflight, diagnose | `analyze.rs`, `preflight.rs`, `diagnose.rs` | `wire_format_tests.rs` moves here |
| 12 | scan and search | `scan.rs`, `search.rs` | threshold search on a fixture |
| 13 | profiler (`profiler` feature) | `profile.rs` | frame attribution on a fixture; syscall estimate |
| 14 | program client | `program.rs` | PDA derivation; MethodBuilder encoding (offline) |
| 15 | mainnet (`rpc` feature) | `rpc.rs`, `reconstruct.rs`, `scope.rs` | `#[ignore]` network tests behind `SVMSCOPE_RPC_URL`; unit tests for provenance and fidelity labels |
| 16 | CLI (`cli` feature, optional) | `src/bin/scope.rs` | argument parsing only |
| 17 | docs | README "Additional Crates", `CHANGELOG.md` Unreleased, `justfile` publish line | |

No network in CI. Anything that needs mainnet is `#[ignore]` and reads its
URL from an env var.

## The existing `scope-crate` branch

`~/Desktop/litesvm` branch `scope-crate`, commit `e346634`, 3,815 lines,
written by Claude on 09-05 for the closed PR #419. It is the same material
as commits 1–6 above, arranged differently:

| Branch file | Corresponds to |
|---|---|
| `errors.rs` (762) | `error.rs` + the error-name half of `diagnose.rs` |
| `idl_model.rs` (578), `idl.rs` (588) | commit 2 |
| `layout.rs` (332) | `decode.rs` native layouts |
| `native.rs` (318) | `ixname.rs` native tables |
| `tree.rs` (329) | `cpi_tree.rs` + `ixname.rs` on cpi-tree frames |
| `ext.rs` (93) | `ScopeExt` trait on `TransactionMetadata` (new, keep the idea) |
| `tests/decode.rs` (468) | offline decode tests |

Decision for Ali: read it and adopt it as the base for commits 1–6, or
rewrite from svmscope's `src/`. Either is fine. What is not fine is opening
a PR containing code you cannot walk a reviewer through line by line.

## Quality gate checklist

Aursen's only condition. Before opening:

- [ ] `cargo test -p svmscope (the crate)` and with `--all-features` green, offline
- [ ] `cargo clippy --all-targets --all-features -- -D warnings` clean
- [ ] `cargo +nightly fmt --all -- --check` clean (CI uses nightly fmt)
- [ ] `cargo +1.89.0 check -p svmscope (the crate)` or MSRV raised with Aursen
- [ ] no `unwrap`/`expect` on external input in library code
- [ ] every `pub` item documented; crate-level docs with one runnable example
- [ ] `litesvm-cpi-tree` reused rather than duplicated
- [ ] no `=` version pins; no deps that the workspace does not need
- [ ] CHANGELOG entry, README section, justfile publish line
- [ ] PR body in Ali's words: what, why one crate, feature map, follow-ups
      (hook, single-run trace), how to review commit by commit
- [ ] Aursen told the PR is coming and roughly when

## Open questions for Ali

1. CLI in or out. You told Aursen "and the CLI". The workspace has no
   binaries today. Feature-gated `src/bin/scope.rs` is the least intrusive.
2. Profiler in this PR or as the first follow-up. It is 1.8k lines and the
   only module that pulls in `rustc-demangle`.
3. `search.rs` and `scan.rs`: engine or app? They are small and pure, so the
   table above includes them. Say if you want them kept in svmscope.

## Status, 2026-09-08 evening: the branch exists

Built by Claude at Ali's instruction ("1"), on `~/Desktop/litesvm` branch
`svmscope-port` from upstream `7dbb802`. Nothing pushed, no PR opened.

**What changed against the plan above.**

- The `rpc` feature was built and then dropped. Behind the gate, almost the
  entire engine went dead: analysis, diagnosis, preflight, the CPI tree and
  IDL encoding are only reachable through the node-backed `Scope`, and the
  offline entry points all start from a fixture that `Scope` produced. A
  feature that leaves two thirds of the crate unreachable is dishonest.
  `solana-client` is a plain dependency; all node access still lives in one
  module, `rpc.rs`, so a gate is a one-line follow-up once offline
  entry points exist.
- MSRV: the workspace declares 1.89, but the `litesvm` core crate itself
  fails on 1.89 (`solana-syscalls` 4.2.1 uses unstable `MaybeUninit`
  helpers). svmscope (the crate) builds on 1.93. Not our problem to solve, but
  say it in the PR.
- `litesvm-cpi-tree` is not reused yet. `cpi_tree.rs` builds the tree from
  the RPC transaction JSON, which the workspace crate does not take as input.
  Expect the question; the honest answer is "follow-up".
- `ReplayResult` moved to `analyze.rs`, the `Replay` session type and its
  helpers moved out of `scope.rs` into `session.rs`, and
  `AccountAssert::eval` (needs the SVM) lives in `replay.rs` while the
  assert vocabulary lives in `mutation.rs`. The trace-versus-run parity tests
  moved from `replay.rs` to `session.rs` with the type they exercise. Every
  cycle in the table above is cut; the module graph is a DAG (checked by
  script, doc links excluded).

**The commits, in order.** Each one compiles with `--all-features
--all-targets` and its unit and integration tests pass on its own. Doc
examples describe the finished API (the checks module's example imports
`Scope`), so they run on the final commit only. Dead-code warnings appear in
intermediate commits for items whose only caller lands later; the final
tree is warning-free under `clippy -D warnings`.

1. crate skeleton, `error.rs`, `utils.rs`, workspace deps, `Cargo.lock`
2. `idl_model.rs`, `idl_encode.rs`
3. `decode.rs`, `idl.rs`, `bundled_idls.rs`, `idls/`
4. `cpi_tree.rs`, `ixname.rs`, `compute.rs`, `diffs.rs`
5. `analyze.rs`
6. `mutation.rs`, `check.rs`, `invariant.rs`, `search.rs`
7. `fixture.rs`, `fidelity.rs`
8. `replay.rs`, `tests/fixtures/`
9. `spec.rs`
10. `trace.rs`, `report.rs`
11. `session.rs`, `submit.rs`, `wire_format_tests.rs`, `tests/offline_fixture.rs`
12. `diagnose.rs`, `preflight.rs`
13. `rpc.rs`
14. `scope.rs`
15. `reconstruct.rs`, `scan.rs`, `program.rs`
16. `profile.rs`, `symbols/` (feature `profiler`)
17. crate docs, crate README, root README section, CHANGELOG, justfile, CI step

**Checks that pass on the final tree.** `cargo test -p svmscope (the crate)` with
`--all-features` (134 unit, 11 fixture, 7 doc) and with
`--no-default-features` (124 unit); `clippy --all-targets -D warnings` for
both; `RUSTDOCFLAGS=-D warnings cargo doc`; `cargo +nightly fmt --check`;
the workspace-root `cargo test --features precompiles --no-run` and root
`clippy -D warnings` exactly as CI runs them; `cargo +1.93.0 check`.

**Left out on purpose.** The CLI (`main.rs`), the server, web and SDK, the
`single-run-trace` feature with its fork and vendored runtime, the
`send_and_capture_localnet` test (needs a validator), and the examples.

**What Ali does now.**

1. Read the branch. `git log --oneline origin/master..svmscope-port`, then
   each commit with `git show`. You must be able to explain every module.
2. Rewrite the commit messages in your own words (`git rebase -i` over
   17 commits, `reword`). They are plain and factual now, with no AI
   attribution, but they are not your voice.
3. Decide on the open questions above (CLI, profiler, scan/search). The
   branch includes profiler, scan and search, and excludes the CLI.
4. Push to the fork, open the PR against `LiteSVM/litesvm` master with a
   body you wrote. Tell Aursen it is up.
