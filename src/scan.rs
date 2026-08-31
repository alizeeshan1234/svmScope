//! Auto breaking-point scan — the deterministic "agent" that finds *every* way a
//! transaction can be broken, instead of making you probe one knob at a time.
//!
//! It reads each touched account's decoded fields (built-in SPL layouts + the
//! program's on-chain IDL) and tests two kinds of break:
//! - **numeric thresholds** — binary-search a balance/reserve/amount for the
//!   value where the outcome flips ("drops below N");
//! - **discrete breaks** — a single toggle that flips it, like a token account
//!   being **frozen**.
//!
//! The replay's world is fetched once; every candidate is a *local* LiteSVM run,
//! so a whole sweep costs no extra RPC. Free — pure current-state replay.

use crate::error::Result;
use crate::replay::Mutation;
use crate::scope::Scope;
use crate::search::search_threshold;
use serde::Serialize;

/// One way the transaction can be broken.
#[derive(Debug, Clone, Serialize)]
pub struct BreakingPoint {
    /// The account whose change breaks it.
    pub account: String,
    /// The field or knob — `SOL balance`, `amount`, `state`…
    pub field: String,
    /// A human description of the break: "drops below 12,985", "is frozen".
    pub condition: String,
    /// The field's current value where relevant (0 otherwise).
    pub current: u64,
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
    /// Also test discrete breaks (e.g. freezing a token account).
    pub include_discrete: bool,
}

impl Default for ScanOptions {
    fn default() -> Self {
        ScanOptions {
            max_accounts: 24,
            max_fields_per_account: 12,
            include_lamports: true,
            include_discrete: true,
        }
    }
}

/// Scan a transaction for every threshold and toggle that flips its outcome.
pub fn scan_breaking_points(
    scope: &Scope,
    signature: &str,
    accounts: &[String],
    opts: ScanOptions,
) -> Result<Vec<BreakingPoint>> {
    // Fetch the world once; every probe below is a local, RPC-free replay.
    let replay = scope.replay(signature)?;
    let baseline = replay.run()?.result.success;

    let mut out: Vec<BreakingPoint> = Vec::new();

    for account in accounts.iter().take(opts.max_accounts) {
        let Ok(info) = scope.decode_account(account, None) else {
            continue;
        };
        if info.executable {
            continue;
        }

        // 1) SOL balance threshold. A flip at ≤ 1 lamport just means "must exist".
        if opts.include_lamports && info.lamports > 0 {
            let hi = info.lamports.saturating_mul(2);
            let acct = account.clone();
            if let Some(th) = search_threshold(0, hi, |v| {
                Ok(replay
                    .simulate(&[Mutation::lamports(acct.clone(), v)])?
                    .result
                    .success)
            })? {
                if th.flips_at > 1 {
                    out.push(BreakingPoint {
                        account: account.clone(),
                        field: "SOL balance".into(),
                        condition: threshold_phrase(th.flips_at, !th.low_success),
                        current: info.lamports,
                    });
                }
            }
        }

        let Some(decoded) = info.decoded else {
            continue;
        };

        // 2) Numeric u64 field thresholds.
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
                continue;
            }
            tried += 1;
            let (acct, off) = (account.clone(), f.offset);
            if let Some(th) = search_threshold(0, current.saturating_mul(2), |v| {
                Ok(replay
                    .simulate(&[Mutation::patch(acct.clone(), off, v.to_le_bytes().to_vec())])?
                    .result
                    .success)
            })? {
                out.push(BreakingPoint {
                    account: account.clone(),
                    field: f.name.clone(),
                    condition: threshold_phrase(th.flips_at, !th.low_success),
                    current,
                });
            }
        }

        // 3) Discrete breaks — toggles the numeric search can't express. For an
        // SPL token account: freezing it (state → 2) blocks any transfer in or
        // out, so if the transaction touches it, that's a break.
        if opts.include_discrete && decoded.type_name == "SPL Token Account" {
            if let Some(state_off) = decoded.fields.iter().find(|f| f.name == "state").map(|f| f.offset) {
                let already_frozen = decoded
                    .fields
                    .iter()
                    .find(|f| f.name == "state")
                    .map(|f| f.value == "2")
                    .unwrap_or(false);
                if !already_frozen && flips(&replay, baseline, account, state_off, &[2])? {
                    out.push(BreakingPoint {
                        account: account.clone(),
                        field: "state".into(),
                        condition: "is frozen".into(),
                        current: 0,
                    });
                }
            }
        }
    }

    Ok(out)
}

/// Whether patching `bytes` at `offset` in `account` flips the outcome vs `baseline`.
fn flips(
    replay: &crate::Replay,
    baseline: bool,
    account: &str,
    offset: usize,
    bytes: &[u8],
) -> Result<bool> {
    let out = replay
        .simulate(&[Mutation::patch(account.to_string(), offset, bytes.to_vec())])?
        .result
        .success;
    Ok(out != baseline)
}

/// Phrase a numeric threshold: `breaks_below` ⇒ "drops below N", else "reaches N or above".
fn threshold_phrase(flips_at: u64, breaks_below: bool) -> String {
    if breaks_below {
        format!("drops below {flips_at}")
    } else {
        format!("reaches {flips_at} or above")
    }
}
