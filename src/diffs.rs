//! Prints the lamport balance change for every account a transaction touched.

use crate::utils::resolve_account_keys;
use serde_json::Value;

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

        // delta == 0 just means this account didn't change — skip it.
        if delta != 0 {
            println!("{:<44} {:+} lamports", &account_keys[i], delta);
        }
    }
}
