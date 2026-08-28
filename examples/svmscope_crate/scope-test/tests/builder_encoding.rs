//! Extreme regression tests for the IDL-driven instruction builder and its
//! Borsh argument encoder, exercised purely through svmscope's PUBLIC API as an
//! external consumer. Everything here is OFFLINE: we only ever call
//! `.instruction()`, never `.transaction()`/`.send_and_capture()`, so no RPC is
//! required (Scope::new does not connect until a call needs the network).
//!
//! Where feasible we assert on the EXACT encoded bytes: `instruction.data` is the
//! 8-byte discriminator followed by the Borsh-encoded arguments in IDL order.

use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use solana_address::Address;
use solana_keypair::Keypair;
use solana_signer::Signer;
use svmscope::{Error, Scope};

const RPC: &str = "http://127.0.0.1:8899";
const REAL_IDL: &str = include_str!("../../target/idl/svmscope_crate.json");
const REAL_PROGRAM_ID: &str = "41NQKxFZ2eEAFPSKyP2BEvw2bkxwXjC8Wjrxcy6njF8u";

fn real_idl() -> Value {
    serde_json::from_str(REAL_IDL).expect("real IDL parses")
}

fn real_program() -> Address {
    Address::from_str_unchecked(REAL_PROGRAM_ID)
}

// solana-address exposes from_str via FromStr; add a tiny helper to avoid the
// import churn and keep call sites readable.
trait AddrExt {
    fn from_str_unchecked(s: &str) -> Address;
}
impl AddrExt for Address {
    fn from_str_unchecked(s: &str) -> Address {
        std::str::FromStr::from_str(s).expect("valid base58 address")
    }
}

/// A single-instruction synthetic program: method "m" with the given args and
/// (optional) defined types, no accounts. Used to isolate the argument encoder.
fn synth(args: Value, types: Value) -> Value {
    json!({
        "instructions": [{ "name": "m", "accounts": [], "args": args }],
        "types": types,
    })
}

/// Build `method`'s instruction data for `idl`, supplying `args` (a JSON object)
/// and a throwaway payer. Returns the full data (discriminator + encoded args).
fn data_for(idl: Value, method: &str, args: Value) -> Result<Vec<u8>, Error> {
    let scope = Scope::new(RPC);
    let payer = Keypair::new();
    let ix = scope
        .program_with_idl(Keypair::new().pubkey(), idl)
        .method(method)?
        .payer(&payer)
        .args(args)?
        .instruction()?;
    Ok(ix.data)
}

/// Just the encoded arguments for a synthetic single-arg-set method (drops the
/// 8-byte discriminator).
fn encode(args_spec: Value, types: Value, values: Value) -> Result<Vec<u8>, Error> {
    let data = data_for(synth(args_spec, types), "m", values)?;
    Ok(data[8..].to_vec())
}

fn encode_ok(args_spec: Value, types: Value, values: Value) -> Vec<u8> {
    encode(args_spec, types, values).expect("encoding should succeed")
}

// ---------------------------------------------------------------------------
// Real-IDL discriminators + account layout
// ---------------------------------------------------------------------------

#[test]
fn every_real_instruction_emits_its_idl_discriminator() {
    let idl = real_idl();
    let scope = Scope::new(RPC);
    let client = scope.program_with_idl(real_program(), idl.clone());
    // Cross-check source of truth: the public idl::instructions() helper.
    let listed = svmscope::idl::instructions(&idl);

    for spec in &listed {
        // Build each instruction with just enough to pass validation, then check
        // the first 8 data bytes equal the declared discriminator.
        let payer = Keypair::new();
        let mut builder = client.method(&spec.name).unwrap().payer(&payer);
        // Supply every account (payer covers signer accounts; others get fresh
        // addresses; fixed-address accounts like system_program auto-fill).
        for acct in &spec.accounts {
            if acct.address.is_some() {
                continue; // fixed address, auto-filled from the IDL
            }
            if acct.signer {
                builder = builder.account_signer(&acct.name, &payer);
            } else {
                builder = builder.account(&acct.name, Keypair::new().pubkey());
            }
        }
        // Supply every argument with a type-appropriate placeholder.
        for arg in &spec.args {
            let v: Value = match arg.ty.as_str() {
                "u64" | "u32" | "u16" | "u8" | "u128" => json!(1),
                "i64" | "i32" | "i16" | "i8" | "i128" => json!(-1),
                "bool" => json!(true),
                _ => json!(0),
            };
            builder = builder.arg(&arg.name, v);
        }
        let ix = builder.instruction().unwrap_or_else(|e| {
            panic!("building {} should succeed offline: {e}", spec.name)
        });
        assert_eq!(
            &ix.data[..8],
            spec.discriminator.as_slice(),
            "discriminator mismatch for {}",
            spec.name
        );
        assert_eq!(ix.program_id, real_program());
    }
    assert_eq!(listed.len(), 5, "expected 5 instructions in the real IDL");
}

#[test]
fn create_vesting_encodes_all_args_in_idl_order() {
    let idl = real_idl();
    let scope = Scope::new(RPC);
    let creator = Keypair::new();
    let beneficiary = Keypair::new().pubkey();
    let schedule = Keypair::new().pubkey();

    let (schedule_id, amount) = (7_u64, 50_000_000_u64);
    let (start_ts, cliff_ts, end_ts) = (-5_i64, 1_000_i64, 2_000_000_i64);

    let ix = scope
        .program_with_idl(real_program(), idl)
        .method("create_vesting")
        .unwrap()
        .payer(&creator)
        .account_signer("creator", &creator)
        .account("beneficiary", beneficiary)
        .account("schedule", schedule)
        .arg("schedule_id", schedule_id)
        .arg("amount", amount)
        .arg("start_ts", start_ts)
        .arg("cliff_ts", cliff_ts)
        .arg("end_ts", end_ts)
        .instruction()
        .unwrap();

    // discriminator + 5 * 8-byte little-endian scalars in IDL order.
    let mut expected = vec![135u8, 184, 171, 156, 197, 162, 246, 44];
    expected.extend_from_slice(&schedule_id.to_le_bytes());
    expected.extend_from_slice(&amount.to_le_bytes());
    expected.extend_from_slice(&start_ts.to_le_bytes());
    expected.extend_from_slice(&cliff_ts.to_le_bytes());
    expected.extend_from_slice(&end_ts.to_le_bytes());
    assert_eq!(ix.data, expected);

    // Accounts: exact order + flags, system_program auto-filled.
    assert_eq!(ix.accounts.len(), 4);
    assert_eq!(ix.accounts[0].pubkey, creator.pubkey());
    assert!(ix.accounts[0].is_signer && ix.accounts[0].is_writable);
    assert_eq!(ix.accounts[1].pubkey, beneficiary);
    assert!(!ix.accounts[1].is_signer && !ix.accounts[1].is_writable);
    assert_eq!(ix.accounts[2].pubkey, schedule);
    assert!(!ix.accounts[2].is_signer && ix.accounts[2].is_writable);
    assert_eq!(ix.accounts[3].pubkey, Address::default()); // system program
    assert!(!ix.accounts[3].is_signer && !ix.accounts[3].is_writable);
}

#[test]
fn argument_order_follows_the_idl_not_json_key_order() {
    let idl = real_idl();
    let scope = Scope::new(RPC);
    let creator = Keypair::new();

    // Supply args as one scrambled JSON object; encoder must still follow the
    // IDL's declared order (schedule_id, amount, start_ts, cliff_ts, end_ts).
    let ix = scope
        .program_with_idl(real_program(), idl)
        .method("create_vesting")
        .unwrap()
        .payer(&creator)
        .account_signer("creator", &creator)
        .account("beneficiary", Keypair::new().pubkey())
        .account("schedule", Keypair::new().pubkey())
        .args(json!({
            "end_ts": 5_i64,
            "amount": 2_u64,
            "cliff_ts": 4_i64,
            "schedule_id": 1_u64,
            "start_ts": 3_i64,
        }))
        .unwrap()
        .instruction()
        .unwrap();

    let mut expected = vec![135u8, 184, 171, 156, 197, 162, 246, 44];
    for v in [1u64, 2] {
        expected.extend_from_slice(&v.to_le_bytes());
    }
    for v in [3i64, 4, 5] {
        expected.extend_from_slice(&v.to_le_bytes());
    }
    assert_eq!(ix.data, expected);
}

// ---------------------------------------------------------------------------
// Borsh scalar encoding — exact bytes
// ---------------------------------------------------------------------------

#[test]
fn bool_is_one_byte() {
    assert_eq!(
        encode_ok(json!([{ "name": "b", "type": "bool" }]), json!([]), json!({ "b": true })),
        vec![1]
    );
    assert_eq!(
        encode_ok(json!([{ "name": "b", "type": "bool" }]), json!([]), json!({ "b": false })),
        vec![0]
    );
}

#[test]
fn unsigned_integers_are_little_endian_with_correct_widths() {
    assert_eq!(
        encode_ok(json!([{ "name": "x", "type": "u8" }]), json!([]), json!({ "x": 255 })),
        vec![255]
    );
    assert_eq!(
        encode_ok(json!([{ "name": "x", "type": "u16" }]), json!([]), json!({ "x": 258 })),
        258u16.to_le_bytes().to_vec()
    );
    assert_eq!(
        encode_ok(json!([{ "name": "x", "type": "u32" }]), json!([]), json!({ "x": 70_000 })),
        70_000u32.to_le_bytes().to_vec()
    );
    assert_eq!(
        encode_ok(json!([{ "name": "x", "type": "u64" }]), json!([]), json!({ "x": 9_000_000_000u64 })),
        9_000_000_000u64.to_le_bytes().to_vec()
    );
}

#[test]
fn u128_and_i128_accept_decimal_strings_at_the_extremes() {
    let u = encode_ok(
        json!([{ "name": "x", "type": "u128" }]),
        json!([]),
        json!({ "x": "340282366920938463463374607431768211455" }),
    );
    assert_eq!(u, u128::MAX.to_le_bytes().to_vec());

    let i = encode_ok(
        json!([{ "name": "x", "type": "i128" }]),
        json!([]),
        json!({ "x": "-170141183460469231731687303715884105728" }),
    );
    assert_eq!(i, i128::MIN.to_le_bytes().to_vec());
}

#[test]
fn signed_integers_encode_negatives_two_complement_le() {
    assert_eq!(
        encode_ok(json!([{ "name": "x", "type": "i8" }]), json!([]), json!({ "x": -1 })),
        vec![255]
    );
    assert_eq!(
        encode_ok(json!([{ "name": "x", "type": "i16" }]), json!([]), json!({ "x": -2 })),
        (-2i16).to_le_bytes().to_vec()
    );
    assert_eq!(
        encode_ok(json!([{ "name": "x", "type": "i64" }]), json!([]), json!({ "x": -1 })),
        (-1i64).to_le_bytes().to_vec()
    );
}

#[test]
fn floats_are_ieee754_little_endian() {
    assert_eq!(
        encode_ok(json!([{ "name": "x", "type": "f32" }]), json!([]), json!({ "x": 1.5 })),
        1.5f32.to_le_bytes().to_vec()
    );
    assert_eq!(
        encode_ok(json!([{ "name": "x", "type": "f64" }]), json!([]), json!({ "x": -2.25 })),
        (-2.25f64).to_le_bytes().to_vec()
    );
}

#[test]
fn pubkey_is_32_raw_bytes() {
    let key = Keypair::new().pubkey();
    let out = encode_ok(
        json!([{ "name": "k", "type": "pubkey" }]),
        json!([]),
        json!({ "k": key.to_string() }),
    );
    assert_eq!(out.len(), 32);
    assert_eq!(out.as_slice(), key.as_ref());
}

#[test]
fn string_length_prefix_is_byte_length_not_char_count() {
    // "héllo": h + é(2 bytes) + l + l + o = 6 bytes, 5 chars.
    let out = encode_ok(
        json!([{ "name": "s", "type": "string" }]),
        json!([]),
        json!({ "s": "héllo" }),
    );
    let mut expected = 6u32.to_le_bytes().to_vec();
    expected.extend_from_slice("héllo".as_bytes());
    assert_eq!(out, expected);
    assert_eq!(out.len(), 4 + 6);
}

#[test]
fn bytes_accept_array_and_hex_forms_with_u32_length_prefix() {
    let arr = encode_ok(
        json!([{ "name": "b", "type": "bytes" }]),
        json!([]),
        json!({ "b": [1, 2, 255] }),
    );
    let mut expected = 3u32.to_le_bytes().to_vec();
    expected.extend_from_slice(&[1, 2, 255]);
    assert_eq!(arr, expected);

    let hexed = encode_ok(
        json!([{ "name": "b", "type": "bytes" }]),
        json!([]),
        json!({ "b": "0xdeadbeef" }),
    );
    let mut expected = 4u32.to_le_bytes().to_vec();
    expected.extend_from_slice(&[0xde, 0xad, 0xbe, 0xef]);
    assert_eq!(hexed, expected);
}

// ---------------------------------------------------------------------------
// Compound types
// ---------------------------------------------------------------------------

#[test]
fn vec_is_u32_count_then_elements() {
    let out = encode_ok(
        json!([{ "name": "v", "type": { "vec": "u16" } }]),
        json!([]),
        json!({ "v": [258, 259] }),
    );
    let mut expected = 2u32.to_le_bytes().to_vec();
    expected.extend_from_slice(&258u16.to_le_bytes());
    expected.extend_from_slice(&259u16.to_le_bytes());
    assert_eq!(out, expected);

    // Empty vec is just the zero count.
    let empty = encode_ok(
        json!([{ "name": "v", "type": { "vec": "u8" } }]),
        json!([]),
        json!({ "v": [] }),
    );
    assert_eq!(empty, 0u32.to_le_bytes().to_vec());
}

#[test]
fn option_uses_a_one_byte_tag() {
    let some = encode_ok(
        json!([{ "name": "o", "type": { "option": "u8" } }]),
        json!([]),
        json!({ "o": 5 }),
    );
    assert_eq!(some, vec![1, 5]);

    let none = encode_ok(
        json!([{ "name": "o", "type": { "option": "u8" } }]),
        json!([]),
        json!({ "o": null }),
    );
    assert_eq!(none, vec![0]);
}

#[test]
fn fixed_array_has_no_length_prefix_and_enforces_exact_count() {
    let out = encode_ok(
        json!([{ "name": "a", "type": { "array": ["u8", 3] } }]),
        json!([]),
        json!({ "a": [1, 2, 3] }),
    );
    assert_eq!(out, vec![1, 2, 3]);

    // Wrong length is an error, never a panic.
    let err = encode(
        json!([{ "name": "a", "type": { "array": ["u8", 3] } }]),
        json!([]),
        json!({ "a": [1, 2] }),
    )
    .unwrap_err();
    assert!(matches!(err, Error::ArgumentEncoding { .. }), "{err:?}");
}

#[test]
fn defined_struct_encodes_fields_in_idl_order() {
    let types = json!([{
        "name": "Config",
        "type": { "kind": "struct", "fields": [
            { "name": "flag", "type": "bool" },
            { "name": "count", "type": "u16" },
        ]}
    }]);
    // Both the new ({defined:{name}}) and old ({defined:"Config"}) shapes.
    for defined in [json!({ "defined": { "name": "Config" } }), json!({ "defined": "Config" })] {
        let out = encode_ok(
            json!([{ "name": "c", "type": defined }]),
            types.clone(),
            json!({ "c": { "count": 258, "flag": true } }), // scrambled key order
        );
        // flag(1 byte) then count(2 bytes LE) — IDL order regardless of JSON order.
        assert_eq!(out, vec![1, 2, 1]);
    }
}

#[test]
fn defined_enum_unit_tuple_and_named_variants() {
    let types = json!([{
        "name": "Mode",
        "type": { "kind": "enum", "variants": [
            { "name": "Off" },
            { "name": "On", "fields": ["u8"] },
            { "name": "Range", "fields": [
                { "name": "lo", "type": "u8" },
                { "name": "hi", "type": "u8" }
            ]},
        ]}
    }]);
    let arg = |v: Value| {
        encode_ok(
            json!([{ "name": "m", "type": { "defined": { "name": "Mode" } } }]),
            types.clone(),
            json!({ "m": v }),
        )
    };
    // Unit variant by bare string -> just the variant index.
    assert_eq!(arg(json!("Off")), vec![0]);
    // Tuple variant -> index then positional payload.
    assert_eq!(arg(json!({ "On": [5] })), vec![1, 5]);
    // Named-field variant -> index then fields in order.
    assert_eq!(arg(json!({ "Range": { "lo": 7, "hi": 9 } })), vec![2, 7, 9]);
}

// ---------------------------------------------------------------------------
// Discriminator derivation (legacy, pre-0.30 IDLs with no discriminator)
// ---------------------------------------------------------------------------

#[test]
fn legacy_discriminator_is_sha256_of_global_snake_case() {
    // Synthetic method "m" (no discriminator in the IDL) -> sha256("global:m").
    let data = data_for(synth(json!([]), json!([])), "m", json!({})).unwrap();
    let expected = &Sha256::digest(b"global:m")[..8];
    assert_eq!(&data[..8], expected);
}

#[test]
fn legacy_discriminator_converts_camel_case_to_snake_case() {
    let idl = json!({
        "instructions": [{ "name": "setConfig", "accounts": [], "args": [] }]
    });
    let data = data_for(idl, "setConfig", json!({})).unwrap();
    let expected = &Sha256::digest(b"global:set_config")[..8];
    assert_eq!(&data[..8], expected);
}

// ---------------------------------------------------------------------------
// Builder validation errors (all pre-RPC, all typed, never panics)
// ---------------------------------------------------------------------------

#[test]
fn unknown_method_is_method_not_found() {
    let scope = Scope::new(RPC);
    // `method()` returns Result<MethodBuilder, _>; MethodBuilder isn't Debug, so
    // match on the result rather than unwrap_err().
    let result = scope
        .program_with_idl(real_program(), real_idl())
        .method("doesNotExist");
    assert!(matches!(result, Err(Error::MethodNotFound { .. })));
}

#[test]
fn missing_payer_is_reported() {
    let scope = Scope::new(RPC);
    let err = scope
        .program_with_idl(real_program(), real_idl())
        .method("increment_counter")
        .unwrap()
        .account("signer", Keypair::new().pubkey())
        .account("counter", Keypair::new().pubkey())
        .instruction()
        .unwrap_err();
    assert!(matches!(err, Error::MissingPayer { .. }), "{err:?}");
}

#[test]
fn missing_required_account_is_reported() {
    let scope = Scope::new(RPC);
    let payer = Keypair::new();
    let err = scope
        .program_with_idl(real_program(), real_idl())
        .method("increment_counter")
        .unwrap()
        .payer(&payer)
        .account_signer("signer", &payer)
        // omit "counter"
        .instruction()
        .unwrap_err();
    assert!(matches!(err, Error::MissingInstructionAccount { .. }), "{err:?}");
}

#[test]
fn account_marked_signer_without_a_signer_is_reported() {
    let scope = Scope::new(RPC);
    let payer = Keypair::new();
    // "signer" is a signer account; supply only its address, register no signer.
    let err = scope
        .program_with_idl(real_program(), real_idl())
        .method("increment_counter")
        .unwrap()
        .payer(&payer)
        .account("signer", Keypair::new().pubkey())
        .account("counter", Keypair::new().pubkey())
        .instruction()
        .unwrap_err();
    assert!(matches!(err, Error::MissingSigner { .. }), "{err:?}");
}

#[test]
fn missing_argument_is_reported() {
    let scope = Scope::new(RPC);
    let payer = Keypair::new();
    let err = scope
        .program_with_idl(real_program(), real_idl())
        .method("claim_vested")
        .unwrap()
        .payer(&payer)
        .account_signer("beneficiary", &payer)
        .account("schedule", Keypair::new().pubkey())
        // omit schedule_id
        .instruction()
        .unwrap_err();
    assert!(matches!(err, Error::MissingArgument { .. }), "{err:?}");
}

#[test]
fn unknown_argument_is_reported() {
    let scope = Scope::new(RPC);
    let payer = Keypair::new();
    let err = scope
        .program_with_idl(real_program(), real_idl())
        .method("claim_vested")
        .unwrap()
        .payer(&payer)
        .account_signer("beneficiary", &payer)
        .account("schedule", Keypair::new().pubkey())
        .arg("schedule_id", 1u64)
        .arg("not_a_real_arg", 5u64)
        .instruction()
        .unwrap_err();
    assert!(matches!(err, Error::UnknownArgument { .. }), "{err:?}");
}

#[test]
fn ambiguous_bare_leaf_account_is_rejected() {
    // Two nested groups both contain a leaf "vault". A bare .account("vault", X)
    // must NOT silently bind both — it is an ambiguity error.
    let idl = json!({
        "instructions": [{
            "name": "m",
            "accounts": [
                { "name": "groupA", "accounts": [
                    { "name": "vault", "writable": true, "signer": false }
                ]},
                { "name": "groupB", "accounts": [
                    { "name": "vault", "writable": true, "signer": false }
                ]},
            ],
            "args": []
        }]
    });
    let scope = Scope::new(RPC);
    let payer = Keypair::new();
    let err = scope
        .program_with_idl(Keypair::new().pubkey(), idl.clone())
        .method("m")
        .unwrap()
        .payer(&payer)
        .account("vault", Keypair::new().pubkey())
        .instruction()
        .unwrap_err();
    assert!(matches!(err, Error::AmbiguousField { .. }), "{err:?}");

    // Fully-qualified dotted names disambiguate and build fine.
    let a = Keypair::new().pubkey();
    let b = Keypair::new().pubkey();
    let ix = scope
        .program_with_idl(Keypair::new().pubkey(), idl)
        .method("m")
        .unwrap()
        .payer(&payer)
        .account("groupA.vault", a)
        .account("groupB.vault", b)
        .instruction()
        .unwrap();
    assert_eq!(ix.accounts.len(), 2);
    assert_eq!(ix.accounts[0].pubkey, a);
    assert_eq!(ix.accounts[1].pubkey, b);
}

#[test]
fn invalid_fixed_address_in_idl_is_reported() {
    let idl = json!({
        "instructions": [{
            "name": "m",
            "accounts": [
                { "name": "sys", "address": "not-a-valid-base58!!", "writable": false, "signer": false }
            ],
            "args": []
        }]
    });
    let scope = Scope::new(RPC);
    let payer = Keypair::new();
    let err = scope
        .program_with_idl(Keypair::new().pubkey(), idl)
        .method("m")
        .unwrap()
        .payer(&payer)
        .instruction()
        .unwrap_err();
    assert!(matches!(err, Error::InvalidAddress(_)), "{err:?}");
}

// ---------------------------------------------------------------------------
// Adversarial encoder inputs — must be typed errors, NEVER panics
// ---------------------------------------------------------------------------

fn expect_encoding_error(ty: Value, value: Value) {
    let err = encode(json!([{ "name": "x", "type": ty }]), json!([]), json!({ "x": value }))
        .expect_err("expected an encoding error");
    assert!(matches!(err, Error::ArgumentEncoding { .. }), "{err:?}");
}

#[test]
fn multibyte_hex_does_not_panic_and_is_an_error() {
    // "0x€0": the euro sign is 3 bytes, so after stripping "0x" the remainder is
    // 4 bytes (even) but not ASCII. Pre-fix this sliced at a non-char boundary
    // and PANICKED; it must now be a clean error.
    expect_encoding_error(json!("bytes"), json!("0x€0"));
}

#[test]
fn odd_and_non_hex_bytes_are_errors() {
    expect_encoding_error(json!("bytes"), json!("0xabc")); // odd length
    expect_encoding_error(json!("bytes"), json!("0xzz")); // not hex
    expect_encoding_error(json!("bytes"), json!([1, 2, 256])); // byte out of range
}

#[test]
fn out_of_range_integers_are_errors() {
    expect_encoding_error(json!("u8"), json!(256));
    expect_encoding_error(json!("u8"), json!(-1));
    expect_encoding_error(json!("i8"), json!(200));
}

#[test]
fn wrong_json_types_are_errors() {
    expect_encoding_error(json!("u64"), json!("not a number"));
    expect_encoding_error(json!("bool"), json!(5));
    expect_encoding_error(json!("pubkey"), json!("not base58 @@@"));
    expect_encoding_error(json!("string"), json!(123));
}

#[test]
fn malformed_enum_inputs_are_errors() {
    let types = json!([{
        "name": "Mode",
        "type": { "kind": "enum", "variants": [
            { "name": "Off" },
            { "name": "On", "fields": ["u8"] },
        ]}
    }]);
    let bad = |v: Value| {
        encode(
            json!([{ "name": "m", "type": { "defined": { "name": "Mode" } } }]),
            types.clone(),
            json!({ "m": v }),
        )
        .expect_err("expected enum encoding error")
    };
    // Unknown variant.
    assert!(matches!(bad(json!("Nope")), Error::ArgumentEncoding { .. }));
    // Object with more than one key.
    assert!(matches!(bad(json!({ "On": [1], "Off": null })), Error::ArgumentEncoding { .. }));
    // Tuple variant wrong arity.
    assert!(matches!(bad(json!({ "On": [1, 2] })), Error::ArgumentEncoding { .. }));
}

#[test]
fn missing_struct_field_is_an_error() {
    let types = json!([{
        "name": "Config",
        "type": { "kind": "struct", "fields": [
            { "name": "flag", "type": "bool" },
            { "name": "count", "type": "u16" },
        ]}
    }]);
    let err = encode(
        json!([{ "name": "c", "type": { "defined": { "name": "Config" } } }]),
        types,
        json!({ "c": { "flag": true } }), // count missing
    )
    .unwrap_err();
    assert!(matches!(err, Error::ArgumentEncoding { .. }), "{err:?}");
}

#[test]
fn unknown_defined_type_is_an_error() {
    let err = encode(
        json!([{ "name": "c", "type": { "defined": { "name": "Ghost" } } }]),
        json!([]),
        json!({ "c": {} }),
    )
    .unwrap_err();
    assert!(matches!(err, Error::ArgumentEncoding { .. }), "{err:?}");
}
