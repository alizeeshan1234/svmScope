# Historical state from transaction events: measured prototype

Three selected historical Pump transactions reproduced their logged trade
amounts and four ending reserve fields after restoring reserve preimages from
their own TradeEvent records. This required ordinary public Solana RPC, no
historical-state service, and no previously running svmscope recorder.

This is a protocol-specific partial-state technique, **not complete exact
replay, a populated 30-day archive, or arbitrary-target-slot support**. All
remaining account bytes and program binaries were loaded from current RPC.
Every sample still differed from the chain in compute units. The samples
were the first decoded Pump event in each of three manually selected blocks;
they do not establish a success rate for other transactions or programs.

| Transaction date (UTC) | Slot | Baseline | With event reserve fields | Chain CU | Replay CU |
|---|---:|---|---|---:|---:|
| 2026-09-04 | 444365059 | Succeeds, wrong SOL amount | Trade amounts and four ending reserves match | 73,485 | 74,001 |
| 2026-08-23 | 441125059 | Fails, custom error 6024 | Trade amounts and four ending reserves match | 72,258 | 72,902 |
| 2026-08-13 | 438965059 | Succeeds, wrong token amount | Trade amounts and four ending reserves match | 100,869 | 101,511 |

On September 4, the baseline trade event reported 267,475,023 lamports;
the chain reported 941,170,770. Restoring reserves yielded 941,170,770.
The September 4 baseline here includes the Token-2022 owner fix described
below; without that fix both variants failed with IncorrectProgramId.

Across the three blocks, 59 successful Pump trade events were extracted.
All 31 observed consecutive same-mint event pairs agreed on the four reserve
fields at their boundary. This is a consistency check of the inversion,
not independent proof of full account contents. Events can expose fields
that generic transaction metadata does not contain.

The field layout and transition rules come from Pump's official
[program documentation](https://github.com/pump-fun/pump-public-docs/blob/main/docs/PUMP_PROGRAM_README.md)
and [IDL](https://github.com/pump-fun/pump-public-docs/blob/main/idl/pump.json).
The experiment decodes the legacy event prefix and does not claim support
for all subsequent quote-asset or account-layout changes.

`history-event-results.json` contains signatures, amounts, reserve values,
compute units, dates and SHA-256 hashes of the original block responses.
The complete local input and output bundles are in `target/history-probe/`.
They are experiment artifacts, not imported recorder state.

Reproduce with a finalized `getBlock` response using `encoding: "json"`,
`transactionDetails: "full"`, `rewards: false`, and
`maxSupportedTransactionVersion: 0`:

```sh
python3 scripts/probe_pump_history.py target/history-probe/2026-08-23-block.json 441125059 --out /tmp/pump-fields.json
cargo run --release --example pump_history_probe -- /tmp/pump-fields.json 0
```

The Rust example uses public RPC unless `RPC` is supplied. Its output
explicitly identifies the result as experimental partial-field recovery.
It patches four reserves only, checks the account owner, PDA and
BondingCurve discriminator, and rejects selecting a later event for the
same curve within the same transaction. Even the first event is only a
candidate transaction preimage: other earlier instructions may modify the
account. Failed or truncated log streams are rejected by the extractor.

The live experiment also identified a production loading bug:
`PreState::reconstruct` always assigned closed token accounts to the legacy
SPL Token program. The fix retains `programId` from historical token-balance
metadata, allowing Token-2022 calls to reach execution with the correct
account owner. It does not reconstruct missing Token-2022 extensions.

Validation: three targeted Rust closed-account tests, five Python event
boundary tests, release compilation, and Clippy with warnings denied pass.

Remaining work before any broader product claim: implement and validate
versioned protocol-specific recovery, obtain the other required historical
fields, establish target-slot account-write coverage, and compare full
execution state. No production deployment or GitHub archive upload was
performed by this experiment.
