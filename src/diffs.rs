//! Prints the lamport balance change for every account a transaction touched.

use crate::utils::resolve_account_keys;
use serde_json::Value;

pub struct BalanceChange {
    pub address: String,
    pub delta: i64
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
