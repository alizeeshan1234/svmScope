//! Prints the CPI (cross-program invocation) call tree of a transaction.

use serde_json::Value;
use crate::utils::resolve_account_keys;

/// Build and print the CPI call tree for a transaction.
pub fn print_cpi_tree(tx: &Value) {
    let instructions = tx["transaction"]["message"]["instructions"]
        .as_array()
        .expect("instructions should be an array");

    // Full account list = static keys + accounts loaded from Address Lookup Tables.
    // A `programIdIndex` can point past the static keys into the loaded ones,
    // in the order: static, then loaded-writable, then loaded-readonly.
    let mut account_keys = resolve_account_keys(tx);

    for k in tx["transaction"]["message"]["accountKeys"]
        .as_array()
        .unwrap()
    {
        account_keys.push(k.as_str().unwrap().into());
    }
    if let Some(w) = tx["meta"]["loadedAddresses"]["writable"].as_array() {
        for k in w {
            account_keys.push(k.as_str().unwrap().into());
        }
    }
    if let Some(r) = tx["meta"]["loadedAddresses"]["readonly"].as_array() {
        for k in r {
            account_keys.push(k.as_str().unwrap().into());
        }
    }

    for (index, ix) in instructions.iter().enumerate() {
        let program_id_index = ix["programIdIndex"].as_u64().unwrap() as usize;
        let program_id = account_keys[program_id_index].as_str();
        println!("#{}  {}", index, program_id);

        let inner_ix = tx["meta"]["innerInstructions"]
            .as_array()
            .expect("innerInstructions should be an array");

        let my_group = inner_ix
            .iter()
            .find(|group| group["index"].as_u64() == Some(index as u64));

        if let Some(my_group) = my_group {
            let inners = my_group["instructions"].as_array().unwrap();
            for inner in inners {
                let pid_index = inner["programIdIndex"].as_u64().unwrap() as usize;
                let pid = account_keys[pid_index].as_str();
                let stack_height = inner["stackHeight"].as_u64().unwrap();
                let indent = "    ".repeat((stack_height - 2) as usize);
                println!("{}└─ [{}] {}", indent, stack_height, pid);
            }
        }
    }
}
