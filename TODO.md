# svmscope — Next build plan

> **Status:** #1 (IDL decoding), #2 (delta assertions AND named-field
> `assert pool.reserveA` — both shipped), and #3 (HTML reports) have shipped.
> #4 (agent/MCP packaging) is the remaining item.

Four features, validated by what Solana devs are actually doing on X (Aug 2026):
fast LiteSVM simulation on real account state, security auditors writing LiteSVM
tests, the Foundation's `solana-dev-skill` teaching LiteSVM/Mollusk testing, and
DeFi keeper teams testing against forked mainnet state. The recurring gap: the
interesting program accounts (AMM pools, perp markets, vaults) are **raw bytes**.

Build in this order — each depends on the previous. **#1 (IDL decoding) unblocks
the rest**, so start there.

---

## Current state (where things are)

- `src/decode.rs` — decodes SPL Token accounts & mints (incl. Token-2022 exts) into
  named `Field`s. `AccountInfo { address, owner, lamports, executable, data_len, decoded }`.
- `src/replay.rs` — `ReplayContext` (fetch state once), `Mutation` (Lamports/Data/DataPatch),
  `Expect`, `ScenarioSpec`, `StateCheck { Lamports, U64At }`, `AccountAssert`, `run_suite`.
  Fixtures: `to_fixture`/`from_fixture`.
- `src/fixture.rs` — `Fixture` (hermetic snapshot: tx + accounts + program ELFs).
- `src/api.rs` — `MutationInput`, `ScenarioInput`, `AssertInput`, `SuiteRequest`.
- `src/lib.rs` — `analyze`, `simulate`, `simulate_suite`, `capture_fixture`, `run_fixture_suite`.
- `src/main.rs` — CLI: `<sig>`, `--mutate`, `freeze`, `test`.
- `src/bin/server.rs` — `/analyze/:sig`, `/simulate`, `/simulate_suite`, `/freeze/:sig`.
- `static/index.html` — UI: explorer + Scenario tests + Advanced field editor.

Reminder: replay is against **current** state → drift. Fixtures fix determinism.
Best demo shape: Pump AMM swap; draining a pool reserve (token amount @64 → 0) reverts.

---

## 1. IDL-aware account decoding  ⭐ do first

**Goal:** turn raw program accounts (Anchor programs) into named, typed, editable
fields — so mutations and assertions read as `pool.reserveA` not `u64@64`.

**Approach**
- New `src/idl.rs`:
  - **Fetch on-chain IDL.** Anchor stores it at
    `create_with_seed(find_program_address(&[], program_id).0, "anchor:idl", program_id)`.
    Account data = 8-byte discriminator + `authority: Pubkey(32)` + `data: Vec<u8>`
    (borsh: u32 len + bytes). `data` is **zlib-compressed JSON**. Inflate → IDL JSON.
    - Add dep: `flate2` (zlib inflate). JSON via existing `serde_json`.
  - **Fallbacks** (many programs, e.g. Pump AMM, publish no on-chain IDL):
    1. local IDL file: `--idl <program>=<path.json>` (CLI) / upload endpoint (UI).
    2. optional registry fetch later (apr.dev / solscan) — stretch, not MVP.
  - **Match account → IDL account type** by the leading 8-byte discriminator
    (`sha256("account:<Name>")[..8]`), then decode fields by walking the IDL type.
  - **Borsh field walker** driven by IDL types: u8/16/32/64/128, i*, bool, pubkey,
    `[u8;N]`, defined structs, simple enums, `Option`. Emit `Field { name, offset,
    type, size, value, editable }` — same shape decode.rs already produces.
    - **GOTCHA (offsets):** borsh offsets are stable only up to the first
      variable-length field (Vec/String/Option-with-var). Compute a running offset;
      once you hit a var-length field, stop assigning stable offsets. Mark only
      fixed-offset scalar fields `editable: true` (that's the common case: reserves,
      amounts, bumps, flags). Everything after a var field is display-only.
- Hook into `src/decode.rs`:
  - After the SPL checks, if `owner` has an IDL (fetched/cached), try IDL decode.
  - Thread an IDL cache through `describe_accounts` (fetch each distinct owner's IDL
    once per analysis; in-memory `HashMap<program, Option<Idl>>`).
- `src/lib.rs` — `analyze` already calls `describe_accounts`; pass the client so idl.rs
  can fetch. Consider `AccountInfo.decoded.source = "spl" | "idl:<Name>" | null` so the
  UI can badge where the decode came from.
- UI (`static/index.html`): the field editor already renders `decoded.fields`, so
  IDL-decoded accounts light up automatically. Add: a small "decoded via IDL (Name)"
  badge, and an **"Upload IDL"** affordance for programs with no on-chain IDL.

**Gotchas**
- Non-Anchor programs (native, Pinocchio) have no IDL — leave as raw-byte fallback.
- Token-2022 / SPL already handled — don't double-decode; SPL path wins first.
- Enums / nested defined types can get deep; MVP: handle one level of `defined`
  structs + scalar leaves; skip exotic types gracefully (show as raw region).

**Acceptance**
- Analyze a tx touching an Anchor program with a published IDL → its account shows
  named fields with correct current values; a fixed-offset numeric field is editable
  and a mutation on it changes replay behavior.
- Program with no IDL → still shows raw-byte fallback (no regression).

---

## 2. Higher-level assertions

**Goal:** assert what keepers/auditors actually check — deltas and named fields —
beyond `u64@offset`.

**Approach**
- `src/replay.rs` — extend `StateCheck`:
  - `LamportsDelta { op, value }` and `TokenAmountDelta { op, value }` — compare
    **pre** (from `ctx.loaded`, the frozen pre-state) vs **post** (post-replay svm).
    `run_suite`/`run_full` already hold both sides; thread pre-state into assert eval.
  - `Field { name, op, value }` — resolve field → offset+type via the account's IDL
    decode (depends on #1), then compare. Lets you write `assert pool.reserveA == N`.
- `src/api.rs` — `AssertInput.kind` gains `"lamports_delta"`, `"token_delta"`,
  `"token_amount"`, `"field"` (+ `field` name). Keep `u64`/`lamports` working.
- UI: a per-scenario **assertion builder** in the Scenario tests panel — pick account
  → pick field (from decoded fields) or delta → op → value. Writes into the exported
  suite's `asserts`.

**Gotchas**
- Token amount = the SPL `amount` u64 @ offset 64; reuse, don't re-derive.
- Deltas need the account present in the fixture pre-state; if missing, fail the
  assert with a clear message (don't silently pass).

**Acceptance**
- A scenario asserting `token_delta` on a pool reserve passes on baseline and fails
  when a mutation makes the delta wrong.
- `field`-based assertion works on an IDL-decoded account.

---

## 3. Shareable audit / report output

**Goal:** a self-contained HTML report an auditor attaches to a finding — "here's
what happens under each edge case."

**Approach**
- `src/report.rs` — render `Vec<ScenarioOutcome>` (+ the tx overview) to a single
  self-contained HTML string (inline CSS, reuse the app's dark styling / verdict
  cards / pass-fail badges). Include: tx summary, per-scenario mutation + expected +
  actual + assertions + collapsible logs, and a pass/fail header (`N/M`).
- CLI: `svmscope report <suite.json> -o report.html` — runs the suite (fixture pref),
  renders, writes the file. Markdown variant (`-o report.md`) is a nice-to-have.
- UI: an **"Export report (.html)"** button next to Export suite — downloads the
  rendered report for the last run.

**Gotchas**
- Keep it fully self-contained (no external assets) so it's email/attach-friendly.
- Don't embed the whole 17MB fixture — just results + the suite spec.

**Acceptance**
- `svmscope report suite.json -o report.html` produces a standalone HTML that opens
  offline and shows each scenario's outcome + assertions + logs.

---

## 4. Agent / MCP packaging

**Goal:** make svmscope drivable by an agent so it rides the `solana-dev-skill` wave —
"freeze this tx, assert these scenarios."

**Approach**
- MVP (low effort, high value): tighten the **CLI as the agent surface** —
  - stable JSON output modes: `analyze --json` (exists), add `test --json` and
    `freeze` already emits JSON. Document exit codes.
  - a short `AGENTS.md` / skill doc: the exact commands an agent runs, in order.
- Stretch: a real **MCP server** (`src/bin/mcp.rs`) over stdio exposing tools:
  `analyze(sig)`, `freeze(sig) -> fixture`, `simulate(sig, mutations)`,
  `run_suite(fixture|sig, scenarios)`. Reuse the lib functions directly. Pick a Rust
  MCP SDK or hand-roll JSON-RPC over stdio.

**Gotchas**
- Keep tool outputs compact + typed (agents choke on huge blobs) — summarize logs,
  don't dump 17MB fixtures into a tool result; return a path instead.

**Acceptance**
- An agent can, from docs alone: analyze a sig, freeze a fixture, run a scenario
  suite, and read pass/fail — without screenshots or human help.

---

## Suggested morning order

1. `flate2` dep + `src/idl.rs` fetch+inflate → parse IDL JSON (print it for one program).
2. Borsh field walker → named `Field`s with fixed offsets; hook into `decode.rs`.
3. Verify in UI: an Anchor account shows named editable fields.
4. Then #2 (field/delta assertions) → #3 (report) → #4 (agent docs, then MCP).

Run the app while iterating: `cargo run --bin server` → http://127.0.0.1:3000.
Good test program to start with: pick a tx touching a well-known Anchor program
that publishes its IDL on-chain (verify the IDL account exists first).
