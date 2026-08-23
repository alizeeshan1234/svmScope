# svmscope

**Transaction autopsy for Solana — paste a signature, see exactly what happened.**

`svmscope` is a command-line tool that fetches any Solana transaction and breaks it
down into a readable report: the full cross-program call tree, who gained or lost SOL,
and how much compute each program burned. It resolves Address Lookup Tables, so it
works on modern versioned transactions — including the deeply-nested DeFi swaps that
explorers show as a wall of raw data.

## Example

Running it on a real Jupiter swap that routes through three different AMMs:

```
$ cargo run -- <SIGNATURE>

#0  ComputeBudget111111111111111111111111111111
#1  ComputeBudget111111111111111111111111111111
#2  ComputeBudget111111111111111111111111111111
#3  JUP6LkbZbjS1jKKwapdHNy74zcZ3tLUZoi5QNyVTaV4
└─ [2] BiSoNHVpsVZW2F7rx2eQ59yQwKxzU5NvBcmKshCSUypi
    └─ [3] TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA
    └─ [3] TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA
└─ [2] DRVSpZ2YUYYKgZP8XtLhAGtT1zYSCKzeHfb4DgRnrgqD
    └─ [3] TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA
    └─ [3] TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA
└─ [2] FLUX6xBayGxLX9UcimVRxXFMHH6q43mAbRvDzSpCsvfK
    └─ [3] TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA
    └─ [3] TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA
└─ [2] JUP6LkbZbjS1jKKwapdHNy74zcZ3tLUZoi5QNyVTaV4

-- account balance changes --
capsFrvPsaCQ6y7RUk3TYkB5D1oNCNjRb4uYuB5UCsZ  -5451 lamports

-- compute units per program --
BiSoNHVpsVZW2F7rx2eQ59yQwKxzU5NvBcmKshCSUypi 98926 CU
DRVSpZ2YUYYKgZP8XtLhAGtT1zYSCKzeHfb4DgRnrgqD 29612 CU
FLUX6xBayGxLX9UcimVRxXFMHH6q43mAbRvDzSpCsvfK 34858 CU
JUP6LkbZbjS1jKKwapdHNy74zcZ3tLUZoi5QNyVTaV4  178113 CU
```

## What it shows

- **CPI call tree** — every top-level instruction and the cross-program invocations
  nested underneath it, indented by call depth. You can read the whole execution path
  at a glance.
- **Account balance changes** — every account that gained or lost lamports, and by how
  much. The trade is visible right in the numbers.
- **Compute units per program** — how much compute each program consumed, parsed from
  the transaction logs.

## How it works

`svmscope` calls `getTransaction` over JSON-RPC, then walks the response:

- The **call tree** comes from `meta.innerInstructions` and each instruction's
  `stackHeight` (the nesting depth).
- **Address Lookup Tables** are resolved by concatenating the static account keys with
  `meta.loadedAddresses` (writable, then readonly), so program IDs in versioned
  transactions resolve correctly.
- **Balance changes** come from `meta.preBalances` vs `meta.postBalances`.
- **Compute** is parsed from the `consumed … compute units` log lines.

## Usage

```bash
# build
cargo build

# run against any mainnet transaction signature
cargo run -- <SIGNATURE>
```

By default it queries the public `https://api.mainnet-beta.solana.com` endpoint, which
only retains recent transactions. For older transactions, point it at an archival RPC.

## Roadmap

`svmscope` is v0.1. The direction from here:

- [ ] **File mode** — analyze a saved `getTransaction` JSON offline.
- [ ] **Configurable RPC endpoint** — pass your own (archival) RPC URL.
- [ ] **IDL decoding** — decode instruction data into human-readable calls, not just program IDs.
- [ ] **Deterministic replay** — re-execute a transaction locally against forked state.
- [ ] **"What-if" mutations** — change an oracle price, a signer, or an account, then replay to see what breaks.

## Built with

Rust · [`solana-client`](https://crates.io/crates/solana-client) · [`serde_json`](https://crates.io/crates/serde_json)
