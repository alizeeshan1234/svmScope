//! Builds the CPI (cross-program invocation) call tree of a transaction.

use crate::utils::resolve_account_keys;
use serde_json::Value;

/// One account an instruction touches, with its IDL role name where known.
#[derive(Debug, Clone, serde::Serialize)]
pub struct IxAccount {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub address: String,
}

/// One decoded instruction argument.
#[derive(Debug, Clone, serde::Serialize)]
pub struct IxArg {
    pub name: String,
    #[serde(rename = "type")]
    pub ty: String,
    pub value: String,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct CpiEntry {
    pub index: usize,
    pub program: String,
    pub stack_height: u64,
    /// Decoded instruction name (e.g. "Route V2"), filled in by `analyze` where an
    /// IDL or a known native layout lets us name it. `None` = couldn't decode.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// The instruction's accounts, named from the IDL / native layout where known.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub accounts: Vec<IxAccount>,
    /// Decoded instruction arguments (name/type/value).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<IxArg>,
    /// Raw instruction data, kept only so `analyze` can decode it; never serialized.
    #[serde(skip)]
    pub data: Vec<u8>,
    /// This instruction's account indexes into the resolved account list; used by
    /// `analyze` to resolve addresses, never serialized.
    #[serde(skip)]
    pub account_indexes: Vec<usize>,
}

/// Build the CPI call tree as a flat list; nesting is carried by `stack_height`
/// (1 = top-level instruction, 2 = a CPI, 3 = a nested CPI, ...).
pub fn build_cpi_tree(tx: &Value) -> Vec<CpiEntry> {
    let empty = vec![];
    let instructions = tx["transaction"]["message"]["instructions"]
        .as_array()
        .unwrap_or(&empty);

    // Full account list, including accounts loaded from Address Lookup Tables.
    let account_keys = resolve_account_keys(tx);
    let program_at = |ix: &Value| -> Option<String> {
        let i = ix["programIdIndex"].as_u64()? as usize;
        account_keys.get(i).cloned()
    };

    // Instruction data is base58 in `json` encoding; keep the raw bytes so the
    // caller can decode an instruction name from them.
    let data_of = |ix: &Value| -> Vec<u8> {
        ix["data"]
            .as_str()
            .and_then(|s| bs58::decode(s).into_vec().ok())
            .unwrap_or_default()
    };
    let accts_of = |ix: &Value| -> Vec<usize> {
        ix["accounts"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|i| i.as_u64().map(|v| v as usize))
                    .collect()
            })
            .unwrap_or_default()
    };
    let entry = |index, program, stack_height, ix: &Value| CpiEntry {
        index,
        program,
        stack_height,
        name: None,
        accounts: vec![],
        args: vec![],
        data: data_of(ix),
        account_indexes: accts_of(ix),
    };

    let mut entries: Vec<CpiEntry> = Vec::new();

    for (index, ix) in instructions.iter().enumerate() {
        let Some(program) = program_at(ix) else {
            continue;
        };

        // Top-level instruction.
        entries.push(entry(index, program, 1, ix));

        // No inner group just means this instruction made no CPIs — that's normal.
        let my_group = tx["meta"]["innerInstructions"]
            .as_array()
            .unwrap_or(&empty)
            .iter()
            .find(|group| group["index"].as_u64() == Some(index as u64));

        if let Some(my_group) = my_group {
            for inner in my_group["instructions"].as_array().unwrap_or(&empty) {
                let Some(program) = program_at(inner) else {
                    continue;
                };
                // Old transactions (pre-v1.14.6 meta) report no stackHeight; a
                // direct CPI (depth 2) is the faithful default there.
                let stack_height = inner["stackHeight"].as_u64().unwrap_or(2);
                entries.push(entry(index, program, stack_height, inner));
            }
        }
    }

    entries
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tx(inner: Value) -> Value {
        json!({
            "transaction": { "message": {
                "accountKeys": ["Payer111", "ProgA", "ProgB"],
                "instructions": [{ "programIdIndex": 1 }]
            }},
            "meta": { "innerInstructions": inner }
        })
    }

    #[test]
    fn builds_tree_with_stack_heights() {
        let t = tx(json!([{ "index": 0, "instructions": [
            { "programIdIndex": 2, "stackHeight": 2 },
            { "programIdIndex": 1, "stackHeight": 3 }
        ]}]));
        let tree = build_cpi_tree(&t);
        assert_eq!(tree.len(), 3);
        assert_eq!(
            (tree[0].program.as_str(), tree[0].stack_height),
            ("ProgA", 1)
        );
        assert_eq!(
            (tree[1].program.as_str(), tree[1].stack_height),
            ("ProgB", 2)
        );
        assert_eq!(
            (tree[2].program.as_str(), tree[2].stack_height),
            ("ProgA", 3)
        );
    }

    #[test]
    fn missing_stack_height_defaults_to_direct_cpi() {
        let t = tx(json!([{ "index": 0, "instructions": [{ "programIdIndex": 2 }] }]));
        let tree = build_cpi_tree(&t);
        assert_eq!(tree[1].stack_height, 2);
    }

    #[test]
    fn tolerates_missing_meta_and_instructions() {
        assert!(build_cpi_tree(&json!({})).is_empty());
        let no_inner = json!({
            "transaction": { "message": {
                "accountKeys": ["Payer111", "ProgA"],
                "instructions": [{ "programIdIndex": 1 }]
            }},
            "meta": {}
        });
        let tree = build_cpi_tree(&no_inner);
        assert_eq!(tree.len(), 1);
        assert_eq!(tree[0].stack_height, 1);
    }
}
