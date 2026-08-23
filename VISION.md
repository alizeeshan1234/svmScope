# svmscope — Vision & Architecture

> A transaction time machine for Solana: replay any transaction, change reality, and
> see exactly where it breaks.

This document describes the full end-to-end system `svmscope` is being built toward.
The current tool (see [README](./README.md)) implements **Stages 1–4** — decode, state
reconstruction, local replay, and the what-if mutation engine. This document describes
the full pipeline and what remains (Stage 5: invariants & generated tests).

---

## The problem

Solana explorers show you *what* a transaction did — a list of instructions, some logs,
a balance table. What they can't do:

- **Explain *why* it failed** in terms you can act on.
- **Let you replay it** against the exact state it ran on.
- **Let you change something** — an oracle price, a signer, an account balance — and
  re-run it to see what would have happened.
- **Turn an incident into a reproducible test** you can fix against.

Developers debugging failures, auditors reproducing exploits, and protocols doing
incident response all need this — and today they each rebuild pieces of it by hand.

`svmscope` is the tool that does it end to end.

---

## The end-to-end pipeline

```
  transaction signature
          │
          ▼
  ┌───────────────────┐
  │ 1. Fetch & Decode │   getTransaction → CPI tree, balance diffs, CU,
  │    (DONE, v0.1)   │   Address Lookup Table resolution
  └───────────────────┘
          │
          ▼
  ┌───────────────────┐
  │ 2. Reconstruct    │   fetch every touched account + program binary,
  │    State          │   rebuild the pre-transaction world
  └───────────────────┘
          │
          ▼
  ┌───────────────────┐
  │ 3. Deterministic  │   load state into an embedded SVM (LiteSVM),
  │    Replay         │   re-execute, confirm identical result
  └───────────────────┘
          │
          ▼
  ┌───────────────────┐
  │ 4. "What-If"      │   mutate reality — oracle price, signer, account,
  │    Mutation       │   time/slot, compute budget — then replay again
  └───────────────────┘
          │
          ▼
  ┌───────────────────┐
  │ 5. Invariants &   │   detect what broke, emit a runnable regression
  │    Test Gen       │   test that reproduces it
  └───────────────────┘
```

Each stage builds on the one before it. You cannot replay a transaction you cannot
fetch and reconstruct — which is why Stage 1 exists first.

---

## The stages in detail

### Stage 1 — Fetch & Decode  ✅
Fetch a transaction by signature and produce a readable report:
- **CPI call tree** from `innerInstructions` + `stackHeight`.
- **Account balance changes** from `pre/postBalances`.
- **Compute units per program** parsed from the logs.
- **Address Lookup Table resolution** so versioned transactions decode correctly.

This is the foundation and the human-readable "what happened" view.

### Stage 2 — Reconstruct State  ✅
To replay a transaction you need the world it ran in:
- Fetch every account it touched (`getMultipleAccounts`) — data, owner, lamports.
- Fetch the **program binaries** for every program it invoked (executable + programdata
  accounts for upgradeable programs).
- Capture the relevant sysvars (clock/slot, rent) and feature set.

The honest hard problem here is **historical** state: an RPC gives you *current* state,
not the state at the transaction's slot. For recent transactions, current ≈ pre-state.
For older ones, this requires archival data or a forked snapshot — and is the deepest
challenge in the whole project.

### Stage 3 — Deterministic Replay  ✅
Load the reconstructed state into an embedded SVM (e.g. [LiteSVM](https://github.com/LiteSVM/litesvm)),
re-execute the transaction, and confirm you get the **same result** — same success or
failure, same logs, same state changes. This is the moment `svmscope` stops being a
decoder and becomes a *simulator*: a transaction you can run, not just read.

### Stage 4 — "What-If" Mutation Engine  ✅
Once you can replay deterministically, you can change one thing and replay again:
- Crash or stale an **oracle price**.
- Remove or swap a **signer**.
- Change an **account balance, owner, or data**.
- Advance **time / slot**.
- Exhaust the **compute budget**.
- Swap a dependency's **program binary** (e.g. test a patch).

This is the core value: understanding not just what happened, but what *would* happen
under different conditions.

### Stage 5 — Invariants & Regression Tests
- Let the user (or the protocol) declare **invariants** — token conservation, authority
  constraints, solvency.
- After a mutation, detect which invariant **broke** and where.
- **Emit a runnable test** (LiteSVM / `solana-program-test`) that reproduces the failure,
  so it can be fixed and guarded against.

---

## Architecture

```
  cli            argument parsing, output formatting
  fetch          RPC client: getTransaction, getMultipleAccounts
  decode         CPI tree, balance diffs, compute, ALT resolution   (Stage 1)
  state          reconstruct accounts + program binaries + sysvars  (Stage 2)
  replay         embedded SVM harness, execute & compare            (Stage 3)
  mutate         state/price/signer/time mutations                  (Stage 4)
  invariants     invariant definitions + checks + test emission     (Stage 5)
```

Today, `cli`, `fetch`, `decode`, `state`, `replay`, and `mutate` all exist (across
`main`, the `cpi_tree` / `diffs` / `compute` / `utils` modules, `state`, and `replay`).
Stage 5 (`invariants`) is the build ahead.

---

## What makes it different

Read-only decoders already exist. The wedge for `svmscope` is **not** decoding — it is
the *integrated end-to-end loop*:

```
  transaction → visual explanation → mutated replay → invariant violation → reproducible test
```

Existing tools expose individual ingredients (forking, in-process SVMs, transaction
parsers). The bet is that no one has combined them into one polished workflow that takes
you from a signature to a reproducible "here's exactly what breaks, and here's the test
that proves it." That workflow is the product.

Honest note: parts of this backend are being built elsewhere (mainnet-forking runtimes,
in-process SVMs). The differentiation has to live in the *workflow and the mutation/
invariant layer*, not in re-implementing an SVM.

---

## Status & roadmap

- [x] **Stage 1 — Fetch & Decode**: CPI tree, balance diffs, CU per program, ALT
      resolution, graceful error handling.
- [x] **Stage 2 — State reconstruction**: fetch accounts + program binaries.
- [x] **Stage 3 — Deterministic replay**: LiteSVM harness, replay & compare.
- [x] **Stage 4 — Mutation engine** (lamports + data): oracle / signer / account / time / compute.
- [ ] **Stage 5 — Invariants & test generation**.

Adjacent quality-of-life: file mode (offline analysis), configurable RPC endpoint,
IDL-based instruction decoding.
