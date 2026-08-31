//! Auto breaking-point scan — the deterministic "agent" that finds *every* way a
//! transaction can be broken, across all its accounts.
//!
//! For each account it tests the full break surface:
//! - **closed** — remove the account (drop it to 0 lamports);
//! - **frozen** — flip a token account's state to frozen;
//! - **authority changed** — swap a token account's owner/authority;
//! - **balance thresholds** — binary-search SOL, and each numeric field, for the
//!   value where the outcome flips.
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
    /// The field or knob — `SOL balance`, `existence`, `state`, `authority`, `reserveA`…
    pub field: String,
    /// A human description: "is closed", "is frozen", "changes", "drops below 12,985".
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
}

impl Default for ScanOptions {
    fn default() -> Self {
        ScanOptions {
            max_accounts: 32,
            max_fields_per_account: 12,
        }
    }
}

/// Well-known infrastructure (programs, sysvars): always present, never the
/// interesting break surface — skip so the list stays meaningful.
fn is_infra(address: &str) -> bool {
    address.starts_with("Sysvar")
        || matches!(
            address,
            "11111111111111111111111111111111"
                | "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA"
                | "TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb"
                | "ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL"
                | "ComputeBudget111111111111111111111111111111"
                | "NativeLoader1111111111111111111111111111111"
                | "BPFLoader2111111111111111111111111111111111"
                | "BPFLoaderUpgradeab1e11111111111111111111111"
        )
}

/// Scan a transaction for every way it can be broken.
pub fn scan_breaking_points(
    scope: &Scope,
    signature: &str,
    accounts: &[String],
    opts: ScanOptions,
) -> Result<Vec<BreakingPoint>> {
    let replay = scope.replay(signature)?;
    let baseline = replay.run()?.result.success;

    let mut out: Vec<BreakingPoint> = Vec::new();

    for account in accounts.iter().take(opts.max_accounts) {
        if is_infra(account) {
            continue;
        }
        let Ok(info) = scope.decode_account(account, None) else {
            continue;
        };
        if info.executable {
            continue;
        }

        // 1) SOL balance — closed (needs to exist) vs a real balance floor.
        if info.lamports > 0 {
            let hi = info.lamports.saturating_mul(2);
            let acct = account.clone();
            if let Some(th) = search_threshold(0, hi, |v| {
                Ok(replay
                    .simulate(&[Mutation::lamports(acct.clone(), v)])?
                    .result
                    .success)
            })? {
                let (field, condition) = if th.flips_at <= 1 {
                    ("existence".to_string(), "is closed".to_string())
                } else {
                    (
                        "SOL balance".to_string(),
                        threshold_phrase(th.flips_at, !th.low_success),
                    )
                };
                out.push(bp(account, field, condition, info.lamports));
            }
        }

        let Some(decoded) = info.decoded else {
            continue;
        };
        let is_token = decoded.type_name == "SPL Token Account";

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
                out.push(bp(
                    account,
                    f.name.clone(),
                    threshold_phrase(th.flips_at, !th.low_success),
                    current,
                ));
            }
        }

        // 3) Discrete token-account breaks: frozen, authority changed.
        if is_token {
            // Frozen — state → 2 blocks any transfer in or out.
            if let Some(off) = field_offset(&decoded, "state") {
                let frozen = field_value(&decoded, "state") == Some("2");
                if !frozen && flips(&replay, baseline, account, off, vec![2])? {
                    out.push(bp(account, "state", "is frozen", 0));
                }
            }
            // Authority changed — swap the owner/authority pubkey (offset 32).
            if let Some(off) = field_offset(&decoded, "owner") {
                let other = [0x11u8; 32];
                if flips(&replay, baseline, account, off, other.to_vec())? {
                    out.push(bp(account, "authority", "changes", 0));
                }
            }
        }
    }

    Ok(out)
}

fn bp(account: &str, field: impl Into<String>, condition: impl Into<String>, current: u64) -> BreakingPoint {
    BreakingPoint {
        account: account.to_string(),
        field: field.into(),
        condition: condition.into(),
        current,
    }
}

fn field_offset(d: &crate::decode::DecodedAccount, name: &str) -> Option<usize> {
    d.fields.iter().find(|f| f.name == name).map(|f| f.offset)
}

fn field_value<'a>(d: &'a crate::decode::DecodedAccount, name: &str) -> Option<&'a str> {
    d.fields.iter().find(|f| f.name == name).map(|f| f.value.as_str())
}

/// Whether patching `bytes` at `offset` flips the outcome vs `baseline`.
fn flips(
    replay: &crate::Replay,
    baseline: bool,
    account: &str,
    offset: usize,
    bytes: Vec<u8>,
) -> Result<bool> {
    let out = replay
        .simulate(&[Mutation::patch(account.to_string(), offset, bytes)])?
        .result
        .success;
    Ok(out != baseline)
}

/// Phrase a numeric threshold.
fn threshold_phrase(flips_at: u64, breaks_below: bool) -> String {
    if breaks_below {
        format!("drops below {flips_at}")
    } else {
        format!("reaches {flips_at} or above")
    }
}
