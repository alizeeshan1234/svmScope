# svmscope

**A transaction simulator for Solana — decode any transaction, replay it locally, and change reality to see what happens.**

`svmscope` takes a mainnet transaction and:

1. **Decodes** it into a readable report — the full cross-program call tree, who gained
   or lost SOL, and compute units per program (with Address Lookup Table resolution).
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
```

Uses the public `https://api.mainnet-beta.solana.com` endpoint, which only retains
recent transactions. For older transactions, point it at an archival RPC.

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
- [ ] Baseline-vs-mutated **diff** output
- [ ] `--mutate` support for data / oracle prices from the CLI
- [ ] Archival state for exact replay of older transactions
- [ ] Invariant checks + generated regression tests

See [VISION.md](./VISION.md) for the full architecture.

## Built with

Rust · [`litesvm`](https://crates.io/crates/litesvm) · [`solana-client`](https://crates.io/crates/solana-client) · [`solana-transaction`](https://crates.io/crates/solana-transaction) · [`serde_json`](https://crates.io/crates/serde_json)
