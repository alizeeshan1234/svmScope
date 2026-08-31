//! Auto breaking-point scan — the deterministic "agent" that finds *every*
//! numeric threshold that flips a transaction's outcome, instead of making you
//! search one knob at a time.
//!
//! It reads each touched account's decoded fields (via built-in layouts and the
//! program's on-chain IDL), then binary-searches every numeric field — plus each
//! account's SOL balance — for the point where the replay flips success ↔ revert.
//! The replay's world is fetched once; every candidate is a *local* LiteSVM run,
//! so a whole sweep costs no extra RPC. Free — pure current-state replay.

use crate::error::Result;
use crate::replay::Mutation;
use crate::scope::Scope;
use crate::search::search_threshold;
use serde::Serialize;

/// One discovered breaking point: a value at which the transaction's outcome
/// flips when a specific account field (or its SOL balance) crosses it.
#[derive(Debug, Clone, Serialize)]
pub struct BreakingPoint {
    /// The account whose field breaks it.
    pub account: String,
    /// The field name — a decoded field like `reserveA`, or `SOL balance`.
    pub field: String,
    /// The field's current value (where the transaction actually succeeds).
    pub current: u64,
    /// The value the field flips the outcome at.
    pub flips_at: u64,
    /// `true` when the transaction *breaks* (reverts) as the value drops **below**
    /// `flips_at`; `false` when it breaks at/**above** `flips_at`.
    pub breaks_below: bool,
    /// How many local replays the search for this point ran.
    pub evaluations: u32,
}

/// How much of the transaction to sweep.
#[derive(Debug, Clone, Copy)]
pub struct ScanOptions {
    /// Maximum accounts to scan (touched-account order).
    pub max_accounts: usize,
    /// Maximum decoded numeric fields to try per account.
    pub max_fields_per_account: usize,
    /// Also probe each account's SOL balance.
    pub include_lamports: bool,
}

impl Default for ScanOptions {
    fn default() -> Self {
        ScanOptions {
            max_accounts: 24,
            max_fields_per_account: 12,
            include_lamports: true,
        }
    }
}

/// Scan a transaction for every numeric threshold that flips its outcome.
/// Returns the breaking points found, most-recently-discovered order.
pub fn scan_breaking_points(
    scope: &Scope,
    signature: &str,
    accounts: &[String],
    opts: ScanOptions,
) -> Result<Vec<BreakingPoint>> {
    // Fetch the world once; every probe below is a local, RPC-free replay.
    let replay = scope.replay(signature)?;

    let mut out: Vec<BreakingPoint> = Vec::new();

    for account in accounts.iter().take(opts.max_accounts) {
        // Decode the account for its numeric fields and current lamports; skip
        // anything we can't read (closed, or a program with no data fields).
        let Ok(info) = scope.decode_account(account, None) else {
            continue;
        };
        if info.executable {
            continue;
        }

        // 1) The account's SOL balance. A flip at ≤ 1 lamport just means "the
        // account must exist" — noise; only a real balance requirement counts.
        if opts.include_lamports && info.lamports > 0 {
            let hi = info.lamports.saturating_mul(2);
            if let Some(bp) = probe(&replay, account, "SOL balance", info.lamports, 0, hi, |v| {
                vec![Mutation::lamports(account.clone(), v)]
            })? {
                if bp.flips_at > 1 {
                    out.push(bp);
                }
            }
        }

        // 2) Each editable u64 field of the decoded layout.
        let Some(decoded) = info.decoded else {
            continue;
        };
        let mut tried = 0usize;
        for f in &decoded.fields {
            if tried >= opts.max_fields_per_account {
                break;
            }
            if f.ty != "u64" || !f.editable {
                continue;
            }
            let Ok(current) = f.value.parse::<u64>() else {
                continue;
            };
            if current == 0 {
                continue; // no meaningful range to search below
            }
            tried += 1;
            let offset = f.offset;
            let hi = current.saturating_mul(2);
            if let Some(bp) = probe(&replay, account, &f.name, current, 0, hi, move |v| {
                vec![Mutation::patch(account.clone(), offset, v.to_le_bytes().to_vec())]
            })? {
                out.push(bp);
            }
        }
    }

    Ok(out)
}

/// Binary-search one knob and, if the outcome flips in range, describe it.
fn probe(
    replay: &crate::Replay,
    account: &str,
    field: &str,
    current: u64,
    lo: u64,
    hi: u64,
    mutate: impl Fn(u64) -> Vec<Mutation>,
) -> Result<Option<BreakingPoint>> {
    if hi <= lo {
        return Ok(None);
    }
    let th = search_threshold(lo, hi, |v| Ok(replay.simulate(&mutate(v))?.result.success))?;
    Ok(th.map(|t| BreakingPoint {
        account: account.to_string(),
        field: field.to_string(),
        current,
        flips_at: t.flips_at,
        // The tx breaks (reverts) toward whichever bound it failed at. It failed
        // at the low bound ⇒ it breaks as the value drops below `flips_at`.
        breaks_below: !t.low_success,
        evaluations: t.evaluations,
    }))
}
