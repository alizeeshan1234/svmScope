//! Prints the lamport balance change for every account a transaction touched.

use serde_json::Value;
use crate::utils::resolve_account_keys;

/// Print the lamport balance change for every account that changed.
pub fn print_account_diffs(tx: &Value) {
    let pre = tx["meta"]["preBalances"]
        .as_array()
        .expect("preBalances should be an array");

    let post = tx["meta"]["postBalances"]
        .as_array()
        .expect("postBalances should be an array");

    let account_keys = resolve_account_keys(tx);

    for i in 0..pre.len() {
        let pre_balance = pre[i].as_u64().expect("preBalance should be u64");
        let post_balance = post[i].as_u64().expect("postBalance should be u64");
        let delta = post_balance as i64 - pre_balance as i64;

        if delta != 0 {
            let account_key = account_keys[i].as_str();
            println!("{:<44} {:+} lamports", account_key, delta);
        }
    }
}
