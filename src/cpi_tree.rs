//! The CPI (cross-program invocation) call tree — YOUR first brick.
//!
//! Where the data lives in the getTransaction JSON:
//!
//!   tx["transaction"]["message"]["accountKeys"]
//!        -> array of program/account pubkeys. A `programIdIndex` indexes into this.
//!
//!   tx["transaction"]["message"]["instructions"]
//!        -> the TOP-LEVEL instructions. Each has:
//!             { "programIdIndex": <usize>, "accounts": [..], "data": "..." }
//!
//!   tx["meta"]["innerInstructions"]
//!        -> the CPIs. An array of:
//!             { "index": <which top-level ix these belong to>,
//!               "instructions": [
//!                   { "programIdIndex": <usize>, "stackHeight": <u64>, ... },
//!                   ...
//!               ] }
//!        `stackHeight` is the depth: 2 = called by the top-level ix,
//!        3 = called by a stackHeight-2 ix, etc.
//!
//! GOAL — print something like:
//!
//!   #0  <program pubkey>
//!       └─ [2] <program pubkey>
//!          └─ [3] <program pubkey>
//!   #1  <program pubkey>
//!
//! HOW TO START (don't do it all at once):
//!   1. Get `accountKeys` as a slice, and write a helper:
//!         fn program_id(keys: &[Value], idx: usize) -> &str
//!   2. Loop over the top-level `instructions`, print each with `#i` and its program id.
//!   3. Then, for each top-level ix `i`, find the matching `innerInstructions`
//!      entry (where "index" == i) and print its inner instructions,
//!      indented by `stackHeight`.
//!
//! Tips:
//!   - `value.get("meta")` returns `Option<&Value>`. Use `?`-style handling or
//!     `.and_then(...)` / `if let Some(..)`.
//!   - `arr.as_array()` gives `Option<&Vec<Value>>`.
//!   - `v["programIdIndex"].as_u64()` reads a number as `Option<u64>`.
//!   - `v["accountKeys"][i].as_str()` reads a string.

use std::println;

use serde_json::Value;

/// Build and print the CPI call tree for a transaction.
pub fn print_cpi_tree(tx: &Value) {
    // TODO(you): implement this. Start tiny — just print the top-level
    // instructions and their program ids. Get that running against
    // sample_tx.json, THEN add the inner instructions indented by stackHeight.

    let instructions = tx["transaction"]["message"]["instructions"]
        .as_array()
        .expect("instructions should be an array");

    let account_keys = tx["transaction"]["message"]["accountKeys"]
        .as_array()
        .expect("accountKeys should be an array");

    for (index, ix) in instructions.iter().enumerate() {
        let program_id_index = ix["programIdIndex"].as_u64().unwrap() as usize;   
        let program_id = account_keys[program_id_index].as_str().unwrap();
        println!("#{}  {}", index, program_id);   

        let inner_ix = tx["meta"]["innerInstructions"]
            .as_array()
            .expect("Inner instructions should be an array");

        let my_group = inner_ix.iter().find(|group| group["index"].as_u64() == Some(index as u64));

        if let Some(my_group) = my_group {
            let inners = my_group["instructions"].as_array().unwrap();

            for inner in inners {
                let pid_index = inner["programIdIndex"].as_u64().unwrap() as usize;
                let pid = account_keys[pid_index].as_str().unwrap();
                let stack_height = inner["stackHeight"].as_u64().unwrap();
                let indent = "    ".repeat((stack_height - 2) as usize);
                println!("{}└─ [{}] {}", indent, stack_height, pid);
            }

        }

    }
}
