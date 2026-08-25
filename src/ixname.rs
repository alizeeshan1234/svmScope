//! Human names for instructions — the difference between a bare program id and
//! "Jupiter · Route V2". Native programs are decoded from their well-known
//! layouts; Anchor programs from their on-chain IDL's instruction discriminators.

use crate::idl;
use serde_json::Value;
use solana_address::Address;
use solana_client::rpc_client::RpcClient;
use std::collections::HashMap;
use std::str::FromStr;

const SYSTEM: &str = "11111111111111111111111111111111";
const COMPUTE_BUDGET: &str = "ComputeBudget111111111111111111111111111111";
const TOKEN: &str = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA";
const TOKEN_2022: &str = "TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb";
const ATA: &str = "ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL";
const MEMO: &str = "MemoSq4gqABAXKb96qnH8TysNcWxMyWCqXgDLGmfcHr";
const MEMO_V1: &str = "Memo1UhkJRfHyvLMcVucJwxXeuD728EqVDDwQDxFMNo";

/// Decode a Token-program instruction (Tokenkeg / Token-2022) by its leading tag.
fn token_ix(data: &[u8]) -> Option<&'static str> {
    Some(match data.first()? {
        0 => "Initialize Mint",
        1 => "Initialize Account",
        3 => "Transfer",
        4 => "Approve",
        5 => "Revoke",
        6 => "Set Authority",
        7 => "Mint To",
        8 => "Burn",
        9 => "Close Account",
        10 => "Freeze Account",
        11 => "Thaw Account",
        12 => "Transfer Checked",
        13 => "Approve Checked",
        14 => "Mint To Checked",
        15 => "Burn Checked",
        16 => "Initialize Account 2",
        17 => "Sync Native",
        18 => "Initialize Account 3",
        _ => return None,
    })
}

/// Decode a System-program instruction by its 4-byte little-endian tag.
fn system_ix(data: &[u8]) -> Option<&'static str> {
    let tag = u32::from_le_bytes(data.get(0..4)?.try_into().ok()?);
    Some(match tag {
        0 => "Create Account",
        1 => "Assign",
        2 => "Transfer",
        3 => "Create Account With Seed",
        8 => "Allocate",
        9 => "Allocate With Seed",
        10 => "Assign With Seed",
        11 => "Transfer With Seed",
        _ => return None,
    })
}

/// Decode a Compute-Budget instruction by its leading tag.
fn compute_budget_ix(data: &[u8]) -> Option<&'static str> {
    Some(match data.first()? {
        1 => "Request Heap Frame",
        2 => "Set Compute Unit Limit",
        3 => "Set Compute Unit Price",
        4 => "Set Loaded Accounts Data Size Limit",
        _ => return None,
    })
}

/// Turn an IDL instruction name (`routeV2`, `shared_accounts_route`) into a
/// display label (`Route V2`, `Shared Accounts Route`).
fn titleize(name: &str) -> String {
    let mut out = String::new();
    let mut prev_lower = false;
    for ch in name.chars() {
        if ch == '_' || ch == '-' {
            out.push(' ');
            prev_lower = false;
            continue;
        }
        // Split camelCase (lower→UPPER) only; keep digits attached ("v2" → "V2").
        if ch.is_uppercase() && prev_lower {
            out.push(' ');
        }
        out.push(ch);
        prev_lower = ch.is_lowercase();
    }
    // Capitalize the first letter of each word.
    out.split(' ')
        .map(|w| {
            let mut c = w.chars();
            match c.next() {
                Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
        .trim()
        .to_string()
}

/// Match an Anchor instruction's 8-byte discriminator against an IDL.
fn anchor_ix(idl: &Value, data: &[u8]) -> Option<String> {
    let disc = data.get(0..8)?;
    idl.get("instructions")?.as_array()?.iter().find_map(|ix| {
        let d = ix.get("discriminator")?.as_array()?;
        let bytes: Vec<u8> = d.iter().filter_map(|b| b.as_u64().map(|v| v as u8)).collect();
        if bytes == disc {
            Some(titleize(ix.get("name")?.as_str()?))
        } else {
            None
        }
    })
}

/// The human name of one instruction, if we can decode it. `idl_cache` avoids
/// re-fetching an IDL for every instruction of the same program.
pub fn decode(
    client: &RpcClient,
    idl_cache: &mut HashMap<String, Option<Value>>,
    program: &str,
    data: &[u8],
) -> Option<String> {
    match program {
        TOKEN | TOKEN_2022 => token_ix(data).map(String::from),
        SYSTEM => system_ix(data).map(String::from),
        COMPUTE_BUDGET => compute_budget_ix(data).map(String::from),
        ATA => Some(if data.first() == Some(&1) { "Create Idempotent".into() } else { "Create".into() }),
        MEMO | MEMO_V1 => Some("Memo".into()),
        _ => {
            // Anchor program: resolve (and cache) its on-chain IDL, then match the
            // instruction discriminator.
            let idl = idl_cache
                .entry(program.to_string())
                .or_insert_with(|| Address::from_str(program).ok().and_then(|a| idl::fetch_idl_json(client, a)));
            idl.as_ref().and_then(|i| anchor_ix(i, data))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn titleize_handles_camel_snake_and_digits() {
        assert_eq!(titleize("routeV2"), "Route V2");
        assert_eq!(titleize("route_v2"), "Route V2");
        assert_eq!(titleize("swapV2"), "Swap V2");
        assert_eq!(titleize("shared_accounts_route"), "Shared Accounts Route");
        assert_eq!(titleize("swap"), "Swap");
    }

    #[test]
    fn native_program_instructions_decode() {
        assert_eq!(token_ix(&[12]), Some("Transfer Checked"));
        assert_eq!(token_ix(&[3]), Some("Transfer"));
        assert_eq!(system_ix(&[2, 0, 0, 0]), Some("Transfer"));
        assert_eq!(compute_budget_ix(&[2]), Some("Set Compute Unit Limit"));
        assert_eq!(compute_budget_ix(&[4]), Some("Set Loaded Accounts Data Size Limit"));
        assert_eq!(token_ix(&[]), None);
    }

    #[test]
    fn anchor_discriminator_match() {
        let idl = serde_json::json!({
            "instructions": [
                { "name": "routeV2", "discriminator": [1, 2, 3, 4, 5, 6, 7, 8] },
                { "name": "swap", "discriminator": [9, 9, 9, 9, 9, 9, 9, 9] }
            ]
        });
        assert_eq!(anchor_ix(&idl, &[1, 2, 3, 4, 5, 6, 7, 8, 99]).as_deref(), Some("Route V2"));
        assert_eq!(anchor_ix(&idl, &[0, 0, 0, 0, 0, 0, 0, 0]).as_deref(), None);
    }
}
