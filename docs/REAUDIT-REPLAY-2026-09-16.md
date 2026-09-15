# Replay-at-slot re-audit — September 16, 2026 (IST)

## Verdict

**Several fixes are verified, but the original exact, universal 30-day launch requirement still does not pass.** The ordinary historical editor values, sampled date lookup, strict age boundary, own-slot age guard, and three false-exact certificate cases improved. A historical account is still missing from the editor; outcome verification and several provenance/retention limitations remain. CI is red on formatting.

This is an audit of checkout `5bdccbecd649f0c735886815e2e3a697f1b3383d` and https://svmscope.vercel.app/ using https://svmscope-engine.onrender.com/. No application code was changed or deployed during this audit. No paid resources, billing, or on-chain transactions were initiated.

## Deployment timing

The frontend matched the checkout byte-for-byte: SHA-256 `54179ec08a407baf304af1e99a25cbc82aeff31d030e0e81153f17d3225fd33b`.

The backend rollout finished **during** the first checks. The image workflow for this commit completed at `2026-09-15T22:01:38Z`. Before the rollout, date responses still had the old behavior and two in-flight replay requests returned HTTP 502 after approximately 49 seconds. By `22:02:07Z`, the engine's own homepage contained the updated wording and its date endpoint rejected February 30. These interrupted requests are recorded as rollout observations, not evidence of a steady-state failure in the new implementation.

All results below are from the repeated checks after that change. The API does not expose a commit identifier, so backend identity is supported by observed new behavior and the public image workflow rather than an explicit build SHA response.

## What is fixed or improved

| Previous issue | Re-audit result |
| --- | --- |
| Unproven absence, partial event fields, unchecked program versions called exact | All three renderer reproductions now suppress the final “this replay is exact” sentence |
| September 1 date picker was 102 seconds late | API now returns a block **1 second** from `2026-09-01T06:30:00Z`; browser displays 06:30 UTC |
| February 30 accepted | HTTP **400** |
| 30.5-day-old target admitted | HTTP **400**, specifically rejected for age |
| `/replay_at_slot` skipped the window guard | August 13 original-slot transaction now rejected as **33.7 days old** |
| Historical editor showed current wallet balance | Orca fee payer now shows **15,793,378,961 lamports**, equal to replay pre-state; all eight changed SOL accounts checked in the September 4 replay also agree |
| Streamed account balance incorrectly treated as established | Different-target replay now marks **18** accounts drifted instead of 16, including two `Recorded` accounts with unproven balances |
| Clippy failed | `cargo clippy --workspace --all-targets --offline -- -D warnings` **passes** |
| Workspace example did not compile | Workspace tests **pass** |

## Live replay results

Same transactions and target slots as the first audit; the previous on-chain records are retained in `docs/qa/replay-2026-09-16/` for comparison.

| Case | HTTP / elapsed | Outcome / provenance |
| --- | --- | --- |
| Orca original slot `447350709` | 200 / **144.08 s** | Success, 41,017 CU; **5 drifted accounts** |
| September 4 Pump original slot `444365059` | 200 / **159.50 s** | Success, 73,485 CU; **3 drifted accounts** |
| Recent Pump transaction at September 4 target `444365059` | 200 / **73.08 s** | `InstructionError(2, InvalidInstructionData)`, 413 CU; **18 drifted accounts** |
| Orca cached repeat | 200 / **0.54 s** | Identical JSON |

For both original-slot cases, overview, logs, CPI tree, SOL changes, token changes, and compute matched the recorded transaction. These are useful successful examples, not proof of complete historical inputs or universal coverage. A failed different-target execution can be legitimate; that failure alone is not an audit finding. Cache equality is not an independent fresh-execution determinism test. Durations are individual measurements, not capacity estimates.

Browser checks passed for analysis, date selection, historical rendering, return to on-chain view, and 390px mobile layout. No JavaScript exceptions or page-wide horizontal overflow were observed.

## Remaining findings

### P1 — Historical account editor still omits accounts absent today

**Reproduced against the updated live backend.** In the September 4 replay, account
`AYXGZkyfQVeXfVQFbGcrvvMd33ukYDSvTkhiiibm1ZdL`:

- Has `Recorded { slot: 444365014 }` provenance and a nonempty account hash.
- Appears in token changes: delta `-9092091881136`, post amount `27276275643411`.
- Is **absent from the response's `accounts` list**, so it is unavailable in the historical account editor/scenario builder.

Cause: `src/scope.rs:657–670` starts with today's `analysis.accounts`, then replaces only entries already present in that list. Historical entries left in `by_addr` are discarded. Conversely, an account that exists today but is absent from the historical context can retain its current-state editor entry. The latter direction is established by code inspection, not a separate live example in this audit.

**Required fix:** build the historical account list directly from the replay context and transaction keys, including historical-only accounts and explicit absence, instead of using today's account list as the membership filter. Test closed accounts, recreated accounts, and accounts created after the target.

### P1 — “Matches the real on-chain result” still compares only success/failure

`static/index.html:2467–2469` still uses `d.onchain_success === r.success`. No error identity, logs, compute, balances, or account outputs are compared for this badge. An execution with a different failure reason can therefore be labelled a match. The successful samples above were independently compared more thoroughly; the UI did not make that comparison.

The browser also still displays “any slot in the last 30 days replays exactly” when submitting an empty slot, and the drift notice still promises that later transactions touching newly watched accounts replay exactly. The homepage wording improved, but these statements remain at `static/index.html:1459`, `2275`, and around `2455`.

**Required fix:** label the current comparison as success/failure agreement, or implement and expose a real output comparison. Remove unconditional exactness promises from validation and recorder notices.

### P1 — Complete historical provenance and retention remain unresolved

The new `balance_unproven` set improves the general stream path and is visible in the live different-target result. However, code inspection shows remaining gaps:

1. It is stored only on the current `Replay`. The account written to `LogStore` contains data, owner, and the selected balance, with **no persisted field-level confidence**. The covered-record lookup returns `Recorded` without restoring that uncertainty (`src/scope.rs:1490–1558`). A later read can therefore lose the distinction between historical data and an estimated balance. Not independently triggered as a cold-store lifecycle test here.
2. The closed-account reconstruction branch can fill an unknown lamport balance with a rent estimate and return `Recorded` without setting `balance_unproven` (`src/scope.rs:1358–1409`).
3. Confirmed polling intervals still establish global `covers()` status, not complete finalized per-account write coverage. The two-second poller can miss intermediate changes.
4. `kept_slots` still removes the last version before the retention boundary, including an anchor needed for a quiet account (`src/records.rs:415`).
5. `record` still drops out-of-order historical versions after a newer version exists (`src/records.rs:934`).

Items 3–5 were unchanged by these fixes. The three live requests still report incomplete historical inputs. Universal exactness remains an unmet acceptance condition, independent of the UI improvements.

**Required fix:** carry field-level provenance through storage/reloads, label every estimate, preserve boundary anchors, and establish complete update coverage before making an exactness guarantee.

### P2 — Date parsing still accepts invalid clock values and trailing input

All of these returned HTTP **200** on the updated backend:

| Input | Parsed behavior |
| --- | --- |
| `2026-09-01T25:00:00Z` | Rolls into September 2, 01:00 |
| `2026-09-01T12:60:00Z` | Rolls into 13:00 |
| `2026-09-01T06:30:00+25:00` | Accepts invalid offset |
| `2026-09-01-extra` | Ignores trailing date component |

`parse_time` now validates month/day but still omits hour, minute, second, offset, and full-input validation (`server/src/main.rs:800–873`). The browser picker constrains normal user input; the public API remains affected.

The resolver now exposes `off_by_secs` and fixed the sampled 102-second error. It can still return up to 60 seconds off even though its loop targets two seconds, and the browser does not show the numeric offset. That broader tolerance was found in code, not observed in the successful date samples here.

### P2 — Window enforcement still has cache and unknown-time exceptions

The seconds-based cutoff and own-slot route guard are fixed. But `/analyze_at` and `/replay_at` return cached responses **before** checking the window; a six-hour cached response can outlive the cutoff (`server/src/main.rs:1857`, corresponding branch in `replay_at_handler`). Failed block-time lookups still let the guard pass. These paths were identified by inspection, not by waiting six hours or inducing upstream failures.

**Required fix:** revalidate the requested target on cache hits and define a truthful response when the window cannot be established.

### P2 — CI remains red because formatting fails

`cargo fmt --all --check` fails in four sections of `src/scope.rs` introduced by these fixes. The public [CI run](https://github.com/alizeeshan1234/svmScope/actions/runs/35028106892) also failed its **Format** step. The separate [image workflow](https://github.com/alizeeshan1234/svmScope/actions/runs/35028106856) succeeded, explaining why the deployment could proceed despite failed CI.

**Required fix:** format the changed Rust code and require passing CI before release.

## Local validation

- `cargo test --workspace --offline`: **193 passed**, 3 ignored.
- `cargo test -p svmscope --features single-run-trace --offline --lib --test offline_fixture`: **174 passed**.
- Clippy with `-D warnings`: **passed**.
- Formatting: **failed**, as above.
- Existing three-case certificate probe: all three false-exact sentences suppressed.
- Expanded renderer probe: confirms success/failure-only match wording and inconsistent labels for `Recorded` accounts with unproven balances.

The test counts are unchanged from the first audit. Passing them does not cover the newly identified closed-account membership issue or prove universal historical coverage.

## Evidence

[Re-audit evidence](qa/replay-2026-09-16-reaudit/) includes post-rollout API responses, the separately named pre-rollout date results, replay results, browser observations/screenshots, certificate probes, test/format logs, and public workflow status. The first audit remains unchanged for comparison.

No load/stress test, provider-quota inspection, restart-durability experiment, or exhaustive all-program/30-day coverage validation was performed. The ₹0/$0 constraint remains in force.
