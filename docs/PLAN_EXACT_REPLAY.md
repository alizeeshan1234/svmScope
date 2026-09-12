# svmscope: exact replay of any recent transaction, free

Plan written 2026-09-11. This is the product, the architecture, the work, and
the order. It replaces the LiteSVM and Surfpool contribution plans.

## The one line

Any Solana transaction from the last 30 days, replayed exactly at its own
slot or at any other slot in that window, stepped through, profiled, and
frozen as a fixture. Free. No archive subscription.

## Why this and why now

- Surfpool added `--fork-slot` (PR 800) and it requires Alchemy's paid Account
  Archive. That is the only historical-state option in the Foundation's own
  tooling, and it costs money per call.
- In the Foundation's developer Discord this week: two teams tried to sell
  historical state and made nothing; delorean died at ~$100k/yr of
  infrastructure; the people who need it are searchers ("why did my
  integration fail") and auditors ("replay the hack"); the consensus scope was
  "last 30 days, per transaction, save fixtures". That is a spec.
- Two unfilled Foundation RFPs cover this: Historical State Verification (up
  to $275k, no winner) and Post-Deployment Monitoring (no winner).
- svmscope already has every building block: current-state replay with
  balance rewind, a fidelity certificate, write-history reconstruction
  (`reconstruct.rs`, written, not wired in), fixtures, the trace, the
  profiler, a hosted engine with caches. What is missing is connecting them
  and measuring the result.

## What we are building

The entry point is `replay_at(signature, slot)`: rebuild the world as it was
at `slot`, set the clock to `slot`, run the transaction. `replay_at_slot` is
the special case `slot == the transaction's own slot`, where the transaction's
metadata also gives exact balances for free. Three tiers of state, chosen
automatically, labelled honestly:

| Tier | Source | Cost | Today |
|---|---|---|---|
| Current + rewind | accounts as of now, balances rewound from the tx metadata | one RPC round trip | exists (`Fidelity::Reconstructed`) |
| **Reconstructed** | drifting accounts rebuilt from their write history, replayed forward in LiteSVM | seconds of CPU, free history from Helius or Old Faithful | **exists in `reconstruct.rs`, not connected** |
| Exact | archive `getAccountInfo` at the slot | paid (Alchemy, Flux) | exists, and per-request keys landed today |

The product is the middle tier, made automatic and fast enough, plus a cache so
the hosted engine never reconstructs the same transaction twice.

## Architecture: four paths per account, tried in order

`replay_at(signature, slot)` rebuilds the world as of the target slot `N`.
For every account the transaction touches, the first path that applies wins:

1. **Metadata.** Token accounts, when `N` is the transaction's own slot: the
   balance is read from the transaction's own pre/post token balances.
   Exact, free. Most written accounts in a swap are these.
2. **Unchanged.** One backward lookup, `Scope::last_write_slot`: if the
   account's newest write is at or before `N`, its current bytes are the bytes
   at `N`. Most accounts stop here. If its first write is after `N`, it did
   not exist: absent from the world, said in the certificate.
3. **Recorded.** For accounts anyone has asked about before, every update
   since is recorded as it happens (RPC `accountSubscribe` or a Geyser
   stream; address, slot, bytes, delta-compressed). "Bytes at `N`" is a
   lookup. Hot pools and markets end up here quickly; a seed list of the top
   ones is recorded from day one. Tens of gigabytes for a few hundred
   accounts over 30 days. The only stored data in the design.
4. **Reconstructed.** Everything else. Search backward from `N` to the last
   write and to the nearest known state (a recording, a cached result); replay
   forward only the writes in that gap, using `reconstruct_account` with a
   starting state. First, test whether the last write is a full overwrite
   (replay it with two different inputs; identical outputs mean it needs no
   history): then it is one replay. Writes cannot be undone, so the replay is
   always forward; the search is what runs backward.

Reconstruction details (path 4): each old write is replayed via
`scope.replay(sig)` with balances rewound from that write's own metadata and
the account's running bytes injected (`Mutation::data`, `Mutation::lamports`),
then read out with `account_after`. Data-bearing co-accounts a write depends
on are reconstructed first, to a capped depth. Helius's
`getTransactionsForAddress` fetches a slot range of writes with contents in
one call as a second `LedgerSource`; plain RPC and Old Faithful stay as the
generic path. A budget per account and per transaction names what it skipped.

**Programs.** Current ELF unless upgraded after `N`; then the certificate
says "upgraded since", and an archive key, if given, supplies the old binary
(it cannot be rebuilt: the upgrade's buffer is closed).

**Assembly.** Build the current-state context; run the four paths; set clock,
epoch and timestamp to `N`; replay. At the transaction's own slot, verify
against the on-chain outcome and compute units. Any account that blew its
budget falls back to the archive at `N` when a key is present. Return the
replay with a per-account certificate: `metadata | current | recorded |
reconstructed { writes, from } | archive | approximate { reason }`. Never a
silent claim. `replay_at_slot(sig)` is `replay_at(sig, own_slot)`.

**Cache.** Worlds as fixtures by (signature, slot); account states by
(address, slot); recordings by address. Evicted after 30 days unless frozen.
Repeat requests in milliseconds.

**Measurement.** Weekly sweep: 200 transactions, top 20 programs, last 30
days, replayed at their own slot on the free path, compared to the on-chain
outcome; where an Alchemy key exists in CI, bytes compared to archive bytes
per account. Exact-match rate by program, median seconds, median writes,
budget hits, published in the README.

**Hosted.** `GET /replay_at/{sig}?slot=`, cached; UI shows the tier and the
certificate; `svmscope freeze <sig> --at <slot>`.

## What stays out

- Anything per-slot or whole-chain. No snapshots of everything, no 50 TB.
- Selling it. Public good, Foundation grant, sponsorship for the RPC.
- Contributing it to Surfpool. It lives here; Surfpool can depend on the
  crate if they want the free tier.

## The work, in order

Each milestone ends with something measurable. Ali writes the code; Claude
maps, reviews, and explains the modules involved beforehand.

| # | Milestone | Modules | Done when |
|---|---|---|---|
| 0 | Read `reconstruct.rs`, `replay_at`, `replay_at_slot` until you can explain them | — | explanation written in your own words |
| 1 | Paths 1 and 2: classification and drift detection against any slot, with tests | `scope.rs`, `fidelity.rs` | correct per-account path for 20 sampled txs at 3 slots each, validated once against archive bytes |
| 2 | Path 4 wired into `replay_at`: reconstruction from creation or from a hand-made recording for two pools | `scope.rs`, `reconstruct.rs`, `replay.rs` | a Raydium swap that drifts today reproduces exactly at its own slot; at a slot three days later it runs with a full certificate |
| 3 | Path 3: automatic recording of asked-about and seed accounts; Helius range fetch | new `recorder.rs`, server job | the same swap under 5 seconds with no manual setup |
| 4 | Overwrite shortcut, co-account recursion, budgets with reasons, upgrade detection | `reconstruct.rs`, `fidelity.rs` | no silent approximations in the sweep output |
| 5 | Caches with 30-day eviction | `server/src/main.rs` | repeat requests under 100 ms |
| 6 | Weekly sweep in CI, results in the README | `examples/fidelity_sweep.rs`, workflows | a number to quote |
| 7 | UI certificate, CLI `--archive` and `--at`, freeze from the site | `web/index.html`, `src/main.rs` | a visitor sees why a replay is exact or not |
| 8 | Write-up and grant application citing the two RFPs and the sweep | `docs/` | submitted |

Milestones 1 to 4 are the engine. Nothing else matters until 2 is done,
because 2 is the claim.

## What to say publicly, and when

Nothing until milestone 2 passes on the transactions that drift today. Then
one post: the Raydium swap that was wrong yesterday and is exact today, with
the certificate, and the line at the top of this document. After milestone 6,
the number. The Discord thread gets one message at that point, not before,
and it links to a result.

---

# Revision 2, 2026-09-12: the zero-cost 30-day design

Written after the first live results. Constraints from Ali: no money, ever;
Helius credits are money; the promise is 30 days, not forever.

## What the live runs established

- The forward loop rebuilds cold accounts exactly (Squads proposal: 2
  writes, 3 s; metadata edition: 1 write). It cannot rebuild hot accounts
  (thousands of writes) or accounts read by every transaction of a busy
  program (finding the last write means fetching thousands of reads).
- A write whose result depends on another account's state at that moment
  (a multisig counter, a mint) fails to replay against current state. It is
  now counted as skipped and the result labelled approximate; the earlier
  "exact" on such accounts was a false positive and is gone.
- Recording works: 150 s of polling a Raydium pool's 47 accounts, then a
  swap from inside the window replayed to within 5 CU of the chain while the
  current tier failed it. 15 of 28 drifting accounts were exact from
  recordings. The remaining 13 were accounts not yet watched.
- Naive storage (JSON, base64, every version) grows ~5 GB/day per pool set.
- Clock anchoring alone fixes a class of failures (time checks); the current
  tier anchors to now, the historical tier to the slot.

## Goals

1. Any transaction from the last 30 days, replayed at its own slot or any
   slot in that window, exact for every account the recorder has covered,
   honestly labelled for everything else. No archive.
2. Zero recurring cost: public RPC by default, Helius only when a caller
   supplies it, storage inside free tiers.
3. Nothing presented as exact that is not.

## Architecture

### RPC policy

- Recorder: public mainnet node. One `getMultipleAccounts` per 100 accounts
  per round; at a 2 s interval that is well inside public limits. Zero
  credits.
- Reconstruction walks and transaction fetches: the scope's RPC, which on
  the hosted engine is whatever `SVMSCOPE_RPC_URL` is. Default it to the
  public node; a caller's `rpc` (vetted) or an operator's Helius key are
  opt-in accelerators.
- `getTransactionsForAddress` (Helius-only, 100 txs with contents per
  call): optional `LedgerSource` used only when the ledger URL is a Helius
  endpoint; turns "find the newest write of a constantly-read account" from
  thousands of calls into tens.

### Storage: two tiers, diffs, compression

- Version = (slot, bytes) stored as a binary diff against the previous
  version, zstd-compressed. A pool change is a few hundred bytes.
- Dense tier, last 24 h: every change. Inside it, "no newer version at or
  before the target" means unchanged: exact by lookup, no walk.
- Sparse tier, days 2 to 30: one version per 30 s. A replay in this range
  starts from the nearest kept version and replays at most 30 s of writes
  forward on the public node. Exact, slower.
- Thinning job: after 24 h keep one version per 30 s; after 30 days drop.
- Sizing: ~30 MB/day dense and ~1 MB/day sparse per hot account. One hundred
  hot accounts for 30 days: roughly 6 GB in total.
- Backends behind the `StateStore` trait: `DirStore` (local dev), Render's
  free ephemeral disk (dense tier; survives until redeploy), Cloudflare R2
  free tier (10 GB, no egress fees; sparse tier as hourly batched blobs to
  stay inside the free operation count). R2 is what makes 30 days a
  promise that survives a redeploy.

### Coverage semantics

- `covers(slot)` is true only inside the dense tier with a polling round on
  both sides of the slot no further apart than `MAX_COVERAGE_GAP`.
- In the sparse tier, coverage is never claimed: the nearest kept version is
  a floor and the walk from it decides.
- A recorder outage leaves a hole; nothing across it is claimed exact.

### The watched set

- Seeds: the top pools, markets, vaults and authorities of the top programs
  by volume, from a checked-in list.
- Every account a replay had to rebuild is added, so the set grows with what
  people ask about.
- Cap the set to what the free tiers hold; evict least-recently-asked.

### Replay path, unchanged in shape

Per account, in order: infra/program/signer skip; token account at own slot
from metadata; unchanged since target from `last_write_slot`; recorded and
covered; forward replay from the nearest recorded version or from creation
within budgets; else current bytes labelled approximate; archive key, if the
caller has one, for anything that fell through. Provenance per account in
the certificate; a `window` line stating the recorder's coverage start.

## Work, in order

| # | Piece | Where | Done when |
|---|---|---|---|
| A | Binary diff + zstd versions; thinning; retention | `records.rs` | a day of one pool's set is tens of MB, tests cover diff round trips and thinning |
| B | Public-node defaults; Helius opt-in; recorder RPC separate from replay RPC | `scope.rs`, server env | a full replay and a recording round spend zero credits by default |
| C | Helius range fetch as an optional `LedgerSource` | `reconstruct.rs` | newest-write detection on the Raydium authority in under 20 calls |
| D | R2 backend (S3-compatible, hourly blobs) behind `StateStore` | new `records/r2.rs` | a version written on Render is readable after a redeploy |
| E | Recorder on Render with seeds; coverage line in the certificate | server env, `fidelity.rs` | the hosted `/replay_at_slot` shows "exact from recording" for a seed pool swap |
| F | Sweep against the covered window; number in the README | `examples/fidelity_sweep.rs`, CI | a reproduction rate to quote |
| G | UI certificate and window display; CLI `--at`, `--archive` | `web/`, `src/main.rs` | a visitor sees why a replay is exact or not |
| H | Write-up for Surfpool and Alchemy; grant application citing the RFPs | `docs/` | sent |

A and B are engine work and come first. E is the moment the 30-day window
starts filling; everything after it gets better with time.

## What is not promised

- Exact hot-account state for a slot before the recorder covered it, without
  an archive key. Labelled approximate.
- A change and change-back inside one polling interval is invisible; the
  interval bounds the blind spot.
- Anything older than 30 days.
