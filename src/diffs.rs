//! Account balance diffs — brick #2.
//!
//! Every transaction records each account's lamport balance BEFORE and AFTER
//! execution. Comparing them shows exactly who paid, who got paid, and how much.
//!
//! Where the data lives:
//!   tx["meta"]["preBalances"]                       -> array of u64 lamports, BEFORE
//!   tx["meta"]["postBalances"]                       -> array of u64 lamports, AFTER
//!   tx["transaction"]["message"]["accountKeys"]      -> the account for each index
//!
//! All three arrays are PARALLEL: index `i` in every array refers to the same
//! account. So account_keys[i] changed from preBalances[i] to postBalances[i].
//!
//! GOAL — print only the accounts that changed, e.g.:
//!
//!   Payer1111...   -5,000,005 lamports
//!   UserToken...   +1,000,000 lamports
//!   PoolToken...   -1,000,000 lamports
//!
//! HINTS:
//!   - Loop by index: `for i in 0..pre.len() { ... }`  (or use `.zip()` if you like).
//!   - Read a lamport value: `pre[i].as_u64().unwrap()`.
//!   - The change can be NEGATIVE, and u64 can't be negative, so compute the
//!     delta as i64:   `let delta = post as i64 - pre as i64;`
//!   - Skip accounts where `delta == 0` (they didn't change).
//!   - (Optional polish) 1 SOL = 1_000_000_000 lamports, if you want to show SOL.

use serde_json::Value;

/// Print the lamport balance change for every account that changed.
pub fn print_account_diffs(tx: &Value) {

    let pre = tx["meta"]["preBalances"]
        .as_array()
        .expect("preBalances should be an array");

    let post = tx["meta"]["postBalances"]
        .as_array()
        .expect("postBalances should be an array");

    let account_keys = tx["transaction"]["message"]["accountKeys"]
        .as_array()
        .expect("accountKeys should be an array");

    for i in 0..pre.len() {
        let pre_balance = pre[i].as_u64().expect("preBalance should be u64");
        let post_balance = post[i].as_u64().expect("postBalance should be u64");
        let delta = post_balance as i64 - pre_balance as i64;

        if delta != 0 {
            let account_key = account_keys[i].as_str().expect("accountKey should be str");
            println!("{:<44} {:+} lamports", account_key, delta);
        }
    }

}
