//! Prints the lamport balance change for every account a transaction touched.

use crate::utils::resolve_account_keys;
use serde_json::Value;

#[derive(serde::Serialize)]
pub struct BalanceChange {
    pub address: String,
    pub delta: i64
}

/// A change in an SPL token account's balance, in raw base units (client
/// divides by 10^decimals for display).
#[derive(serde::Serialize)]
pub struct TokenChange {
    pub address: String,
    pub owner: String,
    pub mint: String,
    pub decimals: u8,
    /// Signed change in raw base units, as a string (can exceed i64).
    pub delta_raw: String,
    /// Post-transaction balance in raw base units, as a string.
    pub post_raw: String,
}

/// SPL token balance changes, computed from meta.pre/postTokenBalances.
///
/// The two arrays are keyed by `accountIndex`; we pair them up, compute the
/// signed delta per account, and drop anything that didn't move.
pub fn token_diffs(tx: &Value) -> Vec<TokenChange> {
    let account_keys = resolve_account_keys(tx);
    let empty = vec![];
    let pre = tx["meta"]["preTokenBalances"].as_array().unwrap_or(&empty);
    let post = tx["meta"]["postTokenBalances"].as_array().unwrap_or(&empty);

    // index -> raw amount (i128) for pre and post
    let raw = |entry: &Value| -> i128 {
        entry["uiTokenAmount"]["amount"]
            .as_str()
            .and_then(|s| s.parse::<i128>().ok())
            .unwrap_or(0)
    };
    let idx = |entry: &Value| entry["accountIndex"].as_u64().unwrap_or(u64::MAX) as usize;

    let mut pre_map: std::collections::HashMap<usize, i128> = std::collections::HashMap::new();
    for e in pre {
        pre_map.insert(idx(e), raw(e));
    }

    let mut out: Vec<TokenChange> = Vec::new();
    let mut seen: std::collections::HashSet<usize> = std::collections::HashSet::new();

    for e in post {
        let i = idx(e);
        seen.insert(i);
        let post_raw = raw(e);
        let pre_raw = pre_map.get(&i).copied().unwrap_or(0);
        let delta = post_raw - pre_raw;
        if delta == 0 {
            continue;
        }
        out.push(TokenChange {
            address: account_keys.get(i).cloned().unwrap_or_default(),
            owner: e["owner"].as_str().unwrap_or_default().to_string(),
            mint: e["mint"].as_str().unwrap_or_default().to_string(),
            decimals: e["uiTokenAmount"]["decimals"].as_u64().unwrap_or(0) as u8,
            delta_raw: delta.to_string(),
            post_raw: post_raw.to_string(),
        });
    }
    // Accounts present in pre but not post (fully drained/closed).
    for e in pre {
        let i = idx(e);
        if seen.contains(&i) {
            continue;
        }
        let pre_raw = raw(e);
        if pre_raw == 0 {
            continue;
        }
        out.push(TokenChange {
            address: account_keys.get(i).cloned().unwrap_or_default(),
            owner: e["owner"].as_str().unwrap_or_default().to_string(),
            mint: e["mint"].as_str().unwrap_or_default().to_string(),
            decimals: e["uiTokenAmount"]["decimals"].as_u64().unwrap_or(0) as u8,
            delta_raw: (-pre_raw).to_string(),
            post_raw: "0".to_string(),
        });
    }
    out
}

/// Print the lamport balance change for every account that changed.
pub fn account_diffs(tx: &Value) -> Vec<BalanceChange> {
    let pre = tx["meta"]["preBalances"]
        .as_array()
        .expect("preBalances should be an array");

    let post = tx["meta"]["postBalances"]
        .as_array()
        .expect("postBalances should be an array");

    let account_keys = resolve_account_keys(tx);

    let mut balance_change: Vec<BalanceChange> = Vec::new();

    for i in 0..pre.len() {
        let pre_balance = pre[i].as_u64().expect("preBalance should be u64");
        let post_balance = post[i].as_u64().expect("postBalance should be u64");
        let delta = post_balance as i64 - pre_balance as i64;

        if delta != 0 {
            balance_change.push(BalanceChange {
                address: account_keys[i].clone(),
                delta: delta
            });
        }
    }

    balance_change
}
