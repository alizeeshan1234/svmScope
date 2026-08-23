//! Account layout decoding.
//!
//! To mutate an account without hand-computing byte offsets, we need to know
//! what its bytes *mean*. This module recognizes well-known account types
//! (SPL Token accounts and mints) and describes their fields — name, offset,
//! type, and current value — so the UI can present a form instead of a hex box.
//!
//! Unknown accounts get no schema; the UI falls back to a raw byte patch.

use base64::Engine;
use serde::Serialize;
use serde_json::json;
use solana_address::Address;
use solana_client::rpc_client::RpcClient;
use solana_client::rpc_request::RpcRequest;

const SPL_TOKEN: &str = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA";
const SPL_TOKEN_2022: &str = "TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb";

/// A recognized account, broken into named fields.
#[derive(Serialize)]
pub struct DecodedAccount {
    /// Human label, e.g. "SPL Token Account".
    pub type_name: String,
    pub fields: Vec<Field>,
}

/// One field within a decoded account.
#[derive(Serialize)]
pub struct Field {
    pub name: String,
    pub offset: usize,
    /// Wire type the UI uses to pick an editor and encode edits:
    /// "u64" | "u8" | "bool" | "pubkey" | "coption-pubkey" | "coption-u64".
    #[serde(rename = "type")]
    pub ty: String,
    /// Size in bytes of the field's payload (for coption-*, the size after the tag).
    pub size: usize,
    /// Current value, formatted for display and to prefill the editor.
    pub value: String,
    /// Whether the UI should let the user change this field.
    pub editable: bool,
    /// Optional human hint (e.g. what a numeric state code means).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// One account's summary plus (if recognized) its decoded fields.
#[derive(Serialize)]
pub struct AccountInfo {
    pub address: String,
    pub owner: String,
    pub lamports: u64,
    pub executable: bool,
    pub data_len: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub decoded: Option<DecodedAccount>,
}

fn read_u64(data: &[u8], off: usize) -> u64 {
    let mut b = [0u8; 8];
    b.copy_from_slice(&data[off..off + 8]);
    u64::from_le_bytes(b)
}

fn read_pubkey(data: &[u8], off: usize) -> String {
    let b: [u8; 32] = data[off..off + 32].try_into().unwrap();
    Address::from(b).to_string()
}

/// A COption is a 4-byte little-endian tag (0 = None, 1 = Some) then the payload.
fn coption_is_some(data: &[u8], off: usize) -> bool {
    read_u32(data, off) == 1
}

fn read_u32(data: &[u8], off: usize) -> u32 {
    let mut b = [0u8; 4];
    b.copy_from_slice(&data[off..off + 4]);
    u32::from_le_bytes(b)
}

fn field(name: &str, offset: usize, ty: &str, size: usize, value: String, editable: bool) -> Field {
    Field { name: name.into(), offset, ty: ty.into(), size, value, editable, note: None }
}

/// Decode an SPL Token account (Token or Token-2022 base layout, 165 bytes).
fn decode_token_account(data: &[u8]) -> DecodedAccount {
    let delegate = if coption_is_some(data, 72) {
        read_pubkey(data, 76)
    } else {
        "none".into()
    };
    let is_native = if coption_is_some(data, 109) {
        read_u64(data, 113).to_string()
    } else {
        "none".into()
    };
    let close_authority = if coption_is_some(data, 129) {
        read_pubkey(data, 133)
    } else {
        "none".into()
    };
    let state_code = data[108];
    let mut state = field("state", 108, "u8", 1, state_code.to_string(), true);
    state.note = Some(
        match state_code {
            0 => "0 = uninitialized",
            1 => "1 = initialized",
            2 => "2 = frozen",
            _ => "unknown",
        }
        .into(),
    );

    DecodedAccount {
        type_name: "SPL Token Account".into(),
        fields: vec![
            field("mint", 0, "pubkey", 32, read_pubkey(data, 0), true),
            field("owner", 32, "pubkey", 32, read_pubkey(data, 32), true),
            field("amount", 64, "u64", 8, read_u64(data, 64).to_string(), true),
            field("delegate", 72, "coption-pubkey", 32, delegate, false),
            state,
            field("isNative", 109, "coption-u64", 8, is_native, false),
            field("delegatedAmount", 121, "u64", 8, read_u64(data, 121).to_string(), true),
            field("closeAuthority", 129, "coption-pubkey", 32, close_authority, false),
        ],
    }
}

/// Decode an SPL Mint (Token or Token-2022 base layout, 82 bytes).
fn decode_mint(data: &[u8]) -> DecodedAccount {
    let mint_authority = if coption_is_some(data, 0) {
        read_pubkey(data, 4)
    } else {
        "none".into()
    };
    let freeze_authority = if coption_is_some(data, 46) {
        read_pubkey(data, 50)
    } else {
        "none".into()
    };
    DecodedAccount {
        type_name: "SPL Mint".into(),
        fields: vec![
            field("mintAuthority", 0, "coption-pubkey", 32, mint_authority, false),
            field("supply", 36, "u64", 8, read_u64(data, 36).to_string(), true),
            field("decimals", 44, "u8", 1, data[44].to_string(), true),
            field("isInitialized", 45, "bool", 1, (data[45] != 0).to_string(), true),
            field("freezeAuthority", 46, "coption-pubkey", 32, freeze_authority, false),
        ],
    }
}

/// Recognize and decode an account by owner + data. Returns `None` for layouts
/// we don't know (the UI then offers a raw byte patch).
fn decode(owner: &str, data: &[u8]) -> Option<DecodedAccount> {
    let is_token = owner == SPL_TOKEN || owner == SPL_TOKEN_2022;
    // Match on the exact base sizes. Token-2022 accounts with extensions are
    // longer; their first bytes still follow this layout, but we stay strict
    // to avoid mis-decoding an extension's bytes.
    match (is_token, data.len()) {
        (true, 165) => Some(decode_token_account(data)),
        (true, 82) => Some(decode_mint(data)),
        _ => None,
    }
}

/// Fetch each account's on-chain state and decode any recognized layouts.
///
/// Parallel to `account_keys`; accounts that don't exist are skipped.
pub fn describe_accounts(client: &RpcClient, account_keys: &[String]) -> Vec<AccountInfo> {
    let resp: serde_json::Value = match client.send(
        RpcRequest::GetMultipleAccounts,
        json!([account_keys, { "encoding": "base64" }]),
    ) {
        Ok(r) => r,
        Err(_) => return vec![],
    };

    let values = match resp["value"].as_array() {
        Some(v) => v,
        None => return vec![],
    };

    let mut out = Vec::new();
    for (i, acc) in values.iter().enumerate() {
        if acc.is_null() {
            continue;
        }
        let owner = acc["owner"].as_str().unwrap_or_default().to_string();
        let lamports = acc["lamports"].as_u64().unwrap_or(0);
        let executable = acc["executable"].as_bool().unwrap_or(false);
        let data = acc["data"][0]
            .as_str()
            .and_then(|s| base64::engine::general_purpose::STANDARD.decode(s).ok())
            .unwrap_or_default();

        let decoded = if executable { None } else { decode(&owner, &data) };

        out.push(AccountInfo {
            address: account_keys[i].clone(),
            owner,
            lamports,
            executable,
            data_len: data.len(),
            decoded,
        });
    }
    out
}
