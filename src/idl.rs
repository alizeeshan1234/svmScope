use std::io::Read;

use crate::decode::{DecodedAccount, Field};
use flate2::bufread::ZlibDecoder;
use serde_json::{json, Value};
use solana_address::Address;
use solana_client::rpc_client::RpcClient;

pub fn fetch_idl_json(client: &RpcClient, program_id: Address) -> Option<Value> {
    let base = Address::find_program_address(&[], &program_id).0;
    let idl_addr = Address::create_with_seed(&base, "anchor:idl", &program_id).ok()?;

    // Most programs don't publish an on-chain IDL — a missing account is the
    // common case, not an error, so return None rather than panicking.
    let idl_account = client.get_account_data(&idl_addr).ok()?;

    let len_bytes: [u8; 4] = idl_account.get(40..44)?.try_into().ok()?;
    let len = u32::from_le_bytes(len_bytes) as usize;

    let compressed = idl_account.get(44..44 + len)?;

    let mut decoder = ZlibDecoder::new(compressed);
    let mut out = Vec::new();

    decoder.read_to_end(&mut out).ok()?;

    serde_json::from_slice::<Value>(&out).ok()
}

/// A program error resolved from an IDL's `errors[]` — the difference between
/// `Custom(6024)` and "Slippage exceeded".
#[derive(serde::Serialize, Clone)]
pub struct IdlError {
    pub code: u64,
    pub name: String,
    pub msg: String,
}

/// Look up a custom error code in an IDL.
pub fn error_for_code(idl: &Value, code: u64) -> Option<IdlError> {
    idl.get("errors")?.as_array()?.iter().find_map(|e| {
        (e.get("code")?.as_u64()? == code).then(|| IdlError {
            code,
            name: e
                .get("name")
                .and_then(|n| n.as_str())
                .unwrap_or("")
                .to_string(),
            msg: e
                .get("msg")
                .and_then(|m| m.as_str())
                .unwrap_or("")
                .to_string(),
        })
    })
}

/// One instruction a program exposes, for the transaction builder.
#[derive(serde::Serialize)]
pub struct IdlInstruction {
    pub name: String,
    /// 8-byte Anchor discriminator.
    pub discriminator: Vec<u8>,
    pub docs: Vec<String>,
    pub accounts: Vec<IdlAccountSpec>,
    pub args: Vec<IdlArg>,
}

#[derive(serde::Serialize)]
pub struct IdlAccountSpec {
    pub name: String,
    pub writable: bool,
    pub signer: bool,
    /// True when the IDL says this account is a PDA.
    pub pda: bool,
    /// The PDA's seeds, so the client can derive the address instead of making
    /// the user paste it: `const` carries literal bytes, `account` names another
    /// account in this instruction whose key is the seed.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub seeds: Vec<PdaSeed>,
    /// A fixed address, when the IDL pins one (e.g. system_program).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub address: Option<String>,
}

/// One seed of a PDA derivation.
#[derive(serde::Serialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum PdaSeed {
    /// Literal bytes.
    Const { bytes: Vec<u8> },
    /// The public key of another account in the same instruction.
    Account { path: String },
}

#[derive(serde::Serialize)]
pub struct IdlArg {
    pub name: String,
    /// Wire type label, e.g. "u64" / "pubkey" / "string".
    #[serde(rename = "type")]
    pub ty: String,
}

/// Extract a program's instructions from its IDL, for building transactions.
pub fn instructions(idl: &Value) -> Vec<IdlInstruction> {
    let Some(list) = idl.get("instructions").and_then(|i| i.as_array()) else {
        return vec![];
    };
    list.iter()
        .filter_map(|ix| {
            Some(IdlInstruction {
                name: ix.get("name")?.as_str()?.to_string(),
                discriminator: ix
                    .get("discriminator")?
                    .as_array()?
                    .iter()
                    .filter_map(|b| b.as_u64().map(|v| v as u8))
                    .collect(),
                docs: ix
                    .get("docs")
                    .and_then(|d| d.as_array())
                    .map(|d| {
                        d.iter()
                            .filter_map(|s| s.as_str().map(String::from))
                            .collect()
                    })
                    .unwrap_or_default(),
                accounts: ix
                    .get("accounts")
                    .and_then(|a| a.as_array())
                    .map(|a| {
                        a.iter()
                            .map(|acc| IdlAccountSpec {
                                name: acc
                                    .get("name")
                                    .and_then(|n| n.as_str())
                                    .unwrap_or("")
                                    .to_string(),
                                writable: acc
                                    .get("writable")
                                    .and_then(|w| w.as_bool())
                                    .unwrap_or(false),
                                signer: acc
                                    .get("signer")
                                    .and_then(|s| s.as_bool())
                                    .unwrap_or(false),
                                pda: acc.get("pda").is_some(),
                                seeds: acc
                                    .get("pda")
                                    .and_then(|p| p.get("seeds"))
                                    .and_then(|s| s.as_array())
                                    .map(|seeds| {
                                        seeds
                                            .iter()
                                            .filter_map(|sd| match sd.get("kind")?.as_str()? {
                                                "const" => Some(PdaSeed::Const {
                                                    bytes: sd
                                                        .get("value")?
                                                        .as_array()?
                                                        .iter()
                                                        .filter_map(|b| b.as_u64().map(|v| v as u8))
                                                        .collect(),
                                                }),
                                                "account" => Some(PdaSeed::Account {
                                                    path: sd.get("path")?.as_str()?.to_string(),
                                                }),
                                                _ => None,
                                            })
                                            .collect()
                                    })
                                    .unwrap_or_default(),
                                address: acc
                                    .get("address")
                                    .and_then(|a| a.as_str())
                                    .map(String::from),
                            })
                            .collect()
                    })
                    .unwrap_or_default(),
                args: ix
                    .get("args")
                    .and_then(|a| a.as_array())
                    .map(|a| {
                        a.iter()
                            .map(|arg| IdlArg {
                                name: arg
                                    .get("name")
                                    .and_then(|n| n.as_str())
                                    .unwrap_or("")
                                    .to_string(),
                                ty: type_label(arg.get("type").unwrap_or(&Value::Null)),
                            })
                            .collect()
                    })
                    .unwrap_or_default(),
            })
        })
        .collect()
}

/// Find the IDL instruction whose 8-byte discriminator matches this instruction's
/// data — the entry that names it and describes its args and accounts.
pub fn find_ix<'a>(idl: &'a Value, data: &[u8]) -> Option<&'a Value> {
    let disc = data.get(0..8)?;
    idl.get("instructions")?.as_array()?.iter().find(|ix| {
        ix.get("discriminator")
            .and_then(|d| d.as_array())
            .map(|d| {
                d.iter()
                    .filter_map(|b| b.as_u64().map(|v| v as u8))
                    .collect::<Vec<u8>>()
                    == disc
            })
            .unwrap_or(false)
    })
}

/// Borsh-decode an Anchor instruction's arguments — the bytes after the 8-byte
/// discriminator — using the IDL instruction's `args`. Returns `(name, type,
/// value)` per arg, stopping at the first variable-length arg (offsets past it
/// are no longer trustworthy), same rule as the account-field walker.
pub fn decode_ix_args(idl_ix: &Value, data: &[u8]) -> Vec<(String, String, String)> {
    let mut out = Vec::new();
    let Some(args) = idl_ix.get("args").and_then(|a| a.as_array()) else {
        return out;
    };
    let mut off = 8usize; // skip the discriminator
    for arg in args {
        let name = arg
            .get("name")
            .and_then(|n| n.as_str())
            .unwrap_or("")
            .to_string();
        let ty = arg.get("type").unwrap_or(&Value::Null);
        match resolve_fixed(ty) {
            Some(kind) => {
                let sz = kind.size();
                let Some(bytes) = data.get(off..off + sz) else {
                    break;
                };
                out.push((name, kind.label(), read_value(bytes, kind)));
                off += sz;
            }
            None => {
                // Variable-length / composite arg — name it, but the value and
                // everything after it can't be read from a fixed offset.
                out.push((name, type_label(ty), String::new()));
                break;
            }
        }
    }
    out
}

/// A readable label for an IDL type (used by the builder's arg inputs).
fn type_label(ty: &Value) -> String {
    if let Some(s) = ty.as_str() {
        return s.to_string();
    }
    if let Some(d) = defined_name(ty) {
        return d.to_string();
    }
    if ty.get("vec").is_some() {
        return "vec".into();
    }
    if ty.get("option").is_some() {
        return "option".into();
    }
    if ty.get("array").is_some() {
        return "array".into();
    }
    "unknown".into()
}

// ---------------------------------------------------------------------------
// IDL account decoding — turn raw program-account bytes into named fields.
// ---------------------------------------------------------------------------

/// A fixed-size scalar type we know how to read from account bytes.
#[derive(Clone, Copy)]
enum Kind {
    U(usize), // unsigned int, `usize` = byte width (1,2,4,8,16)
    I(usize), // signed int
    Bool,
    Pubkey,
    Bytes(usize), // fixed byte array [u8; N]
}

impl Kind {
    /// The display label the UI shows in the "Type" column.
    fn label(self) -> String {
        match self {
            Kind::U(n) => format!("u{}", n * 8),
            Kind::I(n) => format!("i{}", n * 8),
            Kind::Bool => "bool".into(),
            Kind::Pubkey => "pubkey".into(),
            Kind::Bytes(n) => format!("[u8; {n}]"),
        }
    }
    fn size(self) -> usize {
        match self {
            Kind::U(n) | Kind::I(n) | Kind::Bytes(n) => n,
            Kind::Bool => 1,
            Kind::Pubkey => 32,
        }
    }
    /// Whether the UI should let the user edit this field.
    fn editable(self) -> bool {
        !matches!(self, Kind::Bytes(_))
    }
}

/// Resolve an IDL field `type` to a fixed-size [`Kind`]: scalars and fixed
/// `{"array": [inner, N]}` types resolve; anything variable-length or composite
/// (`vec`/`string`/`option`/`defined` — the last is inlined by the walker itself)
/// returns `None`, which tells the caller offsets are no longer trustworthy.
fn resolve_fixed(ty: &Value) -> Option<Kind> {
    // String scalars: "u64", "pubkey", "bool", …
    if let Some(s) = ty.as_str() {
        return match s {
            "u8" => Some(Kind::U(1)),
            "u16" => Some(Kind::U(2)),
            "u32" => Some(Kind::U(4)),
            "u64" => Some(Kind::U(8)),
            "u128" => Some(Kind::U(16)),
            "i8" => Some(Kind::I(1)),
            "i16" => Some(Kind::I(2)),
            "i32" => Some(Kind::I(4)),
            "i64" => Some(Kind::I(8)),
            "i128" => Some(Kind::I(16)),
            "bool" => Some(Kind::Bool),
            "pubkey" | "publicKey" => Some(Kind::Pubkey),
            _ => None,
        };
    }
    // Fixed arrays keep stable offsets; every other object type stops the walk.
    if let Some(arr) = ty.get("array").and_then(|a| a.as_array()) {
        let inner = resolve_fixed(arr.first()?)?;
        let count = arr.get(1)?.as_u64()?;
        return Some(Kind::Bytes(inner.size() * count as usize));
    }

    None
}

/// Format a fixed-size scalar's bytes into a human-readable value string.
fn read_value(bytes: &[u8], kind: Kind) -> String {
    match kind {
        Kind::U(_) => {
            let mut buf = [0u8; 16];
            buf[..bytes.len()].copy_from_slice(bytes);
            u128::from_le_bytes(buf).to_string()
        }
        Kind::I(n) => {
            let mut buf = [0u8; 16];
            buf[..bytes.len()].copy_from_slice(bytes);
            // sign-extend if the value is negative
            if bytes[n - 1] & 0x80 != 0 {
                for b in &mut buf[n..] {
                    *b = 0xff;
                }
            }
            i128::from_le_bytes(buf).to_string()
        }
        Kind::Bool => (bytes[0] != 0).to_string(),
        Kind::Pubkey => {
            let arr: [u8; 32] = bytes.try_into().unwrap();
            Address::from(arr).to_string()
        }
        Kind::Bytes(_) => bytes.iter().map(|b| format!("{b:02x}")).collect(),
    }
}

/// Get a `defined` field's type name, handling both the new IDL shape
/// (`{"defined": {"name": "Fees"}}`) and the old one (`{"defined": "Fees"}`).
fn defined_name(ty: &Value) -> Option<&str> {
    let d = ty.get("defined")?;
    d.as_str()
        .or_else(|| d.get("name").and_then(|n| n.as_str()))
}

/// Read a little-endian u32 at `offset` (borsh length prefixes).
fn read_u32_at(data: &[u8], offset: usize) -> Option<u32> {
    let b = data.get(offset..offset + 4)?;
    Some(u32::from_le_bytes(b.try_into().ok()?))
}

/// Look up a struct type's fields in `idl["types"]` by name.
/// Returns `None` for enums / unknown types — those are handled separately.
fn struct_fields<'a>(types: &'a [Value], name: &str) -> Option<&'a Vec<Value>> {
    let t = types
        .iter()
        .find(|t| t.get("name").and_then(|n| n.as_str()) == Some(name))?;
    let ty = t.get("type")?;
    if ty.get("kind").and_then(|k| k.as_str()) == Some("struct") {
        ty.get("fields")?.as_array()
    } else {
        None
    }
}

/// The variants of an enum type in `idl["types"]`, if `name` is an enum.
fn enum_variants<'a>(types: &'a [Value], name: &str) -> Option<&'a Vec<Value>> {
    let t = types
        .iter()
        .find(|t| t.get("name").and_then(|n| n.as_str()) == Some(name))?;
    let ty = t.get("type")?;
    if ty.get("kind").and_then(|k| k.as_str()) == Some("enum") {
        ty.get("variants")?.as_array()
    } else {
        None
    }
}

/// Walk a struct's fields, appending a `Field` for each and moving `*offset`
/// forward. When a field is itself a struct, it calls itself to read that struct
/// inline (that's the recursion). Returns `false` the moment it hits something
/// variable-length or unknown — the caller stops too, because every offset after
/// that point would be wrong.
fn walk_fields(
    fields_json: &[Value],
    types: &[Value],
    data: &[u8],
    offset: &mut usize,
    prefix: &str,
    out: &mut Vec<Field>,
) -> bool {
    for f in fields_json {
        // The field's display name, e.g. "bump" at the top level or
        // "flat_fees.numerator" inside a nested struct.
        let fname = match f.get("name").and_then(|n| n.as_str()) {
            Some(n) => format!("{prefix}{n}"),
            None => return false,
        };
        let ty = match f.get("type") {
            Some(t) => t,
            None => return false,
        };

        // Is this field a nested struct? If so, read its fields inline, right
        // here at the current cursor, by calling ourselves with a dotted prefix.
        if let Some(name) = defined_name(ty) {
            if let Some(sub) = struct_fields(types, name) {
                if !walk_fields(sub, types, data, offset, &format!("{fname}."), out) {
                    return false; // the nested struct hit something variable
                }
                continue;
            }
            // An enum is a 1-byte variant tag, then that variant's payload (if any),
            // so we can read the tag, name the variant, and walk its fields.
            if let Some(variants) = enum_variants(types, name) {
                let Some(&tag) = data.get(*offset) else {
                    return false;
                };
                let variant = variants.get(tag as usize);
                let vname = variant
                    .and_then(|v| v.get("name"))
                    .and_then(|n| n.as_str())
                    .unwrap_or("unknown");
                out.push(Field {
                    name: fname.clone(),
                    offset: *offset,
                    ty: format!("enum {name}"),
                    size: 1,
                    value: vname.to_string(),
                    editable: false,
                    note: Some(format!("variant {tag}")),
                });
                *offset += 1;
                // Tuple/struct variants carry fields; walk them so the cursor stays true.
                if let Some(vfields) = variant
                    .and_then(|v| v.get("fields"))
                    .and_then(|f| f.as_array())
                {
                    // Tuple variants are bare types; give them positional names.
                    let named: Vec<Value> = vfields
                        .iter()
                        .enumerate()
                        .map(|(i, f)| {
                            if f.get("name").is_some() {
                                f.clone()
                            } else {
                                json!({ "name": i.to_string(), "type": f.clone() })
                            }
                        })
                        .collect();
                    if !walk_fields(&named, types, data, offset, &format!("{fname}."), out) {
                        return false;
                    }
                }
                continue;
            }
            return false; // unknown defined type → stop
        }

        // A fixed array of structs (e.g. `reward_infos: [WhirlpoolRewardInfo; 3]`)
        // expands into indexed sub-fields, read inline like a nested struct.
        if let Some(arr) = ty.get("array").and_then(|a| a.as_array()) {
            if let (Some(inner), Some(count)) = (arr.first(), arr.get(1).and_then(|c| c.as_u64())) {
                if let Some(name) = defined_name(inner) {
                    let Some(sub) = struct_fields(types, name) else {
                        return false;
                    };
                    for i in 0..count {
                        if !walk_fields(sub, types, data, offset, &format!("{fname}[{i}]."), out) {
                            return false;
                        }
                    }
                    continue;
                }
            }
        }

        // Variable-length borsh types carry their length in the data, so we can
        // read them and keep walking — the offsets after them are still correct.
        //
        // string: u32 length + utf8 bytes
        if ty.as_str() == Some("string") {
            let Some(len) = read_u32_at(data, *offset) else {
                return false;
            };
            let start = *offset + 4;
            let end = start + len as usize;
            if end > data.len() {
                return false;
            }
            let text = String::from_utf8_lossy(&data[start..end]).to_string();
            out.push(Field {
                name: fname,
                offset: *offset,
                ty: "string".into(),
                size: 4 + len as usize,
                value: text,
                editable: false,
                note: None,
            });
            *offset = end;
            continue;
        }

        // option<T>: 1-byte tag, then T when present.
        if let Some(inner) = ty.get("option") {
            let Some(&tag) = data.get(*offset) else {
                return false;
            };
            *offset += 1;
            if tag == 0 {
                out.push(Field {
                    name: fname,
                    offset: *offset - 1,
                    ty: "option".into(),
                    size: 1,
                    value: "none".into(),
                    editable: false,
                    note: None,
                });
                continue;
            }
            // Present: fall through by walking the inner type as a single field.
            let one = json!([{ "name": fname, "type": inner.clone() }]);
            let Some(arr) = one.as_array() else {
                return false;
            };
            if !walk_fields(arr, types, data, offset, "", out) {
                return false;
            }
            continue;
        }

        // vec<T>: u32 count, then the elements. Walk each so the cursor stays true.
        if let Some(inner) = ty.get("vec") {
            let Some(count) = read_u32_at(data, *offset) else {
                return false;
            };
            out.push(Field {
                name: format!("{fname}.len"),
                offset: *offset,
                ty: "u32".into(),
                size: 4,
                value: count.to_string(),
                editable: false,
                note: None,
            });
            *offset += 4;
            // Cap how many elements we expand so a huge vec can't flood the UI;
            // beyond the cap we can't know the size, so stop cleanly.
            const MAX_ELEMS: u32 = 32;
            if count > MAX_ELEMS {
                return false;
            }
            for i in 0..count {
                let one = json!([{ "name": format!("{fname}[{i}]"), "type": inner.clone() }]);
                let Some(arr) = one.as_array() else {
                    return false;
                };
                if !walk_fields(arr, types, data, offset, "", out) {
                    return false;
                }
            }
            continue;
        }

        // Otherwise it's a plain scalar or fixed array. Read it and advance.
        let kind = match resolve_fixed(ty) {
            Some(k) => k,
            None => return false, // unknown type → stop
        };
        let size = kind.size();
        if *offset + size > data.len() {
            return false;
        }
        out.push(Field {
            name: fname,
            offset: *offset,
            ty: kind.label(),
            size,
            value: read_value(&data[*offset..*offset + size], kind),
            editable: kind.editable(),
            note: None,
        });
        *offset += size;
    }
    true
}

/// Decode an account's raw bytes using its program's Anchor IDL.
///
/// Returns `None` if the leading 8-byte discriminator doesn't match any account
/// type in the IDL. Fields are walked from offset 8 (Anchor prepends the
/// discriminator) and stop at the first variable-length field.
pub fn decode_with_idl(idl: &Value, data: &[u8]) -> Option<DecodedAccount> {
    if data.len() < 8 {
        return None;
    }
    let disc = &data[0..8];

    // 1. Match the discriminator to an account name.
    let accounts = idl.get("accounts")?.as_array()?;
    let mut type_name: Option<String> = None;
    for a in accounts {
        let d = a.get("discriminator").and_then(|d| d.as_array());
        let matches = d.is_some_and(|d| {
            d.len() == 8
                && d.iter()
                    .zip(disc)
                    .all(|(v, b)| v.as_u64() == Some(*b as u64))
        });
        if matches {
            type_name = a.get("name").and_then(|n| n.as_str()).map(String::from);
            break;
        }
    }
    let type_name = type_name?;

    // 2. Find that type's field layout in idl["types"].
    let types = idl.get("types")?.as_array()?;
    let fields_json = types
        .iter()
        .find(|t| t.get("name").and_then(|n| n.as_str()) == Some(type_name.as_str()))?
        .get("type")?
        .get("fields")?
        .as_array()?;

    // 3. Walk the fields from offset 8 (Anchor prepends the discriminator),
    //    reading each into `fields` and stopping at the first variable field.
    let mut fields: Vec<Field> = Vec::new();
    let mut offset = 8usize;
    walk_fields(fields_json, types, data, &mut offset, "", &mut fields);

    Some(DecodedAccount { type_name, fields })
}
