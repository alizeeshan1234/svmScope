# svmscope

**Start from a real transaction.** Decode it, replay it locally in an embedded SVM, mutate state and time-travel, freeze it into a fixture, and assert on it forever.

The Solana testing stack has unit testing ([LiteSVM](https://github.com/LiteSVM/litesvm)), instruction testing ([Mollusk](https://github.com/anza-xyz/mollusk)), and integration testing from current mainnet state ([Surfpool](https://github.com/txtx/surfpool)). svmscope covers the fourth quadrant: **post-mortem and regression testing from a historical transaction** — a real signature already encodes its entire world (accounts, programs, state), so one signature replaces a hundred lines of test setup.

```rust
use svmscope::{Check, Cmp, Mutation, Scope};

let scope = Scope::new("https://api.mainnet-beta.solana.com");

// Decode: the full CPI tree, every instruction named from its on-chain IDL.
let analysis = scope.analyze(signature)?;

// Reconstruct the transaction's world once — every account, every program ELF.
// All RPC happens here; every run below is local, instant, and free.
let mut replay = scope.replay(signature)?;
assert!(replay.run()?.result.success);

// What-if, with declarative checks (a reverting replay is data, not an Err):
let outcome = replay.verify(
    "draining the vault makes the claim revert",
    &[Mutation::lamports(vault, 0)],
    &[
        Check::revert_contains("InsufficientFunds"),
        Check::account(user).token_delta(Cmp::eq(0)).build(),
    ],
)?;
assert!(outcome.pass);

// Time is just the Clock sysvar — warp it. A vesting claim that reverts
// today succeeds at +30 days.
replay.advance_seconds(30 * 86_400);
let future = replay.run()?;

// Freeze the whole world into one JSON file: accounts, ELFs, IDLs, and the
// recorded on-chain outcome. It replays identically forever, offline.
std::fs::write("fixtures/claim.json", scope.capture(signature)?.to_json()?)?;
```

```toml
[dependencies]
svmscope = "0.2"
```

## The three things it does

**1. Post-mortem a transaction.** Paste a failed mainnet signature: the CPI tree arrives with instructions, arguments, and accounts *named* (resolved from the on-chain Anchor IDL or known native layouts), balance and token diffs, per-program compute units, and — on replay — the failure explained in plain language (`"SlippageToleranceExceeded"`, not `Custom(6001)`). Then change one thing and run it again.

**2. Test against reality.** `scope.replay(sig)` rebuilds the transaction's world inside [LiteSVM](https://github.com/LiteSVM/litesvm) — no validator, no ports, no devnet dance. Mutate lamports or bytes, flip runtime feature gates, warp the clock by slots/epochs/seconds or to an absolute point, and assert on outcomes *and resulting state* with a mollusk-style `Check` DSL, including named fields: `Check::account(pool).field("reserve_a", Cmp::gt(0))`.

**3. Regression-test it in CI, offline.** `scope.capture(sig)` freezes everything — transaction, accounts, program binaries, IDLs, and the actual on-chain outcome — into one portable JSON fixture. `Replay::from_fixture` rebuilds the world with **zero RPC**: deterministic suites in CI with no key, no drift, no flakes, and `Check::matches_onchain()` as the "does it still behave like mainnet" primitive.

Errors are typed and self-explanatory: a typo'd mutation address is a hard `Error::MutationTargetMissing`, never a fake "revert" your test happily accepts; an unknown field name errors *listing the available fields*.

Run the [examples](./examples) against any transaction:

```bash
cargo run --example post_mortem -- <signature>
cargo run --example what_if    -- <signature> <account>
cargo run --example fixture_ci -- <signature>
```

## The CLI

The same engine, on the command line:

```bash
cargo run -- <SIGNATURE>                          # decode + replay
cargo run -- <SIGNATURE> --mutate <ADDR>:<LAMPORTS>  # + a what-if
cargo run -- freeze <SIGNATURE> -o fixture.json   # capture a fixture
cargo run -- test suite.json                      # run a scenario suite (CI-ready)
cargo run -- report suite.json -o report.html     # shareable HTML report
cargo run -- upgrade fixture.json                 # re-capture an old fixture as v2
```

Every command takes `--cluster <mainnet|devnet|testnet|localnet>` or `--rpc <url>`.

```
$ cargo run -- <SIGNATURE>

#0  Route V2  (JUP6LkbZbjS1jKKwapdHNy74zcZ3tLUZoi5QNyVTaV4)
    └─ [2] Swap  (BiSoNHVpsVZW2F7rx2eQ59yQwKxzU5NvBcmKshCSUypi)
        └─ [3] Transfer  (TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA)

-- compute units per program --
JUP6LkbZbjS1jKKwapdHNy74zcZ3tLUZoi5QNyVTaV4  178113 CU

-- replay --
REPLAY: failed ❌  error: InstructionError(4, Custom(6024))
```

That's the *real* Jupiter program executing locally. Swaps often fail on replay with a slippage error — not a bug, but the honest consequence of **state drift**: replays run against current reconstructed state, and pool prices have moved since the original slot. That's exactly why fixtures exist: freeze once, and the replay is pinned forever.

## Scenario suites (JSON)

Suites also exist as a JSON format — the same one the web UI exports and `svmscope test` runs. Reference a fixture for the deterministic, offline path:

```json
{
  "fixture": "fixture.json",
  "scenarios": [
    { "name": "baseline replays faithfully", "expect": "success" },
    {
      "name": "draining the pool reverts",
      "expect": "revert",
      "mutations": [{ "kind": "data", "address": "<POOL>", "offset": 64, "bytes_hex": "0000000000000000" }],
      "asserts": [{ "address": "<POOL>", "kind": "field", "field": "amount", "op": "==", "value": 0 }]
    }
  ]
}
```

```
$ cargo run -- test suite.json
svmscope test — fixture 4RHX…oJWt (16 accounts, 5 programs) [deterministic, offline]
  PASS  baseline replays faithfully  (expect: succeeds; got: succeeded)
  PASS  draining the pool reverts  (expect: reverts; got: reverted (Custom(6004)))
2/2 passed
```

Assert kinds: `lamports`, `u64` (at an offset), `token_amount`, `lamports_delta`, `token_delta`, and **named fields** — `"field": "pool.reserveA"` resolved through SPL layouts or the program's IDL (`field_delta` for changes). v2 fixtures carry their IDLs, so named-field asserts work fully offline.

## How it works

- **Decode** — walks `getTransaction`: the CPI tree from `innerInstructions` + `stackHeight`, diffs from `pre/postBalances`, compute from the logs, Address Lookup Table resolution, instruction/account/field naming from on-chain IDLs (Anchor and the Program Metadata program) or built-in native layouts.
- **Reconstruct** — `getMultipleAccounts` for every touched account; programs resolve through the upgradeable loader's programdata pointer to the raw ELF; closed accounts (drained fee payers, closed token accounts) are rebuilt from the transaction's own metadata; pre-transaction SPL balances are rewound so swaps replay faithfully.
- **Replay** — everything loads into a pristine LiteSVM per run (sigverify/blockhash checks off — the original blockhash can't be valid in a fresh SVM), the clock anchored to the transaction's real slot and block time. There is no validator to wait for, so runs are microseconds and trivially parallel.
- **Time travel** — programs read time from the Clock sysvar; we own it. Warps move slot, epoch, and timestamp *coherently* (432k slots/epoch, ~400ms/slot), so a program checking all three sees a consistent world.

## Live app

| | |
| --- | --- |
| **App** | https://svmscope.vercel.app |
| **Engine (API)** | https://svmscope.onrender.com |

The web UI is a visual layer over this same library — paste a signature, click through the CPI tree, edit named account fields, run suites, freeze fixtures. The `server` feature (off by default — library consumers never compile axum) builds the HTTP API: `cargo run --bin server --features server` → `http://127.0.0.1:3000`, `GET /api` lists the surface. A typed TypeScript client lives in [`sdk/`](./sdk).

## Deploy your own

The server reads `HOST`, `PORT`, and `SVMSCOPE_RPC_URL` from the environment:

```bash
docker build -t svmscope . && docker run -p 3000:3000 -e SVMSCOPE_RPC_URL=https://your-rpc svmscope
```

Or on [render.com](https://render.com): **New → Blueprint** → connect the repo (reads [`render.yaml`](./render.yaml)) → deploy. The public mainnet RPC is heavily rate-limited; use your own endpoint for anything hosted.

## Roadmap

- [x] Decode, reconstruct, replay, mutate, time-travel, feature gates
- [x] Hermetic fixtures (v2: IDLs + recorded outcome captured — offline named-field asserts and `matches_onchain`)
- [x] Typed errors; mutations validated up front (no silently-passing revert tests)
- [x] `Scope`/`Replay` library API — fetch once, replay forever
- [x] Mollusk-style `Check` DSL with named-field assertions
- [ ] Codama/Shank IDL support (named fields for native & Pinocchio programs)
- [ ] Named-field mutations — `Mutation::field(addr, "count", 99)`
- [ ] Async/trait RPC abstraction
- [ ] Anchor event decoding; archival state at the exact slot; cross-account invariants

See [VISION.md](./VISION.md) for the full architecture.

## Built with

Rust · [`litesvm`](https://crates.io/crates/litesvm) · [`solana-client`](https://crates.io/crates/solana-client) · [`thiserror`](https://crates.io/crates/thiserror) · [`serde_json`](https://crates.io/crates/serde_json)
