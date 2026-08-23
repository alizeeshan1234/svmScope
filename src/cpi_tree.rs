//! Builds the CPI (cross-program invocation) call tree of a transaction.

use crate::utils::resolve_account_keys;
use serde_json::Value;

#[derive(serde::Serialize)]
pub struct CpiEntry {
    pub index: usize,
    pub program: String,
    pub stack_height: u64,
}

/// Build the CPI call tree as a flat list; nesting is carried by `stack_height`
/// (1 = top-level instruction, 2 = a CPI, 3 = a nested CPI, ...).
pub fn build_cpi_tree(tx: &Value) -> Vec<CpiEntry> {
    let instructions = tx["transaction"]["message"]["instructions"]
        .as_array()
        .expect("instructions should be an array");

    // Full account list, including accounts loaded from Address Lookup Tables.
    let account_keys = resolve_account_keys(tx);

    let mut entries: Vec<CpiEntry> = Vec::new();

    for (index, ix) in instructions.iter().enumerate() {
        let program_id_index = ix["programIdIndex"].as_u64().unwrap() as usize;
        let program_id = &account_keys[program_id_index];

        // Top-level instruction.
        entries.push(CpiEntry {
            index,
            program: program_id.to_string(),
            stack_height: 1,
        });

        let inner_ix = tx["meta"]["innerInstructions"]
            .as_array()
            .expect("innerInstructions should be an array");

        let my_group = inner_ix
            .iter()
            .find(|group| group["index"].as_u64() == Some(index as u64));

        // No inner group just means this instruction made no CPIs — that's normal.
        if let Some(my_group) = my_group {
            let inners = my_group["instructions"].as_array().unwrap();
            for inner in inners {
                let pid_index = inner["programIdIndex"].as_u64().unwrap() as usize;
                let pid = &account_keys[pid_index];
                let stack_height = inner["stackHeight"].as_u64().unwrap();

                entries.push(CpiEntry {
                    index,
                    program: pid.to_string(),
                    stack_height,
                });
            }
        }
    }

    entries
}
