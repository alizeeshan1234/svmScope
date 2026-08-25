/**
 * svmscope — TypeScript client for the Solana transaction simulation layer.
 *
 * Decode, replay, mutate, assert, and pre-flight simulate any Solana transaction,
 * backed by real on-chain programs running in an embedded SVM.
 *
 * @example
 * ```ts
 * import { Svmscope } from "svmscope";
 * const svm = new Svmscope("http://127.0.0.1:3000");
 *
 * // Preview an unsigned transaction before signing (wallet use case):
 * const result = await svm.preflight(unsignedTx.serialize());
 * if (!result.success) console.warn("would fail:", result.error);
 *
 * // Debug a landed transaction:
 * const analysis = await svm.analyze(signature);
 * ```
 */

// ---------------------------------------------------------------------------
// Types (mirror the server's JSON payloads)
// ---------------------------------------------------------------------------

/** The result of executing a transaction in the embedded SVM. */
export interface ReplayResult {
  success: boolean;
  /** The failure reason, e.g. `InstructionError(2, Custom(6024))`, or null on success. */
  error: string | null;
  /** Program log lines, exactly as the runtime emitted them. */
  logs: string[];
  compute_units: number;
}

/** One decoded field of an account (SPL layout or the program's Anchor IDL). */
export interface Field {
  name: string;
  offset: number;
  type: string;
  size: number;
  /** Current value, formatted (decimal for ints, base58 for pubkeys). */
  value: string;
  /** Whether this field can be edited in a what-if. */
  editable: boolean;
  note?: string;
}

export interface DecodedAccount {
  /** e.g. "SPL Token Account", "SPL Mint", or an IDL type like "Pool" / "LbPair". */
  type_name: string;
  fields: Field[];
}

export interface AccountInfo {
  address: string;
  owner: string;
  lamports: number;
  executable: boolean;
  data_len: number;
  /** Present when the account's layout is recognized (SPL or a fetched IDL). */
  decoded?: DecodedAccount;
}

export interface CpiEntry {
  index: number;
  program: string;
  stack_height: number;
}
export interface BalanceChange {
  address: string;
  delta: number;
}
export interface TokenChange {
  address: string;
  owner: string;
  mint: string;
  decimals: number;
  /** Signed change in raw base units, as a string (can exceed 2^53). */
  delta_raw: string;
  post_raw: string;
}
export interface CuUsage {
  program: string;
  cu: number;
}
export interface Overview {
  success: boolean;
  fee: number;
  slot?: number;
  compute_units?: number;
  top_programs: string[];
}

/** The full decode of a transaction (`analyze`). `replay` is opt-in, so null here. */
export interface Analysis {
  /** Resolved signature (useful when the input was an address). */
  signature: string;
  overview: Overview;
  cpi_tree: CpiEntry[];
  balance_change: BalanceChange[];
  token_change: TokenChange[];
  compute: CuUsage[];
  replay: ReplayResult | null;
  accounts: AccountInfo[];
}

/** A what-if change to apply to an account before replaying. */
export type Mutation =
  | { kind: "lamports"; address: string; lamports: number }
  | { kind: "data"; address: string; offset: number; bytes_hex: string };

/** A post-replay state assertion. */
export interface Assert {
  address: string;
  /**
   * What to check (default "u64"):
   * - `"u64"` — little-endian u64 at `offset`.
   * - `"lamports"` — the account's lamports.
   * - `"token_amount"` — SPL token amount (u64 @ 64).
   * - `"lamports_delta"` — change in lamports (post − pre); `value` may be negative.
   * - `"token_delta"` — change in SPL token amount (post − pre); `value` may be negative.
   * - `"field"` — the named `field` of the account's decoded layout (SPL layouts,
   *   or the owner program's IDL), e.g. `field: "pool.reserveA"` — no offsets.
   *   Matched by exact name or final dot-segment; integer/bool fields only.
   * - `"field_delta"` — change in the named `field` (post − pre); may be negative.
   */
  kind?: "u64" | "lamports" | "token_amount" | "lamports_delta" | "token_delta" | "field" | "field_delta";
  offset?: number;
  /** Field name for the `field` / `field_delta` kinds. */
  field?: string;
  /** Comparison operator; default "==". */
  op?: "==" | "!=" | "<" | "<=" | ">" | ">=";
  value: number;
}

/** One test scenario: mutations + the outcome (and optional state) it asserts. */
export interface Scenario {
  name: string;
  /** "success", "revert" (or "fail"), or "any". Default "any". */
  expect?: "success" | "revert" | "fail" | "any";
  /** When reverting, require the error/logs to contain this text. */
  contains?: string;
  mutations?: Mutation[];
  asserts?: Assert[];
}

export interface AssertOutcome {
  description: string;
  pass: boolean;
}
export interface ScenarioOutcome {
  name: string;
  expect: string;
  pass: boolean;
  actual: ReplayResult;
  asserts: AssertOutcome[];
}

/** One entry in an address's recent transaction history. */
export interface SigInfo {
  signature: string;
  slot?: number;
  /** Whether the transaction failed on-chain. */
  err: boolean;
  /** Unix timestamp, when reported. */
  block_time?: number;
}


/** A failure explained in human terms, resolved from the program's IDL when possible. */
export interface Explanation {
  /** e.g. "Overflow" or "Slippage exceeded". */
  title: string;
  /** The human message, e.g. "counter overflowed". */
  detail: string;
  /** Which program raised it, when attributable. */
  program?: string;
  /** The raw error, e.g. "InstructionError(0, Custom(6000))". */
  raw: string;
}

/** One field that changed in an account, before → after. */
export interface FieldDiff {
  name: string;
  type: string;
  before: string;
  after: string;
}

/** An account the transaction changed. */
export interface AccountDiff {
  address: string;
  owner: string;
  lamports_before: number;
  lamports_after: number;
  fields: FieldDiff[];
  /** Data changed but the layout wasn't decodable. */
  raw_data_changed: boolean;
}

/**
 * Warp the SVM clock so time-gated logic can be tested without waiting —
 * unstake after an epoch, claim after a vesting cliff, withdraw after a cooldown.
 * Relative fields add to the transaction's clock; `at*` fields set it outright.
 */
export interface TimeTravel {
  /** Jump forward this many epochs (staking). */
  epochs?: number;
  /** Jump forward this many slots. */
  slots?: number;
  /** Jump forward this many seconds (cliffs, cooldowns). */
  seconds?: number;
  at_unix_timestamp?: number;
  at_slot?: number;
  at_epoch?: number;
}

/** A simulation plus everything a developer needs to act on it. */
export interface SimulationReport {
  replay: ReplayResult;
  /** Where the clock was warped to, when time travel was requested. */
  clock?: string;
  /** Present when the transaction failed. */
  explain?: Explanation;
  diffs: AccountDiff[];
}

/** One instruction a program exposes (from its on-chain IDL). */
export interface IdlInstruction {
  name: string;
  discriminator: number[];
  docs: string[];
  accounts: { name: string; writable: boolean; signer: boolean; pda: boolean; address?: string }[];
  args: { name: string; type: string }[];
}

/** A self-contained, deterministic snapshot for offline replay. */
export interface Fixture {
  signature: string;
  captured_slot?: number;
  tx_b64: string;
  entries: unknown[];
}

// ---------------------------------------------------------------------------
// Mutation / assertion builders (ergonomic helpers)
// ---------------------------------------------------------------------------

/** Set an account's lamports. */
export const setLamports = (address: string, lamports: number | bigint): Mutation => ({
  kind: "lamports",
  address,
  lamports: Number(lamports),
});

/** Overwrite raw bytes at an offset (hex, e.g. `"00e1f505"`). */
export const patchBytes = (address: string, offset: number, hex: string): Mutation => ({
  kind: "data",
  address,
  offset,
  bytes_hex: hex.replace(/^0x/, ""),
});

/** Write a u64 (little-endian) at an offset — e.g. an SPL token amount at 64. */
export const patchU64 = (address: string, offset: number, value: number | bigint): Mutation => ({
  kind: "data",
  address,
  offset,
  bytes_hex: u64le(value),
});

/** Convenience: set an SPL token account's `amount` (u64 at offset 64). */
export const setTokenAmount = (tokenAccount: string, amount: number | bigint): Mutation =>
  patchU64(tokenAccount, 64, amount);

function u64le(value: number | bigint): string {
  let v = BigInt(value);
  if (v < 0n || v > 0xffffffffffffffffn) throw new RangeError("value out of u64 range");
  let hex = "";
  for (let i = 0; i < 8; i++) {
    hex += (v & 0xffn).toString(16).padStart(2, "0");
    v >>= 8n;
  }
  return hex;
}

// ---------------------------------------------------------------------------
// Client
// ---------------------------------------------------------------------------

export class SvmscopeError extends Error {
  constructor(public status: number, message: string) {
    super(message);
    this.name = "SvmscopeError";
  }
}

/** Which cluster to run against (default "mainnet"). */
export type Cluster = "mainnet" | "devnet" | "testnet" | "localnet";

export interface SvmscopeOptions {
  /** Cluster to target; one instance serves all of them. Default "mainnet". */
  cluster?: Cluster;
  /** Explicit RPC URL, overrides `cluster`. */
  rpc?: string;
  /** Custom fetch implementation (defaults to global `fetch`). */
  fetchImpl?: typeof fetch;
}

export class Svmscope {
  private readonly cluster?: Cluster;
  private readonly rpc?: string;
  private readonly fetchImpl: typeof fetch;

  /**
   * @param baseUrl svmscope API base URL (default `http://127.0.0.1:3000`).
   * @param options cluster / rpc / fetch. Per-call cluster can also be passed to
   *   each method's last argument.
   */
  constructor(
    private readonly baseUrl: string = "http://127.0.0.1:3000",
    options: SvmscopeOptions = {},
  ) {
    this.baseUrl = baseUrl.replace(/\/$/, "");
    this.cluster = options.cluster;
    this.rpc = options.rpc;
    this.fetchImpl = options.fetchImpl ?? fetch;
  }

  /** Query string carrying the cluster/rpc for GET requests. */
  private clusterQuery(over?: Cluster): string {
    const cluster = over ?? this.cluster;
    const params = new URLSearchParams();
    if (cluster) params.set("cluster", cluster);
    if (this.rpc) params.set("rpc", this.rpc);
    const s = params.toString();
    return s ? `?${s}` : "";
  }
  /** cluster/rpc fields to merge into POST bodies. */
  private clusterBody(over?: Cluster): { cluster?: Cluster; rpc?: string } {
    return { cluster: over ?? this.cluster, rpc: this.rpc };
  }

  /** Recent transactions for an account/program (newest first) — explorer-style. */
  signatures(address: string, cluster?: Cluster): Promise<SigInfo[]> {
    return this.get(`/signatures/${encodeURIComponent(address)}` + this.clusterQuery(cluster));
  }

  /** Decode a landed transaction — CPI tree, balances, compute, IDL-decoded accounts. */
  analyze(signature: string, cluster?: Cluster): Promise<Analysis> {
    return this.get(`/analyze/${encodeURIComponent(signature)}` + this.clusterQuery(cluster));
  }

  /** Re-execute a landed transaction locally against reconstructed pre-state. */
  replay(signature: string, cluster?: Cluster): Promise<ReplayResult> {
    return this.get(`/replay/${encodeURIComponent(signature)}` + this.clusterQuery(cluster));
  }

  /** Replay a landed transaction with what-if account mutations. */
  simulate(
    signature: string,
    mutations: Mutation[],
    opts: { timeTravel?: TimeTravel; cluster?: Cluster } = {},
  ): Promise<ReplayResult> {
    return this.post("/simulate", {
      signature,
      mutations,
      time_travel: opts.timeTravel ?? {},
      ...this.clusterBody(opts.cluster),
    });
  }

  /** Run a suite of test scenarios (outcome + state assertions) against a transaction. */
  runSuite(
    signature: string,
    scenarios: Scenario[],
    opts: { timeTravel?: TimeTravel; cluster?: Cluster } = {},
  ): Promise<ScenarioOutcome[]> {
    return this.post("/simulate_suite", {
      signature,
      scenarios,
      time_travel: opts.timeTravel ?? {},
      ...this.clusterBody(opts.cluster),
    });
  }

  /**
   * Pre-flight simulate an **unsigned** transaction against current state, before
   * it's sent — "what will this do if I sign and send it now?".
   *
   * @param transaction base64 string, or the raw serialized bytes (e.g.
   *   `versionedTx.serialize()` from `@solana/web3.js`).
   * @param mutations optional what-if edits to preview against edited state.
   */
  preflight(
    transaction: string | Uint8Array,
    mutations: Mutation[] = [],
    cluster?: Cluster,
  ): Promise<ReplayResult> {
    const b64 = typeof transaction === "string" ? transaction : toBase64(transaction);
    return this.post("/preflight", { transaction: b64, mutations, ...this.clusterBody(cluster) });
  }


  /**
   * Pre-flight simulate an unsigned transaction and get the full report:
   * outcome, a human-readable failure reason, and the field-level account diff.
   */
  preflightReport(
    transaction: string | Uint8Array,
    mutations: Mutation[] = [],
    opts: { timeTravel?: TimeTravel; cluster?: Cluster } = {},
  ): Promise<SimulationReport> {
    const b64 = typeof transaction === "string" ? transaction : toBase64(transaction);
    return this.post("/preflight_report", {
      transaction: b64, mutations,
      time_travel: opts.timeTravel ?? {},
      ...this.clusterBody(opts.cluster),
    });
  }

  /** Replay a landed transaction (optionally mutated) with explanation + diff. */
  replayReport(
    signature: string,
    mutations: Mutation[] = [],
    opts: { timeTravel?: TimeTravel; cluster?: Cluster } = {},
  ): Promise<SimulationReport> {
    return this.post("/replay_report", {
      signature, mutations,
      time_travel: opts.timeTravel ?? {},
      ...this.clusterBody(opts.cluster),
    });
  }

  /** The instructions a program exposes, from its on-chain IDL. */
  instructions(programId: string, cluster?: Cluster): Promise<IdlInstruction[]> {
    return this.get(`/instructions/${encodeURIComponent(programId)}` + this.clusterQuery(cluster));
  }

  /** Capture a self-contained fixture for deterministic, offline replay. */
  freeze(signature: string, cluster?: Cluster): Promise<Fixture> {
    return this.get(`/freeze/${encodeURIComponent(signature)}` + this.clusterQuery(cluster));
  }

  // --- internals ---
  private async get<T>(path: string): Promise<T> {
    return this.request<T>(path, { method: "GET" });
  }
  private async post<T>(path: string, body: unknown): Promise<T> {
    return this.request<T>(path, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(body),
    });
  }
  private async request<T>(path: string, init: RequestInit): Promise<T> {
    const res = await this.fetchImpl(this.baseUrl + path, init);
    if (!res.ok) {
      const text = await res.text().catch(() => res.statusText);
      throw new SvmscopeError(res.status, text || `HTTP ${res.status}`);
    }
    return res.json() as Promise<T>;
  }
}

/** Base64-encode bytes in both Node and the browser. */
function toBase64(bytes: Uint8Array): string {
  if (typeof Buffer !== "undefined") return Buffer.from(bytes).toString("base64");
  let binary = "";
  for (const b of bytes) binary += String.fromCharCode(b);
  return btoa(binary);
}

export default Svmscope;
