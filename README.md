# svmscope

[![crates.io](https://img.shields.io/crates/v/svmscope.svg)](https://crates.io/crates/svmscope)
[![docs.rs](https://img.shields.io/docsrs/svmscope)](https://docs.rs/svmscope)
[![CI](https://github.com/alizeeshan1234/svmScope/actions/workflows/ci.yml/badge.svg)](https://github.com/alizeeshan1234/svmScope/actions/workflows/ci.yml)
[![license: MIT](https://img.shields.io/crates/l/svmscope.svg)](./LICENSE)

**Start from a real transaction.** Decode it, replay it locally in an embedded SVM, mutate state and time-travel, freeze it into a fixture, and assert on it forever.

The Solana testing stack has unit testing ([LiteSVM](https://github.com/LiteSVM/litesvm)), instruction testing ([Mollusk](https://github.com/anza-xyz/mollusk)), and integration testing from current mainnet state ([Surfpool](https://github.com/txtx/surfpool)). svmscope covers the fourth quadrant: **post-mortem and regression testing from a historical transaction** — a real signature already encodes its entire world (accounts, programs, state), so one signature replaces a hundred lines of test setup.

## Add it to a Rust project

svmscope is a testing tool, so add it as a dev-dependency:

```bash
cargo add --dev svmscope
```

```toml
[dev-dependencies]
svmscope = "0.2"
```

Then point it at a real transaction and replay it locally — no validator, no
setup:

```rust,no_run
use svmscope::{Check, Mutation, Scope};

# fn main() -> Result<(), Box<dyn std::error::Error>> {
let scope = Scope::new("https://api.mainnet-beta.solana.com");

// Reconstruct the transaction's world once — every account, every program ELF.
let mut replay = scope.replay("<signature>")?;
assert!(replay.run()?.result.success);

// Then ask "what if?" — a reverting replay is data, not an error.
let out = replay.verify(
    "draining the vault makes the claim revert",
    &[Mutation::lamports("<vault-address>", 0)],
    &[Check::revert_contains("InsufficientFunds")],
)?;
assert!(out.pass);
# Ok(())
# }
```

That's the whole loop: **reconstruct once, replay and mutate forever.** The rest
of this README goes deeper — building and submitting transactions, freezing
offline fixtures, and the full assertion DSL.

The optional HTTP server is not compiled for normal library users. Enable it
only when you need the API server:

```toml
svmscope = { version = "0.2", features = ["server"] }
```

## Library quickstart

```rust,no_run
use svmscope::{Check, Cmp, Mutation, Scope};

let scope = Scope::new("https://api.mainnet-beta.solana.com");
let signature = "a mainnet transaction signature";
let (vault, user) = ("the vault's address", "the user's address");

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
# Ok::<(), Box<dyn std::error::Error>>(())
```

The main API is available directly from the crate root. Import
`Scope`, `Replay`, `Mutation`, `Check`, `Cmp`, `Scenario`, `Fixture`, and result
types as `svmscope::Type`; implementation modules are intentionally private.
The `idl`, `report`, and `spec` modules are public for IDL inspection, HTML
reports, and the JSON scenario format respectively.

## Three ways to mutate state

A what-if is one or more `Mutation`s applied before the replay. There are three,
from coarse to surgical — editing account *bytes* is where most of the power is
(flip an oracle price, a token balance, a vesting cliff, a config flag):

```rust,no_run
use svmscope::{Mutation, Scope};
# fn main() -> Result<(), Box<dyn std::error::Error>> {
# let scope = Scope::new("https://api.mainnet-beta.solana.com");
# let mut replay = scope.replay("<signature>")?;
# let (vault, oracle, config) = ("<vault>", "<oracle>", "<config>");
let out = replay.simulate(&[
    // 1. Set an account's SOL balance.
    Mutation::lamports(vault, 0),

    // 2. Patch a slice at a byte offset — the usual surgical what-if.
    //    e.g. overwrite a u64 price/amount in place with little-endian bytes.
    Mutation::patch(oracle, 8, 1_000_000_u64.to_le_bytes().to_vec()),

    // 3. Replace an account's data wholesale.
    Mutation::data(config, vec![0u8; 128]),
])?;
assert!(out.result.success || out.result.error.is_some());
# Ok(())
# }
```

Prefer named-field mutations to raw offsets where a layout is known — decode the
account (`scope.decode_account` or a fixture's IDL) to find a field's offset, or
assert on it directly with `Check::account(addr).field("amount", Cmp::gt(0))`.
The JSON [scenario suites](#scenario-suites-json) below express the same patches
declaratively with `{"kind":"data","offset":…,"bytes_hex":…}`.

## Build and send the transaction inside the Rust test

You can also start from scratch instead of an existing signature. A Rust test can
build an Anchor instruction from the program IDL, sign it, send it to a local
`solana-test-validator`, wait for it to land, and immediately receive a replay of
the exact pre-transaction state — no copying signatures out of a separate test
suite.

### 1. Build and deploy the Anchor program

Terminal 1 (leave it running):

```bash
solana-test-validator --reset
```

Terminal 2:

```bash
anchor build
anchor deploy --provider.cluster localnet
mkdir -p tests/fixtures
solana-keygen new --no-bip39-passphrase -o tests/fixtures/payer.json
solana airdrop 10 "$(solana address -k tests/fixtures/payer.json)" --url localhost
anchor keys list
```

Keep the generated `target/idl/<program>.json`. The local validator must already
have the program deployed before svmscope constructs the replay because the
replay captures the deployed ELF and all input accounts. Put the address printed
by `anchor keys list` in `YOUR_PROGRAM_ID`. If the instruction updates state,
initialize that state/PDA first and put its address in
`YOUR_EXISTING_STATE_ACCOUNT`.

### 2. Add the test dependencies

```toml
[dev-dependencies]
svmscope = "0.2"
serde_json = "1"
solana-address = "2.6"
solana-keypair = "3.1"
solana-signer = "3.0"
```

### 3. Construct, submit, capture, mutate, and time-travel

```rust,no_run
use std::str::FromStr;

use serde_json::json;
use solana_address::Address;
use solana_keypair::read_keypair_file;
use solana_signer::Signer;
use svmscope::{Mutation, Scope};

fn invokes_program_and_replays_it() -> Result<(), Box<dyn std::error::Error>> {
    let scope = Scope::new("http://127.0.0.1:8899");
    let program_id = Address::from_str("YOUR_PROGRAM_ID")?;
    let state = Address::from_str("YOUR_EXISTING_STATE_ACCOUNT")?;
    let payer = read_keypair_file("tests/fixtures/payer.json")?;
    let idl = serde_json::from_str(&std::fs::read_to_string(
        "target/idl/your_program.json",
    )?)?;

    let mut captured = scope
        .program_with_idl(program_id, idl)
        .method("setValue")?
        .payer(&payer)
        // Names must match the IDL. Nested accounts may use "group.account".
        .account("authority", payer.pubkey())
        .account("state", state)
        .args(json!({ "value": 42 }))?
        .send_and_capture()?;

    // This signature was created here; nothing is copied from a TS test.
    println!("landed transaction: {}", captured.signature);
    assert!(captured.replay.recorded().is_some());

    // Re-execute locally from the state captured immediately before submission.
    let baseline = captured.replay.run()?;
    assert!(baseline.result.success);

    // Mutations and time travel now reuse that in-memory replay with no RPC.
    let changed = captured
        .replay
        .simulate(&[Mutation::lamports(state.to_string(), 0)])?;
    println!("after mutation: {:?}", changed.result.error);

    captured.replay.advance_seconds(30 * 86_400);
    let future = captured.replay.run()?;
    println!("after 30 days: {}", future.result.success);

    // Optional: freeze this newly created transaction for offline CI.
    let fixture = captured.replay.to_fixture()?;
    std::fs::write("fixtures/set_value.json", fixture.to_json()?)?;

    Ok(())
}
```

Use `.account_signer("accountName", &keypair)` when an IDL account must sign,
or `.signer(&keypair)` when its address was supplied separately. Fixed-address
accounts in modern Anchor IDLs, such as the System Program, are filled
automatically. PDAs are addresses, not signers: derive them in the test and pass
the result with `.account(...)`.

Argument values are JSON and are Borsh-encoded in IDL order. Supported values
include booleans; signed and unsigned integers through 128 bits; floats; strings;
public keys; bytes; vectors; options; fixed arrays; and IDL-defined structs and
enums. Pass integers larger than JSON's exact numeric range as decimal strings,
and bytes either as `[0, 1, 255]` or a `"0x..."` string.

`send_and_capture` waits up to 20 seconds. A program revert is still a landed
transaction and returns `Ok(CapturedTransaction)` with
`captured.replay.recorded().unwrap().success == false`. `Err` is reserved for an
RPC failure, timeout, malformed IDL/accounts/arguments, or transaction-building
failure.

If you already construct a `VersionedTransaction` yourself, use the lower-level
path directly:

```rust,ignore
let captured = scope.send_and_capture(signed_versioned_transaction)?;
```

## Offline fixture use

Capture a transaction once while connected to RPC, then load it without any
network access in tests or CI:

```rust,no_run
use svmscope::{Check, Fixture, Replay, Scenario};

let fixture = Fixture::from_json(&std::fs::read_to_string("fixtures/claim.json")?)?;
let replay = Replay::from_fixture(&fixture)?;
let outcomes = replay.run_suite(&[
    Scenario::new("matches mainnet").check(Check::matches_onchain()),
    Scenario::new("still succeeds").check(Check::success()),
])?;

assert!(outcomes.iter().all(|outcome| outcome.pass));
# Ok::<(), Box<dyn std::error::Error>>(())
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

For a full real-world consumer — an Anchor program (counter + SOL vesting) plus a
standalone Rust project that depends on the published crate and drives the entire
build → submit → capture → replay → mutate → time-travel → freeze workflow, with a
129-test offline suite and validator-gated online tests — see the
[**svmscope_example**](https://github.com/alizeeshan1234/svmscope_example) repo.

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

```text
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

```text
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

## Live demo

A hosted web UI over this same library — paste a signature, click through the CPI
tree, edit named account fields, run suites, freeze fixtures — is at
**[svmscope.vercel.app](https://svmscope.vercel.app)**.

## Optional HTTP server

The `server` feature (off by default — library consumers never compile axum)
builds a small HTTP API over the engine, reading `HOST`, `PORT`, and
`SVMSCOPE_RPC_URL` from the environment:

```bash
cargo run --bin server --features server   # → http://127.0.0.1:3000, GET /api lists the surface
```

A typed TypeScript client lives in [`sdk/`](./sdk). Point it at your own RPC
endpoint — the public mainnet RPC is heavily rate-limited.

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

## License

MIT — see [LICENSE](./LICENSE).
