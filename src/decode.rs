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
#[derive(Serialize, Debug)]
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
///
/// The base Token and Token-2022 layouts are identical, so we decode both. A
/// Token-2022 account with extensions is longer than its base size; the type
/// byte at offset 165 (Token-2022 pads mints past that offset precisely to
/// disambiguate) tells us whether it's a Mint (1) or an Account (2).
pub fn decode_bytes(owner: &str, data: &[u8]) -> Option<DecodedAccount> {
    decode(owner, data)
}

const STAKE_PROGRAM: &str = "Stake11111111111111111111111111111111111111";
const ALT_PROGRAM: &str = "AddressLookupTab1e1111111111111111111111111";
const SYSTEM_PROGRAM: &str = "11111111111111111111111111111111";

fn decode(owner: &str, data: &[u8]) -> Option<DecodedAccount> {
    match owner {
        SPL_TOKEN | SPL_TOKEN_2022 => match data.len() {
            82 => Some(decode_mint(data)),   // base mint
            165 => Some(decode_token_account(data)), // base token account
            // Token-2022 with extensions: disambiguate by the account-type byte.
            n if n > 165 => match data[165] {
                1 => Some(decode_mint(data)),          // Mint (base 82 still valid)
                2 => Some(decode_token_account(data)), // Account (base 165 still valid)
                _ => None,
            },
            _ => None,
        },
        // Native programs never publish an IDL, so their layouts are hand-written.
        ALT_PROGRAM => decode_lookup_table(data),
        STAKE_PROGRAM => decode_stake(data),
        SYSTEM_PROGRAM if data.len() == 80 => decode_nonce(data),
        _ => None,
    }
}

/// Address Lookup Table: a fixed 56-byte header, then a list of 32-byte addresses.
fn decode_lookup_table(data: &[u8]) -> Option<DecodedAccount> {
    if data.len() < 56 {
        return None;
    }
    let authority = if data[20] == 1 && data.len() >= 56 {
        read_pubkey(data, 22)
    } else {
        "none".into()
    };
    let addresses = (data.len() - 56) / 32;
    Some(DecodedAccount {
        type_name: "Address Lookup Table".into(),
        fields: vec![
            field("deactivationSlot", 4, "u64", 8, read_u64(data, 4).to_string(), true),
            field("lastExtendedSlot", 12, "u64", 8, read_u64(data, 12).to_string(), true),
            field("lastExtendedSlotStartIndex", 20, "u8", 1, data[20].to_string(), false),
            field("authority", 22, "pubkey", 32, authority, false),
            field("addressCount", 56, "usize", 0, addresses.to_string(), false),
        ],
    })
}

/// Stake account: 4-byte enum discriminant, then Meta (authorized + lockup) and,
/// when delegated, the Stake struct.
fn decode_stake(data: &[u8]) -> Option<DecodedAccount> {
    if data.len() < 120 {
        return None;
    }
    let state = match read_u32(data, 0) {
        0 => "Uninitialized",
        1 => "Initialized",
        2 => "Stake",
        3 => "RewardsPool",
        _ => "unknown",
    };
    let mut fields = vec![
        field("state", 0, "enum", 4, state.into(), false),
        field("rentExemptReserve", 4, "u64", 8, read_u64(data, 4).to_string(), true),
        field("authorizedStaker", 12, "pubkey", 32, read_pubkey(data, 12), true),
        field("authorizedWithdrawer", 44, "pubkey", 32, read_pubkey(data, 44), true),
        field("lockupUnixTimestamp", 76, "i64", 8, read_u64(data, 76).to_string(), true),
        field("lockupEpoch", 84, "u64", 8, read_u64(data, 84).to_string(), true),
        field("lockupCustodian", 92, "pubkey", 32, read_pubkey(data, 92), false),
    ];
    // Delegation follows Meta (124 bytes in) when the account is staked.
    if read_u32(data, 0) == 2 && data.len() >= 196 {
        fields.extend([
            field("voterPubkey", 124, "pubkey", 32, read_pubkey(data, 124), true),
            field("stake", 156, "u64", 8, read_u64(data, 156).to_string(), true),
            field("activationEpoch", 164, "u64", 8, read_u64(data, 164).to_string(), true),
            field("deactivationEpoch", 172, "u64", 8, read_u64(data, 172).to_string(), true),
        ]);
    }
    Some(DecodedAccount { type_name: "Stake Account".into(), fields })
}

/// Durable nonce account (system-owned, exactly 80 bytes).
fn decode_nonce(data: &[u8]) -> Option<DecodedAccount> {
    if data.len() < 80 {
        return None;
    }
    Some(DecodedAccount {
        type_name: "Nonce Account".into(),
        fields: vec![
            field("version", 0, "u32", 4, read_u32(data, 0).to_string(), false),
            field("state", 4, "u32", 4, read_u32(data, 4).to_string(), false),
            field("authority", 8, "pubkey", 32, read_pubkey(data, 8), true),
            field("blockhash", 40, "pubkey", 32, read_pubkey(data, 40), false),
            field("lamportsPerSignature", 72, "u64", 8, read_u64(data, 72).to_string(), true),
        ],
    })
}

/// Best-effort structural inference for accounts with no schema at all.
///
/// Without type information we can't know field *names*, but the byte patterns
/// are still informative: a 32-byte run that base58-decodes to a plausible key is
/// almost certainly a pubkey, small u64s are counters/amounts, and values near
/// the current unix time are timestamps. Everything is reported as `@offset` so
/// it stays honest about being inferred, and stays editable so it's still useful
/// for what-ifs.
pub fn infer_layout(data: &[u8]) -> Option<DecodedAccount> {
    if data.len() < 8 {
        return None;
    }
    let mut fields = Vec::new();
    let mut i = 0usize;
    // Anchor-style accounts start with an 8-byte discriminator; show it as such.
    fields.push(field(
        "discriminator?",
        0,
        "[u8; 8]",
        8,
        data[0..8].iter().map(|b| format!("{b:02x}")).collect(),
        false,
    ));
    i += 8;

    while i + 8 <= data.len() && fields.len() < 64 {
        // A 32-byte window that isn't mostly zeros and isn't all 0xff reads as a pubkey.
        if i + 32 <= data.len() {
            let w = &data[i..i + 32];
            let zeros = w.iter().filter(|b| **b == 0).count();
            let looks_key = zeros <= 8 && w.iter().any(|b| *b != 0xff);
            if looks_key {
                fields.push(field(&format!("pubkey@{i}"), i, "pubkey", 32, read_pubkey(data, i), true));
                i += 32;
                continue;
            }
        }
        let v = read_u64(data, i);
        // Plausible unix seconds (2020..2050) → timestamp; otherwise a number.
        let label = if (1_577_836_800..4_102_444_800).contains(&v) { "i64 (time?)" } else { "u64" };
        fields.push(field(&format!("u64@{i}"), i, label, 8, v.to_string(), true));
        i += 8;
    }
    Some(DecodedAccount { type_name: "Inferred layout".into(), fields })
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

    // Cache each program's IDL so we fetch it at most once per analysis (an IDL
    // fetch is an RPC call). `None` = we already checked and it has no on-chain IDL.
    let mut idl_cache: std::collections::HashMap<String, Option<serde_json::Value>> =
        std::collections::HashMap::new();

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

        // Try the built-in SPL layouts first; fall back to the program's Anchor
        // IDL (fetched + cached) so arbitrary program accounts decode too.
        let decoded = if executable {
            None
        } else {
            // Built-in layouts → the program's on-chain IDL → structural inference.
            decode(&owner, &data)
                .or_else(|| {
                    let idl = idl_cache.entry(owner.clone()).or_insert_with(|| {
                        std::str::FromStr::from_str(&owner)
                            .ok()
                            .and_then(|a| crate::idl::fetch_idl_json(client, a))
                    });
                    idl.as_ref()
                        .and_then(|idl| crate::idl::decode_with_idl(idl, &data))
                })
                .or_else(|| infer_layout(&data))
        };

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
