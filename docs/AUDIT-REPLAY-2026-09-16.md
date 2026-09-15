# Replay-at-slot launch check — September 16, 2026 (IST)

## Verdict: hold the exact, universal 30-day announcement

The feature executes real historical replays and presents useful provenance, but the tested deployment does **not** establish exact replay for every program/account at every slot in the window. All three initial completed live replay requests contained unproven historical inputs. There are also reproducible certificate, account-editor, date-selection, and retention-boundary defects.

Tested https://svmscope.vercel.app/ and its configured API, https://svmscope-engine.onrender.com/, against checkout `e0de020`. The downloaded frontend and local `static/index.html` had identical SHA-256 `4ad79a65d6f7d85a63775439910ea5fabf1bca61f18be1a0713625dba1dfec96`. The backend reports version `0.6.0`; its exact deployed commit is not exposed. Times below are individual observations, not latency percentiles.

All live checks used the existing public service. No infrastructure, subscriptions, billing, or paid data sources were enabled. Tests used no wallet and submitted no on-chain transactions.

## Live results

| Check | Result |
| --- | --- |
| Homepage, API discovery, transaction analysis | Passed |
| Empty, negative, fractional slot in browser | Rejected before replay |
| Missing/negative/fractional API slot | HTTP 400 |
| Future target slot | HTTP 400 with current-slot explanation |
| August 12 target, outside current rolling window | HTTP 400, 34 days old |
| Recent Orca transaction at original slot | HTTP 200 in **274.47 s**, execution succeeded; **5 current-state accounts** |
| Compare that replay with recorded transaction | Overview, logs, CPI tree, SOL changes, token changes, and compute all equal |
| Repeat identical Orca request | HTTP 200 in **0.37 s**, identical response; this verifies caching, not an independent fresh execution |
| Recent Pump transaction evaluated at September 4 slot | HTTP 200 in **117.47 s**; **16 drifted accounts**, execution failed with `InvalidInstructionData` |
| September 4 Pump transaction at original slot | HTTP 200 in **210.43 s**, execution succeeded; **3 current-state accounts**; overview, logs, CPI tree, SOL/token changes, compute matched the recorded result |
| Browser historical result, fidelity warning, return to on-chain view | Passed; no JavaScript exceptions |
| 390px mobile viewport | No page-wide horizontal overflow; replay controls visible |
| Date selection September 1 06:30 UTC | Returned **06:31:42 UTC**, 102 seconds late |
| Invalid calendar date February 30 | Incorrectly accepted as March 2 |
| Target roughly 30.5 days old | Passed age check and reached signature validation; should have failed the 30-day check |

**A failure at a different target is not itself a bug:** the account/program may not have existed or the instruction may legitimately have failed then. The defect is insufficient evidence for exact inputs, not the unsuccessful outcome alone.

The live API advertised `recorded_from: 446444753` and `replay_window_days: 30`. A global recorder start slot is not proof of account-by-account historical coverage.

### Reproduction transactions

- Orca original: `5rgzzBr2vVNxftzJHtF4EJhZGtg2nPunLrxAcHqrXg2ejRAXXc2kS4W3vXSmC1HeSp21bs2ZT4XT49y21DLGSTCq`, slot `447350709`.
- Pump different target: `2Vjtt1PZ3LpAyHZk7Vo8W6h6gynKRq6iHBDxC8nZXhJBZMYGTYdMyopC6aqiLNMx8a5ZXFYFFsnUFSAoRjob4RXB`, landed at `447351391`, target `444365059` (September 4).
- September 4 original: `dmr57ZNeg3qUrp3eyA7RSDkwtPK2vkTw33sDKjvZp8P32PxMhZYLteUaAUPjZYm6Tb6cctRURFdrDnnNPVMc1yY`, slot `444365059`.
- Routes: `/analyze_at/{signature}` and `/analyze_at/{signature}?slot={target}`.

## Findings

### P1 — Public exactness guarantee exceeds observed coverage

`static/index.html:1154` promises every account and binary over 30 days. The recent replay used 5 `CurrentRpc` accounts; the different-target request used 7 `CurrentRpc` and 9 `MetadataEstimate` accounts. The older original-slot replay used 3 `CurrentRpc` accounts. All three certificates were `Reconstructed`. Matching one transaction's output is useful evidence, but does not prove all its inputs were historical or universal coverage.

**Launch requirement:** complete historical inputs and execution context must be established for the claimed scope. Until that exists, display coverage and approximation explicitly; changing copy alone does not fulfill the universal requirement.

### P1 — Certificates can call incomplete state exact

In `static/index.html:2399–2463`, successful execution receives “replayed as it happened” even with drift. The final exactness sentence excludes several warnings but omits unproven absence, partial event data, and unchecked program versions. Three offline cases using the actual downloaded renderer all emitted “Every account is as it was at that moment — this replay is exact”:

1. `Absent { proven: false }` with the account listed in `drifted`.
2. `EventLog` that restores only selected reserve fields, leaving other bytes current.
3. `Program { upgraded_since: null }`.

The “matches the real on-chain result” message checks **only the success boolean**, not error identity, compute, logs, or state changes. Two unrelated failures can therefore “match.” The live Orca example did match the richer comparison independently, but the UI does not perform it.

**Fix:** derive exactness from complete per-input evidence, and make outcome comparison explicit. Treat partial/unknown evidence as unresolved. Do not infer exactness from execution success.

Reproduce the renderer cases without network:

```sh
node docs/qa/replay-2026-09-16/certificate-probe.mjs
```

### P1 — Historical page displays current account-editor values

`Scope::analyze_at` builds replay-derived sections, then calls `analysis_of`, whose `accounts` field comes from `decode::describe_accounts(&self.client, ...)` against current RPC (`src/scope.rs:626–723`). The editor and generated mutation scenarios consume those current values.

In the Orca response, the fee-payer editor shows `15,788,933,176` lamports. The replay reports post-balance `15,793,373,960` and delta `-5,001`, implying pre-balance `15,793,378,961`. The editor thus shows neither the replay's pre-state nor its post-state.

**Fix:** populate historical account details from the loaded replay context, with a stated before/after contract. Verify displayed editable values against the actual simulation starting state.

### P1 — Recorded provenance does not always prove complete historical state

Code inspection found additional reasons that a no-drift certificate cannot currently justify the universal guarantee:

- The account-change stream schema has no lamports (`src/substreams.rs:294–306`). The general stream path fills lamports from today's account or an estimated rent minimum, then labels the whole account `Recorded` (`src/scope.rs:1497–1525`). Historical account bytes do not establish historical SOL balance.
- `LogStore::covers` describes global polling intervals, not complete finalized mutations per account (`src/records.rs:1029–1070`). Polling uses confirmed state every two seconds, in batches, and can miss intervening writes. That cannot prove an arbitrary slot's exact state.
- Retention removes every version older than the cutoff (`src/records.rs:416–434`). A quiet account's last predecessor can be deleted even though its state remains necessary throughout the retained window.
- `record` silently ignores an older streamed version once a newer version exists (`src/records.rs:940–944`). The promise that each historical version is persistently cached is therefore conditional; the response cache can hide repeat reconstruction cost temporarily.

These are code findings, not claims that every path was independently reproduced against live state. Fix provenance/coverage semantics and preserve boundary anchors before certifying exactness. Historical runtime, sysvars, upgrades, same-slot ordering, closures/recreation, and dependency completeness still need broad acceptance tests.

### P2 — Date lookup can choose the wrong target and accepts invalid dates

`/slot_at?time=2026-09-01T06:30:00Z` returned slot `443356092`, requested time `1788244200`, block time `1788244302`: **102 seconds late**. The browser reproduced the same minute-level discrepancy. The resolver makes at most six estimates at 400 ms/slot and returns its last result even if it never reaches the intended tolerance (`server/src/main.rs:749–765`).

`/slot_at?time=2026-02-30T00:00:00Z` returned HTTP 200, normalizing February 30 to March 2. `parse_time` performs calendar arithmetic without validating calendar ranges.

**Fix:** search and verify the nearest available block with an explicit tolerance; return an honest error if unresolved. Validate calendar dates and offsets before arithmetic.

### P2 — The 30-day cutoff is inconsistent

`check_replay_window` floors age to integer days, then rejects only `age_days > 30` (`server/src/main.rs:663–682`), effectively allowing nearly 31 days. A target `439615716` at August 16 09:30 UTC passed the window check on September 15 after 21:25 UTC; `/replay_at/invalid?slot=439615716` was rejected only later for invalid signature.

The guard also allows requests through when no block time can be read. `/replay_at_slot/{signature}` does not call this guard at all (`server/src/main.rs:1929–1942`). Cache hits bypass revalidation until their six-hour expiry. Retention itself uses a fixed number of slots rather than elapsed chain time.

**Fix:** centralize exact cutoff validation across replay routes and cache hits, compare seconds, and define behavior when chain time is unavailable.

### P2 — Initial replay latency remains substantial

The three measured initial replay requests took 274 s, 117 s, and 210 s. The UI shows elapsed time, but the pre-recording notice estimates one to three minutes only for targets older than the global start slot. The recent 274 s request received no equivalent expectation. A cached repeat was fast. No crowd/load test was performed against the free service.

**Fix:** report queue/reconstruction progress, provide bounded cancellation/timeouts, and validate concurrency and cache behavior before announcement traffic. This report makes no capacity guarantee.

## Local validation and change

The first `cargo test --workspace --offline --lib --bins` run passed 170 tests. The broader workspace run initially failed to compile `examples/fidelity_sweep.rs` because its provenance match omitted the new `Absent` variant.

**Fixed locally:** added separate `absent` / `absent~` labels for proven and unproven absence. This is the only production/example code change in this check. Nothing was deployed.

After that fix:

- `cargo test --workspace --offline`: **193 passed**, 3 ignored (2 require a local validator, 1 ignored documentation example).
- `cargo test -p svmscope --features single-run-trace --offline --lib --test offline_fixture`: **174 passed**.
- `cargo fmt --all --check`: passed.
- `cargo clippy --workspace --all-targets --offline -- -D warnings`: **failed**, existing `filter_map_bool_then` in `src/replay.rs:2754`, plus unused test helpers `encode_chain` and `rewrite` in `src/records.rs`.

Passing offline tests does not establish complete on-chain historical data coverage.

## Evidence and remaining acceptance gates

[Evidence directory](qa/replay-2026-09-16/) contains raw public API responses, browser observations/screenshots, renderer reproductions, timings, and test logs.

Before an exact universal launch, resolve the P1 findings and validate transactions across the window, including quiet accounts, account creation/closure/recreation, upgrades, same-slot writes, failed transactions, and targets different from the original execution. Verify output equality and complete input provenance separately. Also verify the historical data provider's remaining free allowance and restart durability; neither quota nor deployment secrets were inspected. All further work remains subject to the ₹0/$0 constraint.
