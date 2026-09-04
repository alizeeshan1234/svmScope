# svmscope debug — the step debugger for Solana transactions

## One line

Paste a signature or a local transaction and step through it instruction by instruction, CPI by CPI, with decoded account state before and after every step, the failing step pinpointed with its error decoded, and the ability to change any value and re-run.

Solana has never had this. Developers debug with `msg!` logs and explorer log dumps. Tenderly has had it on Ethereum since 2019.

## What the user sees

1. **Input.** A mainnet signature, a base64 transaction from a wallet, or a fixture from CI. Same three inputs svmscope already accepts.
2. **Timeline (left).** A tree: top-level instructions as rows, CPIs nested under them. Each row shows program name, instruction name from IDL, compute used, and a red mark on the step that failed. Keyboard `j`/`k` walks it.
3. **Step detail (center).** For the selected step: the decoded instruction with named arguments, the accounts it touched with their roles, and for each touched account a before/after diff as named fields (using the existing IDL and SPL layout decoders), not hex.
4. **Logs (right).** The program logs, scrolled and highlighted to the selected step.
5. **Error banner.** When the transaction fails: which step, which program, the error name and docs string from the IDL, and the plain-English explanation svmscope already generates.
6. **Edit and re-run.** Any field in the before-state of any step is editable. Change it, press run, and the whole trace re-executes with a diff against the previous trace: which steps changed outcome, which accounts ended differently.
7. **Share.** Every trace has a URL. Opening it shows exactly this view, no re-fetch, because the trace is stored as a fixture v2 (already exists) with the trace attached.

## Architecture

### The one new primitive: `Replay::trace()`

```rust
pub struct Trace { pub steps: Vec<Step>, pub result: ReplayResult }
pub struct Step {
    pub path: String,            // "2", "2.0", "2.0.1"  (top-level index, then CPI indices)
    pub depth: u8,
    pub program: String,
    pub instruction: Option<DecodedInstruction>,   // name + args via IDL (exists)
    pub accounts: Vec<AccountRole>,                // address, signer, writable, name from IDL (exists)
    pub cu_consumed: Option<u64>,
    pub logs: Range<usize>,                        // slice into result.logs
    pub diffs: Vec<AccountDiff>,                   // before/after for accounts this step changed (exists: decode_diffs)
    pub error: Option<StepError>,                  // decoded via explain_error (exists)
    pub return_data: Option<Vec<u8>>,
}
```

**Phase A, prefix replay (top-level steps, days 1–3).** For k in 0..n, build the message with instructions `0..=k`, run it through the existing `ctx.run_with_diff`, and diff account state between run k and run k−1. That gives exact before/after state per top-level instruction with zero runtime changes. CPI rows come from parsing `Program X invoke [depth]` / `Program X consumed` / `Program X success|failed` lines in the logs (`preflight::compute_breakdown` already does the CU part), attached under their parent with their logs and compute, but without per-CPI state.

**Phase B, runtime hook (CPI-level state, days 11–14).** LiteSVM executes through `solana-program-runtime`'s `InvokeContext`. A post-instruction callback there captures the account set after every CPI, not just top level. Do it as a small feature-gated patch to LiteSVM and open the PR upstream (its author follows you; svmscope is his crate's biggest public consumer). Until it merges, vendor the fork behind a cargo feature.

### Edit and re-run

Mutations already exist (`Mutation::lamports/data/owner/patch/field`). "Edit at step k" in v1 means: apply the mutation to the initial state and re-trace from step 0. That's semantically clean and needs no new machinery. True "resume from step k with a changed value" comes with Phase B.

`Trace::diff(&other) -> TraceDiff` lists steps whose outcome, error, or diffs changed. The existing `minimize_mutations` and `find_threshold` plug straight into this: "what's the smallest change that makes this pass" and "at what price does this liquidation flip".

### Surfaces

- **Crate:** `replay.trace()`, `Trace::diff`, serialization into fixture v2.
- **CLI:** `svmscope debug <sig>` prints the tree with a failing step highlighted; `--json` for tooling.
- **Server:** `POST /trace` returning the JSON; `GET /debug/<sig>` serving a stored trace.
- **Web:** `/debug/<sig>` view on svmscope.vercel.app. Vanilla JS like the rest of the UI. Three panels, keyboard navigation, edit fields, run button, share button. OG image per trace so links unfurl on X with the failing step's name.

## Milestones

| Days | Milestone | Done when |
|---|---|---|
| 1–3 | **M1 Trace primitive** | `svmscope debug <sig>` prints a correct tree with per-step diffs for three real transactions: a Jupiter swap, a failed liquidation, a pump.fun buy that reverted. Tests on fixtures. |
| 4–7 | **M2 Debugger UI** | `/debug/<sig>` live. Timeline, detail, logs, error banner. Deep links. |
| 8–10 | **M3 Edit and re-run** | Change a field, re-run, see the trace diff. Share links with OG cards. |
| 11–14 | **M4 CPI-level state** | LiteSVM hook behind a feature flag, PR opened upstream. Inner steps show real before/after. Local base64 transactions accepted. |
| 14 | **Launch** | 20-second video: failing transaction, click, the bad field lights up. Thread. |

## Launch plan

- Video first, text second. The whole pitch is one clip.
- Post from your account, tag nothing in the first post. Reply to yourself with the crate, the repo, and three example links people can click.
- Ask Jonas (Solana Foundation DevRel, follows you) to quote it. Ask Aursen (LiteSVM author, follows you) to look at the hook PR the same day.
- Post the three example links as answers on Solana Stack Exchange questions about "custom program error" debugging. That's where developers search.
- Docs page: "Debugging a failed Solana transaction in 30 seconds."

## What counts as working

- Week 1: 500 distinct traces opened, 3 people you don't know post a trace link.
- Month 1: one protocol team says they use it in incident response; LiteSVM hook merged or in review.
- Month 3: it is the link people paste when asking "why did this fail" in Solana dev Discords.

## Non-goals for v1

- BPF instruction-level stepping. Instruction and CPI granularity is what developers need.
- Historical state at arbitrary slots for old transactions. Same archive constraint as before; recent transactions and locally built ones are the target.
- Private or unconfirmed transactions beyond what the wallet base64 path already handles.

## Risks

- **Prefix replay cost.** n instructions means n replays. Transactions have 1–10 instructions; LiteSVM runs each in milliseconds. Fine.
- **LiteSVM fork drift.** Keep the patch tiny and feature-gated; upstream it early.
- **UI scope creep.** Three panels and a run button. Nothing else until launch.

## Why this and not the exploit-autopsy tool

Autopsy is the same engine pointed at a small audience on rare days. The debugger is the same engine pointed at every developer on a bad day, which is most days. Ship the debugger; the autopsy becomes a use of it.
