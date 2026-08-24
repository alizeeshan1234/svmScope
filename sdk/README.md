# svmscope (TypeScript SDK)

TypeScript client for **svmscope** — the Solana transaction simulation layer.
Decode, replay, mutate, assert, and **pre-flight simulate** any transaction against
the real on-chain programs, from one dependency.

```bash
npm install svmscope
```

```ts
import { Svmscope } from "svmscope";

const svm = new Svmscope("http://127.0.0.1:3000"); // your svmscope API URL
```

## Preview a transaction before signing (wallets)

`preflight` simulates an **unsigned** transaction against current on-chain state —
what will happen if the user signs and sends it. Unlike the RPC's
`simulateTransaction`, this decodes accounts and lets you run what-ifs.

```ts
import { VersionedTransaction } from "@solana/web3.js";

const tx: VersionedTransaction = await buildSwap(); // your unsigned tx
const result = await svm.preflight(tx.serialize());

if (!result.success) {
  // e.g. "InstructionError(2, Custom(6024))" — show the user before they sign
  showWarning(`This transaction would fail: ${result.error}`);
} else {
  showDetails(`Uses ${result.compute_units.toLocaleString()} compute units`);
}
```

## Debug a failed transaction

```ts
const analysis = await svm.analyze(signature);

// Named programs + IDL-decoded accounts, no raw bytes:
for (const acc of analysis.accounts) {
  if (acc.decoded) console.log(acc.decoded.type_name, acc.decoded.fields);
}

// Run the local replay to see the real error + logs:
const replay = await svm.replay(signature);
console.log(replay.success ? "ok" : replay.error);
```

## Ask "what if?"

```ts
import { setTokenAmount } from "svmscope";

// What if this pool reserve were empty?
const after = await svm.simulate(signature, [setTokenAmount(poolReserve, 0)]);
console.log(after.success ? "still works" : `reverts: ${after.error}`);
```

## Assert edge cases (scenario tests)

```ts
const outcomes = await svm.runSuite(signature, [
  { name: "baseline succeeds", expect: "success" },
  {
    name: "draining the reserve reverts",
    expect: "revert",
    contains: "6025",
    mutations: [setTokenAmount(poolReserve, 0)],
    asserts: [{ address: poolReserve, kind: "u64", offset: 64, op: "==", value: 0 }],
  },
]);

const passed = outcomes.filter((o) => o.pass).length;
console.log(`${passed}/${outcomes.length} passed`);
```

## Mutation helpers

```ts
import { setLamports, setTokenAmount, patchU64, patchBytes } from "svmscope";

setLamports(address, 0);              // zero an account's SOL
setTokenAmount(tokenAccount, 1_000n); // SPL amount (u64 @ 64)
patchU64(address, 36, 500n);          // a u64 field at a known offset
patchBytes(address, 0, "01");         // raw bytes
```

## API

| Method | Description |
| --- | --- |
| `analyze(signature)` | Decode a landed tx (CPI tree, balances, compute, IDL accounts). |
| `replay(signature)` | Re-execute it locally against reconstructed pre-state. |
| `simulate(signature, mutations)` | Replay with what-if account mutations. |
| `runSuite(signature, scenarios)` | Run a scenario suite with outcome + state assertions. |
| `preflight(tx, mutations?)` | Simulate an **unsigned** tx before sending. |
| `freeze(signature)` | Capture a deterministic, offline fixture. |

Every method returns typed results (`ReplayResult`, `Analysis`, `ScenarioOutcome[]`,
`Fixture`). Zero runtime dependencies — uses the platform `fetch`.
