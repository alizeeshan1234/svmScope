# svmscope

**A transaction simulator for Solana — decode any transaction, replay it locally, and change reality to see what happens.**

`svmscope` takes a mainnet transaction and:

1. **Decodes** it into a readable report — the full cross-program call tree with every
   instruction named and its arguments and accounts decoded (from the on-chain IDL or
   known native layouts), who gained or lost SOL, and compute units per program (with
   Address Lookup Table resolution).
2. **Replays** it locally in an embedded SVM ([LiteSVM](https://github.com/LiteSVM/litesvm)) —
   re-executing the real programs against the transaction's reconstructed state.
3. **Mutates** — change an account's balance or data, then replay again, to ask
   *"what if this had been different?"*

It's a decoder, a debugger, and a what-if lab in one command-line tool.

## Decode

```
$ cargo run -- <SIGNATURE>

#0  ComputeBudget111111111111111111111111111111
#1  ComputeBudget111111111111111111111111111111
#2  ComputeBudget111111111111111111111111111111
#3  JUP6LkbZbjS1jKKwapdHNy74zcZ3tLUZoi5QNyVTaV4
└─ [2] BiSoNHVpsVZW2F7rx2eQ59yQwKxzU5NvBcmKshCSUypi
    └─ [3] TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA
└─ [2] DRVSpZ2YUYYKgZP8XtLhAGtT1zYSCKzeHfb4DgRnrgqD
    └─ [3] TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA
└─ [2] FLUX6xBayGxLX9UcimVRxXFMHH6q43mAbRvDzSpCsvfK
    └─ [3] TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA

-- account balance changes --
capsFrvPsaCQ6y7RUk3TYkB5D1oNCNjRb4uYuB5UCsZ  -5451 lamports

-- compute units per program --
BiSoNHVpsVZW2F7rx2eQ59yQwKxzU5NvBcmKshCSUypi 98926 CU
DRVSpZ2YUYYKgZP8XtLhAGtT1zYSCKzeHfb4DgRnrgqD 29612 CU
JUP6LkbZbjS1jKKwapdHNy74zcZ3tLUZoi5QNyVTaV4  178113 CU
```

## Replay

`svmscope` then reconstructs the world the transaction ran in — every account it
touched *and* every program it invoked (following the upgradeable-loader programdata
pointer for the ELF) — loads it all into an in-process SVM, and re-executes the
transaction:

```
-- replay --
loaded 22 data accounts and 6 programs into the SVM
REPLAY: failed ❌  error: InstructionError(4, Custom(6024))
  Program JUP6LkbZbjS1jKKwapdHNy74zcZ3tLUZoi5QNyVTaV4 invoke [1]
  Program log: Instruction: RouteV2
  Program JUP6... failed: custom program error: 0x1788
```

That's the *real* Jupiter program executing locally. Swaps frequently fail on replay
with a slippage error — not a bug, but the honest consequence of **state drift**: the
current pool prices differ from the transaction's original slot. Exact replay of
older transactions needs archival state (see the roadmap); recent/simple transactions
replay cleanly.

## Mutate — the "what-if" engine

Pass `--mutate <address>:<lamports>` to change an account before replaying, then compare:

```
$ cargo run -- <SIGNATURE> --mutate <ACCOUNT>:0

REPLAY: failed ❌  error: InstructionError(4, Custom(6024))
MUTATED REPLAY: failed ❌  error: AccountNotFound
```

Here zeroing an account's balance changed the outcome — because an account with zero
lamports is treated as non-existent. The engine also supports **data mutations**
(rewriting an account's bytes — e.g. a token balance or an oracle price) via the
`Mutation::Data` API, which is how you'd flip a failing swap into a succeeding one by
rewinding a pool's reserves.

## Usage

```bash
cargo build

# decode + replay
cargo run -- <SIGNATURE>

# decode + replay + a what-if mutation (multiple allowed)
cargo run -- <SIGNATURE> --mutate <ADDRESS>:<LAMPORTS>

# web UI (decode + replay + scenario tests)
cargo run --bin server        # → http://127.0.0.1:3000
```

Uses the public `https://api.mainnet-beta.solana.com` endpoint, which only retains
recent transactions. For older transactions, point it at an archival RPC.

## Scenario testing — the hermetic part

The point isn't just "what if?", it's **coverage**: assert what *should* happen in
each edge case, with no test harness to write. svmscope replays the real programs
against edited state and checks both the transaction outcome and the resulting
account state.

The catch with replaying live transactions is **state drift** — current on-chain
state moves, so a replay that passes today can fail tomorrow for reasons unrelated
to your code. A test that flakes on mainnet state is worse than no test. So freeze
a **fixture** first: a self-contained snapshot (transaction + every account + every
program ELF) that replays identically forever, offline.

```bash
# 1. Freeze the world the transaction ran in → one portable file
cargo run -- freeze <SIGNATURE> -o fixture.json

# 2. Write a suite next to it (see below), then run it — deterministic, no RPC
cargo run -- test suite.json      # exits non-zero if any scenario fails → CI-ready
```

A `suite.json` references the fixture and lists scenarios. Each scenario mutates
accounts, asserts a transaction outcome (`success` / `revert` [+ `contains`]), and
optionally asserts on **resulting state** after replay:

```json
{
  "fixture": "fixture.json",
  "scenarios": [
    {
      "name": "baseline swap succeeds and moves the reserve",
      "expect": "success",
      "asserts": [
        { "address": "<POOL_RESERVE>", "kind": "u64", "offset": 64, "op": "!=", "value": 894205215695071 }
      ]
    },
    {
      "name": "draining the pool reserve reverts",
      "expect": "revert",
      "mutations": [
        { "kind": "data", "address": "<POOL_RESERVE>", "offset": 64, "bytes_hex": "0000000000000000" }
      ],
      "asserts": [
        { "address": "<POOL_RESERVE>", "kind": "u64", "offset": 64, "op": "==", "value": 0 }
      ]
    }
  ]
}
```

```
$ cargo run -- test suite.json
svmscope test — fixture 4RHX…SoJWt (16 accounts, 5 programs, 2 scenarios) [deterministic, offline]

  PASS  baseline swap succeeds and moves the reserve  (expect: succeeds; got: succeeded)
          ✓ assert Cb3N…zXYo u64@64 != 894205215695071
  PASS  draining the pool reserve reverts  (expect: reverts; got: reverted (Custom(6004)))
          ✓ assert Cb3N…zXYo u64@64 == 0

2/2 passed
```

The web UI (`--bin server`) starts you with a suite auto-generated from the
transaction; **❄ Freeze fixture** downloads the fixture and **⤓ Export suite**
writes the matching `suite.json`.

## How it works

- **Decode** — walks `getTransaction`: the CPI tree from `innerInstructions` +
  `stackHeight`, balance diffs from `pre/postBalances`, compute from the logs, and
  Address Lookup Table resolution (static keys + `meta.loadedAddresses`).
- **State** — `getMultipleAccounts` for every account; for programs, it follows the
  upgradeable loader's programdata pointer to the ELF.
- **Replay** — loads accounts + programs into LiteSVM (sigverify/blockhash checks off,
  since the original blockhash isn't valid in a fresh SVM), reconstructs the signed
  `VersionedTransaction`, loads the referenced lookup tables, and executes.
- **Mutate** — `get_account` → change lamports/data → `set_account`, before executing.

## Roadmap

- [x] Decode: CPI tree, balance diffs, compute, ALT resolution
- [x] State reconstruction (accounts + program binaries)
- [x] Deterministic local replay
- [x] What-if mutation engine (lamports + data)
- [x] Baseline-vs-mutated **diff** output (web UI)
- [x] Hermetic **fixtures** — freeze state once, replay deterministically offline
- [x] Scenario suites + post-state **assertions**; `svmscope test` runner for CI
- [x] IDL-aware account decoding (named fields on Anchor program accounts, on-chain or pasted IDL)
- [x] **Instruction decoding** — named instructions + decoded arguments + named accounts, from the on-chain IDL (Anchor) or known native layouts, at every depth of the call tree
- [x] Richer assertions (`token_amount`, `lamports_delta`, `token_delta`)
- [x] **Time travel** — warp the clock (slots/epochs/seconds or absolute) for replays, what-ifs, and suites
- [x] Any cluster or **custom RPC** endpoint, per request
- [ ] Anchor event decoding (`emit!` events from the program logs)
- [ ] Archival state so fixtures capture pre-transaction state at the exact slot
- [ ] Named-field assertions (`pool.reserve_a == N` instead of `u64@64`)
- [ ] Cross-account invariants (token conservation, solvency)

See [VISION.md](./VISION.md) for the full architecture.

## Live

| | |
| --- | --- |
| **App** | https://svmscope.vercel.app |
| **Engine (API)** | https://svmscope.onrender.com |

The UI is static and hosted on Vercel; the engine runs as a container (it executes
real Solana programs in an embedded SVM, so it needs a long-lived process). They
talk over HTTP — point the UI at any engine with `?api=<url>`.

## API & SDK

`cargo run --bin server` exposes the engine over HTTP (CORS-enabled; `GET /api`
lists the surface):

| Endpoint | Description |
| --- | --- |
| `GET /analyze/{sig}` | Decode a tx: named CPI tree (instruction names, decoded args & accounts), balances, compute, IDL-decoded accounts. |
| `GET /replay/{sig}` | Re-execute it locally against reconstructed pre-state. |
| `POST /simulate` | `{ signature, mutations[], time_travel? }` — replay with what-if edits. |
| `POST /simulate_suite` | `{ signature, scenarios[], time_travel? }` — scenario tests with assertions. |
| `POST /preflight` | `{ transaction, mutations[] }` — simulate an **unsigned** tx before sending. |
| `GET /freeze/{sig}` | Capture a deterministic, offline fixture. |

**Clusters:** every endpoint takes an optional cluster — `?cluster=devnet` (or
`mainnet` / `testnet` / `localnet`) on GETs, a `cluster` field on POSTs, or
`?rpc=<url>` for a custom endpoint — so one instance serves them all. The UI's
cluster dropdown includes a **Custom RPC…** option; the CLI takes `--cluster <name>`
/ `--rpc <url>`.

**Time travel:** `time_travel` warps the SVM clock before executing — relative
(`{ "epochs": 1 }`, `{ "seconds": 2592000 }`, `{ "slots": N }`) or absolute
(`at_unix_timestamp` / `at_slot` / `at_epoch`) — so time-gated logic (vesting
cliffs, staking cooldowns, auction ends) is testable without waiting. The UI's
**⏱ Now** control applies it globally.

A typed TypeScript client lives in [`sdk/`](./sdk) — a single zero-dependency
file; vendor `sdk/src/index.ts` until it's published to npm:

```ts
import { Svmscope } from "./svmscope";
const svm = new Svmscope("http://127.0.0.1:3000");
const result = await svm.preflight(unsignedTx.serialize()); // preview before signing
```

## Deploy

The server reads `HOST`, `PORT`, and `SVMSCOPE_RPC_URL` from the environment, so it
runs unchanged locally or on any platform.

**No CLI / no local Docker — deploy from GitHub in a browser:**
[render.com](https://render.com) → **New → Blueprint** → connect this repo (it reads
[`render.yaml`](./render.yaml)) → set `SVMSCOPE_RPC_URL` in the dashboard → deploy.
Builds the Dockerfile on Render's builders.

**Or with a CLI:**

```bash
# Docker
docker build -t svmscope .
docker run -p 3000:3000 -e SVMSCOPE_RPC_URL=https://your-rpc svmscope

# Fly.io (builds remotely — no local Docker needed)
fly launch --no-deploy && fly deploy
fly secrets set SVMSCOPE_RPC_URL=https://your-rpc   # optional, for a faster RPC
```

The public mainnet RPC is heavily rate-limited; point `SVMSCOPE_RPC_URL` at your own
endpoint for a hosted deployment.

## Built with

Rust · [`litesvm`](https://crates.io/crates/litesvm) · [`solana-client`](https://crates.io/crates/solana-client) · [`solana-transaction`](https://crates.io/crates/solana-transaction) · [`serde_json`](https://crates.io/crates/serde_json)
