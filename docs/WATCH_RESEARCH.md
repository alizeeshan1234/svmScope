# Post-deployment monitoring on Solana: what exists, what is missing, what svmscope can be

Research notes, 2026-09-11. For the `svmscope watch` decision and the grant application.

## 1. The Foundation's ask

RFP "Post-Deployment Monitoring Tooling", posted by pkxro (Solana Foundation) on 2024-02-07.
https://forum.solana.com/t/post-deployment-monitoring-tooling/1031

Problem statement, in their words: the ecosystem lacks post-deployment monitoring
tools that give program developers "realtime and actionable insights". Developers
need continuous observability to address active threats and to see meaningful
program data: real-time filtering and analysis of program updates, accounts, and
instruction calls.

Three suggested shapes, applicant's choice, deliberately non-prescriptive:

1. Observability tools
2. Live fuzzing tools
3. Off-chain alerting and dispatcher tools

Funding: USD-equivalent locked SOL, milestone-based, amount set per proposal.
Application deadline was 2024-02-29; the grants programme itself is rolling and
the RFP database is still linked from the thread. Thread replies: 0xmulch (an
open-source monitoring project, considered Colosseum), SendBlocks (asked which
of the three mattered most; answer: all three are real gaps). No winner or
shipped solution is recorded in the thread.

Related unfilled RFP, same forum: "Solana Historical State Verification Tool"
(2024-11, up to $275k, two applicants, no announced result). A tool that
re-executes a transaction with the runtime and feature set active at its slot
and verifies the result. svmscope's `replay_at_slot` plus feature toggles is
the front half.
https://forum.solana.com/t/solana-historical-state-verification-tool/2249

## 2. What exists today

| Tool | What it is | Status | Gap it leaves |
|---|---|---|---|
| Sec3 WatchTower | Threat monitor for deployed programs: built-in monitors (abnormal transfers, rug pulls, flash loans, fake accounts) plus "continuously-learned, auto-tuned invariants" and custom cross-transaction rules. Announced 2022-09. | Paid, pilot-only at launch. No longer listed as a product on sec3.dev; monitoring is bundled into audit and "SecLaunch" engagements. Not self-serve, not open source, no DSL. | A developer cannot sign up, write a rule about their own program's state, and get paged. |
| Hexagate (Chainalysis) | Real-time threat intel and pre-sign simulation, ecosystem-wide. | Enterprise, EVM-first. | Not a program developer's tool. |
| Forta | Decentralised detection bot network. | EVM. | No Solana runtime awareness. |
| txscope.com | Pre-signing threat report for multisig transactions: durable nonces, authority transfers, oracle quality, withdrawal-guard changes. Free, no login. | Live. | Pre-sign only. Looks at the proposed payload, not at what lands on chain; nothing after the signature. |
| hasip-timurtas/solana-watchtower | Open-source Rust monitor: WebSocket/Geyser subscriber, `Rule` trait in Rust, Telegram/Slack. | 4 stars, last commit 2025-06-25, two days after creation. Abandoned. | Rules are over raw events (a transfer above X). No account state, no replay, no invariants over decoded fields. |
| Helius / Triton webhooks, Dialect | Event delivery. | Live, widely used. | Plumbing. You still have to write the detection. |
| Validator monitoring (solana-mission-control etc.) | Node operator metrics. | Live. | Different problem. |

The pattern: every serious thing is a paid security firm's bundled service, and
every open-source thing is an event filter with no notion of program state.
Nobody offers a self-serve, open-source monitor where a program developer
writes invariants over their own decoded account fields and gets an alert with
the exact instruction that broke one.

## 3. What would have caught Drift

Drift Protocol, 2026-04-01, about $285M. Sequence (BlockSec, TxScope, Chainalysis):

- 16:05:19 UTC, TX1: a pre-signed durable-nonce transaction transfers admin
  authority to a fresh wallet.
- 16:05:39 UTC, TX2: one six-instruction transaction lists a fabricated token
  (CVT) as collateral and raises withdrawal safety limits by 20x to 100,000x
  across five markets.
- 16:06:09 to 16:06:19 UTC: 31 withdrawals drain JLP ($155.6M), USDC ($66.4M),
  SOL ($10.45M) and more.

Three invariants that already exist in svmscope's `Invariant` module fire on
each step:

- `authority_unchanged(state, "admin")` fires at TX1, 50 seconds before the
  first withdrawal.
- `field_constant(market, "withdraw_guard")` or `monotonic` fires at TX2, 30
  seconds before.
- `max_token_loss(vault, limit)` over a window fires on the first withdrawal
  transaction, with 30 more to go.

Each alert links to a trace of the instruction that changed the field, with the
before and after values decoded through Drift's IDL. That is the demo: backfill
that hour, watch three invariants fire in order, click each one.

Sources: https://blocksec.com/blog/drift-protocol-incident-multisig-governance-compromise-via-durable-nonce-exploitation ,
https://txscope.com/blog/drift-exploit , https://www.chainalysis.com/blog/lessons-from-the-drift-hack/

## 4. What svmscope already has

- The check vocabulary: `Check`, `Scenario`, `Invariant` (`authority_unchanged`,
  `no_lamport_loss`, `max_lamport_loss`, `no_token_loss`, `max_token_loss`,
  `monotonic`, `field_constant`, `field_equals`) and the JSON form in `spec`.
- Field-level decoding by IDL name, so a rule is written as `market.withdraw_guard`
  rather than a byte offset.
- The trace, so an alert can say which instruction, which CPI, before and after.
- Time travel and replay at slot, so a rule can be evaluated against history.
- A hosted engine with a world cache, rate limiting, and 22 API routes.

## 5. What `svmscope watch` adds

**Input.** A program id (or a list of accounts) and a rules file: the existing
`spec` JSON, plus a window clause for rate-style rules (amount per hour).

**Ingest.** Subscribe to every transaction touching the program as it lands
(WebSocket `logsSubscribe` first, Geyser gRPC later for teams that have it).
Bounded fetch of the full transaction, same shape as the Axiom-style indexer.

**Evaluate.** For each transaction, decode the accounts it wrote (pre and post
from the transaction metadata; the replay engine is not needed for most rules,
which keeps it cheap) and evaluate every rule. Rules that need execution detail
(which instruction changed the field) run a trace on demand.

**Alert.** Rule name, program, signature, the field, before and after, the
instruction, a trace link. Delivered to a webhook, Telegram, Slack, or an HTTP
stream. Batched and rate-limited.

**Backfill.** The same rules over a slot range, so "has this ever happened" is
one command and the Drift demo is `svmscope watch --backfill`.

**Respond** (later). A rule can call a webhook that pauses the protocol or
triggers a multisig proposal. That is the circuit breaker, off-chain, driven by
the same invariants that gate it on-chain.

Open source: the crate, the CLI, the rule format, the evaluator. Hosted: the
always-on watcher with delivery, history and a dashboard, per program, paid.

## 6. Why this is defensible

- Rules are over decoded program state, not raw events. That needs the IDL
  layer, the decoder and the trace, which nobody else ships open source.
- The same rule runs in `cargo test` on a fixture and in production on live
  transactions. Write the invariant once. No other tool connects the test suite
  to monitoring.
- Every alert is explainable to the exact instruction, because the engine can
  replay it. WatchTower's learned invariants are opaque by design.
- It answers a Foundation RFP with no winner, in the RFP's own terms, from a
  tool that already exists and is already published.

## 7. Risks

- Rate limits and cost on the ingest side for busy programs. Mitigation: BYOK
  RPC, Geyser for paying teams, and rules that need no replay for the common
  case.
- False positives make people mute alerts. Mitigation: windowed rules, severity,
  and a "what changed" payload that lets a human dismiss in one glance.
- Sec3 could productise WatchTower again. They have not in four years, and it
  would still be closed and bundled.
- It is a security product, and you do not want a security job. This is a
  developer tool that happens to prevent losses, and the pitch stays on the
  developer side: your invariants, your alerts, your trace.

## 8. First milestone

`svmscope watch --program <id> --rules rules.json --backfill <from>..<to>` over
the Drift hour, printing three alerts with trace links. Everything it needs
exists except the ingest loop, the windowed rule, and the alert formatter. That
is the demo, the first grant milestone, and the first post.
