# svmscope reference example: counter + linear SOL vesting

A complete, runnable reference for testing an Anchor program with
[`svmscope`](https://crates.io/crates/svmscope) **as a real dependency** — the
Rust consumer here depends on the published crate (`svmscope = "0.2"`), not a
local path. It builds transactions inside Rust (no TypeScript-signature copy
step) and carries each one straight into replay, mutation, time travel,
assertions, and offline fixtures.

The Anchor program deliberately contains two examples:

| Example | Why it exists | svmscope behavior demonstrated |
| --- | --- | --- |
| Counter | Smallest possible state mutation | Build, sign, submit, capture, and replay a successful transaction |
| SOL vesting | Real clock-dependent state machine | Capture a landed revert, explain an Anchor error, mutate account state, time-travel the Clock, assert named fields, and freeze an offline fixture |

The vesting program is production-shaped educational code that has **not** been
independently audited. Do not custody real funds with it without a security
review.

## Layout

- `programs/svmscope_crate/src/lib.rs` — the counter + vesting Anchor program.
- `scope-test/` — a standalone Rust crate that depends on the published
  `svmscope` and drives the whole workflow:
  - `src/main.rs` — the end-to-end reference run (needs a validator).
  - `tests/replay_engine.rs`, `tests/builder_encoding.rs`,
    `tests/spec_and_public_api.rs`, `tests/adversarial.rs` — **129 offline
    tests** (no validator, run in CI) covering the replay engine, the IDL
    builder + Borsh encoding, the JSON/spec surface, and adversarial/DoS input.
  - `tests/localnet.rs` — **validator-gated online tests** (`#[ignore]`) that
    submit real transactions to a local validator and capture them.
- `tests/svmscope_crate.ts` — the program's ordinary Anchor integration tests.
- `target/idl/svmscope_crate.json` — the generated IDL both clients consume.

## Run the offline Rust test suite (no validator)

```bash
cd scope-test
cargo test
```

All 129 offline tests run against the published crate with no network — this is
the CI-safe surface.

## Run the full end-to-end demonstration (needs a validator)

Build the program (from this directory):

```bash
anchor build
```

Start a clean validator with the program preloaded:

```bash
solana-test-validator --reset \
  --bpf-program 41NQKxFZ2eEAFPSKyP2BEvw2bkxwXjC8Wjrxcy6njF8u \
  target/deploy/svmscope_crate.so
```

In a second terminal, run the reference workflow and/or the online tests:

```bash
cd scope-test
cargo run                                   # the narrated end-to-end demo
cargo test --test localnet -- --ignored     # the validator-gated tests
```

The consumer defaults to `http://127.0.0.1:8899`; override with
`SVMSCOPE_RPC_URL=<url>` if your validator is elsewhere.

The reference run does this in order:

1. Funds fresh Rust keypairs from localnet.
2. Builds counter instructions from the program IDL.
3. Submits and locally replays the counter increment.
4. Creates a 30-day vesting schedule with a 7-day cliff.
5. Submits a claim before the cliff (RPC preflight disabled).
6. Confirms the failed instruction still landed and has a signature.
7. Resolves Anchor error `6003` to `CliffNotReached` via the supplied IDL.
8. Mutates the captured schedule timestamps and proves the claim succeeds.
9. Restores the state, advances the Clock 31 days, and proves the full amount is claimable.
10. Freezes the captured world and repeats baseline/time-travel checks offline.

The core transaction construction looks like this:

```rust,ignore
let mut captured = program
    .method("claim_vested")?
    .payer(&beneficiary)
    .account_signer("beneficiary", &beneficiary)
    .account("schedule", schedule_pda)
    .arg("schedule_id", schedule_id)
    .send_and_capture()?;

assert!(!captured.replay.recorded().unwrap().success); // landed revert, captured as data

captured.replay.advance_seconds(31 * 86_400);           // warp past the cliff
assert!(captured.replay.run()?.result.success);         // same tx now succeeds
```

## Program behavior

`create_vesting` creates a deterministic schedule PDA seeded by
`["vesting", beneficiary, schedule_id.to_le_bytes()]`, storing the creator,
beneficiary, timestamps, total and claimed amounts, and bump. The deposited SOL
is held in the program-owned schedule account above its rent reserve.

`claim_vested` reads the Clock sysvar. Before the cliff it returns
`CliffNotReached`; after it, it releases the linearly vested amount
(`total × (now − start) / (end − start)`, minus what's already claimed), with
the full amount vested at or after `end_ts`. `close_vesting` is creator-authorized
and only succeeds once the escrow is fully claimed. The code uses checked
arithmetic, preserves the rent reserve, enforces creator/beneficiary
relationships, caps schedules at ten years, and emits creation/claim events.

## Why the failed claim is useful

A program revert is a valid landed transaction, so svmscope returns
`Ok(CapturedTransaction)` and stores the failure under `replay.recorded()`;
only RPC, timeout, malformed-transaction, or reconstruction problems return
`Err`. That makes the pre-cliff claim an honest regression fixture: CI can assert
it still fails before the cliff and succeeds after time travel — without waiting
31 days or touching the validator.
