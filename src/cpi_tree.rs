//! Prints the CPI (cross-program invocation) call tree of a transaction.

use crate::utils::resolve_account_keys;
use serde_json::Value;

/// Build and print the CPI call tree for a transaction.
pub fn print_cpi_tree(tx: &Value) {
    let instructions = tx["transaction"]["message"]["instructions"]
        .as_array()
        .expect("instructions should be an array");

    // Full account list, including accounts loaded from Address Lookup Tables.
    let account_keys = resolve_account_keys(tx);

    for (index, ix) in instructions.iter().enumerate() {
        let program_id_index = ix["programIdIndex"].as_u64().unwrap() as usize;
        let program_id = &account_keys[program_id_index];
        println!("#{}  {}", index, program_id);

        let inner_ix = tx["meta"]["innerInstructions"]
            .as_array()
            .expect("innerInstructions should be an array");

        let my_group = inner_ix
            .iter()
            .find(|group| group["index"].as_u64() == Some(index as u64));

        // No inner group just means this instruction made no CPIs — that's
        // normal, so we simply print nothing extra and move on.
        if let Some(my_group) = my_group {
            let inners = my_group["instructions"].as_array().unwrap();
            for inner in inners {
                let pid_index = inner["programIdIndex"].as_u64().unwrap() as usize;
                let pid = &account_keys[pid_index];
                let stack_height = inner["stackHeight"].as_u64().unwrap();
                let indent = "    ".repeat((stack_height - 2) as usize);
                println!("{}└─ [{}] {}", indent, stack_height, pid);
            }
        }
    }
}
