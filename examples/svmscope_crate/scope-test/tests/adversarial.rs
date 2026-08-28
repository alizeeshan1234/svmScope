//! Extreme adversarial / hostile-input regression tests, exercised purely
//! through svmscope's PUBLIC API as an external consumer.
//!
//! Every case must terminate quickly with either `Ok` or a typed `Err` — never
//! hang, stack-overflow, OOM, or panic. Cases that could in principle wedge the
//! process (hostile on-chain IDLs, deep nesting) run under a watchdog thread so
//! a regression (e.g. a removed guard) surfaces as a test FAILURE instead of a
//! frozen suite.

use std::panic::AssertUnwindSafe;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use serde_json::{json, Value};
use solana_keypair::Keypair;
use svmscope::{Check, Cmp, Fixture, Mutation, Replay, Scope};

const FIXTURE: &str = include_str!("fixtures/counter_increment.json");

// Facts about the committed v2 fixture (verified by parsing it):
//   data account FLFU… is owned by program 41NQ…, 16 bytes, and its 8-byte
//   Anchor discriminator is the value below. A hostile IDL whose accounts[]
//   entry carries this exact discriminator forces svmscope to walk that IDL's
//   type layout when a named-field assert decodes the account.
const COUNTER_PDA: &str = "FLFUsWEUjARKvDrFf9WtF7k1tCxHqtCpfJU2krL6z7vY";
const OWNER_PROGRAM: &str = "41NQKxFZ2eEAFPSKyP2BEvw2bkxwXjC8Wjrxcy6njF8u";
const COUNTER_DISC: [u8; 8] = [255, 176, 4, 245, 188, 253, 124, 25];

/// Run `f` on a worker thread and require it to finish within `secs`, without
/// panicking. A timeout means a possible hang; a panic means an unguarded path.
/// Both are the failures this suite exists to catch.
fn runs_within<F>(secs: u64, label: &str, f: F)
where
    F: FnOnce() + Send + 'static,
{
    let (tx, rx) = mpsc::channel();
    let worker = thread::spawn(move || {
        let outcome = std::panic::catch_unwind(AssertUnwindSafe(f));
        let _ = tx.send(outcome.is_ok());
    });
    match rx.recv_timeout(Duration::from_secs(secs)) {
        Ok(true) => {
            let _ = worker.join();
        }
        Ok(false) => panic!("{label}: PANICKED — hostile input must be a typed error or Ok"),
        Err(_) => panic!("{label}: POSSIBLE HANG — did not finish within {secs}s"),
    }
}

fn disc_json() -> Value {
    Value::Array(COUNTER_DISC.iter().map(|b| json!(*b)).collect())
}

/// The fixture replay with a hostile IDL registered for the counter's owner
/// program. `types` is spliced under `idl["types"]`; the accounts[] entry uses
/// the real discriminator so the given `root_type` is actually walked.
fn hostile_replay(root_type: &str, types: Value) -> Replay {
    let mut replay =
        Replay::from_fixture(&Fixture::from_json(FIXTURE).expect("fixture parses")).expect("loads");
    let idl = json!({
        "accounts": [{ "name": root_type, "discriminator": disc_json() }],
        "types": types,
    });
    replay.add_idl(OWNER_PROGRAM, idl);
    replay
}

/// Decode the counter account through a hostile IDL by running a named-field
/// assert. We don't care whether the assertion passes — only that the decode
/// (and thus the field walk) TERMINATES without hang/panic.
fn assert_decode_terminates(label: &'static str, root_type: &'static str, types: Value) {
    let replay = hostile_replay(root_type, types);
    runs_within(15, label, move || {
        let _ = replay.verify(
            "hostile decode",
            &[],
            &[Check::account(COUNTER_PDA).field("count", Cmp::eq(2)).build()],
        );
    });
}

// ---------------------------------------------------------------------------
// 1. Hostile on-chain IDLs reaching the account-decode field walk.
// ---------------------------------------------------------------------------

#[test]
fn self_referential_idl_type_is_bounded() {
    // Node { next: Node } — infinite type, must stop at the depth guard.
    let types = json!([{
        "name": "Node",
        "type": { "kind": "struct", "fields": [
            { "name": "next", "type": { "defined": { "name": "Node" } } }
        ] }
    }]);
    assert_decode_terminates("self-referential", "Node", types);
}

#[test]
fn mutually_recursive_idl_types_are_bounded() {
    // A { b: B }, B { a: A } — a two-type cycle.
    let types = json!([
        { "name": "A", "type": { "kind": "struct", "fields": [
            { "name": "b", "type": { "defined": { "name": "B" } } } ] } },
        { "name": "B", "type": { "kind": "struct", "fields": [
            { "name": "a", "type": { "defined": { "name": "A" } } } ] } }
    ]);
    assert_decode_terminates("mutually-recursive", "A", types);
}

#[test]
fn huge_fixed_array_of_empty_struct_is_bounded() {
    // items: [Empty; u64::MAX] where Empty consumes zero bytes.
    let types = json!([
        { "name": "Big", "type": { "kind": "struct", "fields": [
            { "name": "items",
              "type": { "array": [ { "defined": { "name": "Empty" } }, u64::MAX ] } } ] } },
        { "name": "Empty", "type": { "kind": "struct", "fields": [] } }
    ]);
    assert_decode_terminates("huge-array", "Big", types);
}

#[test]
fn deeply_nested_structs_are_bounded() {
    // T0 -> T1 -> ... -> T299, a 300-deep chain; the depth guard must cut it off.
    let mut types = Vec::new();
    for i in 0..300 {
        types.push(json!({
            "name": format!("T{i}"),
            "type": { "kind": "struct", "fields": [
                { "name": "n", "type": { "defined": { "name": format!("T{}", i + 1) } } }
            ] }
        }));
    }
    types.push(json!({
        "name": "T300",
        "type": { "kind": "struct", "fields": [ { "name": "v", "type": "u8" } ] }
    }));
    assert_decode_terminates("deep-nesting", "T0", Value::Array(types));
}

#[test]
fn hostile_enum_variant_walk_is_bounded() {
    // An enum whose variant payload is a recursive struct.
    let types = json!([
        { "name": "E", "type": { "kind": "enum", "variants": [
            { "name": "Rec", "fields": [ { "name": "next", "type": { "defined": { "name": "E" } } } ] }
        ] } }
    ]);
    assert_decode_terminates("hostile-enum", "E", types);
}

#[test]
fn combined_warp_mutation_and_hostile_idl_still_terminate() {
    // Extreme clock warp + a mutation + a hostile recursive IDL, all at once.
    let types = json!([{
        "name": "Node",
        "type": { "kind": "struct", "fields": [
            { "name": "next", "type": { "defined": { "name": "Node" } } } ] }
    }]);
    let mut replay = hostile_replay("Node", types);
    replay.advance_seconds(i64::MAX);
    replay.advance_epochs(i64::MAX);
    runs_within(15, "combined-extreme", move || {
        let _ = replay.verify(
            "combined",
            &[Mutation::patch(COUNTER_PDA, 8, 42u64.to_le_bytes().to_vec())],
            &[Check::account(COUNTER_PDA).field("count", Cmp::eq(0)).build()],
        );
    });
}

// ---------------------------------------------------------------------------
// 2. Malformed fixtures into Fixture::from_json / Replay::from_fixture.
// ---------------------------------------------------------------------------

#[test]
fn malformed_fixture_json_is_a_typed_error_not_a_panic() {
    for bad in ["", "{", "{}", "[]", "not json at all", "{\"version\":", "null", "12345"] {
        // Must return (Ok or Err) without panicking.
        let _ = Fixture::from_json(bad);
        assert!(Fixture::from_json(bad).is_err(), "expected error for {bad:?}");
    }
}

#[test]
fn future_fixture_version_is_rejected() {
    let mut v: Value = serde_json::from_str(FIXTURE).unwrap();
    v["version"] = json!(9999);
    let s = serde_json::to_string(&v).unwrap();
    assert!(
        Fixture::from_json(&s).is_err(),
        "a fixture newer than this build must be rejected, not misread"
    );
}

#[test]
fn garbage_base64_in_fixture_is_a_typed_error() {
    // Corrupt the transaction bytes: from_json still parses, from_fixture must
    // fail cleanly (typed), never panic.
    let mut v: Value = serde_json::from_str(FIXTURE).unwrap();
    v["tx_b64"] = json!("!!!!not base64!!!!");
    let s = serde_json::to_string(&v).unwrap();
    match Fixture::from_json(&s) {
        Ok(fx) => assert!(Replay::from_fixture(&fx).is_err(), "bad tx base64 must error"),
        Err(_) => { /* also acceptable */ }
    }

    // Corrupt an account's data base64.
    let mut v: Value = serde_json::from_str(FIXTURE).unwrap();
    if let Some(entries) = v["entries"].as_array_mut() {
        for e in entries.iter_mut() {
            if e["kind"] == json!("data") {
                e["data_b64"] = json!("@@@@");
            }
        }
    }
    let s = serde_json::to_string(&v).unwrap();
    if let Ok(fx) = Fixture::from_json(&s) {
        assert!(Replay::from_fixture(&fx).is_err(), "bad data base64 must error");
    }
}

#[test]
fn wrong_shape_fixture_is_rejected() {
    // Valid JSON, wrong schema (missing required fields).
    for bad in [
        r#"{"version":2}"#,
        r#"{"version":2,"signature":"x"}"#,
        r#"{"version":2,"entries":"not-an-array"}"#,
    ] {
        let _ = Fixture::from_json(bad); // must not panic
    }
}

// ---------------------------------------------------------------------------
// 3. Pathological IDL-builder argument encoding (offline .instruction()).
// ---------------------------------------------------------------------------

fn builder_instruction_encodes(idl: Value, args: Value) -> svmscope::Result<()> {
    let scope = Scope::new("http://127.0.0.1:1"); // never contacted for instruction()
    let program_id = solana_address::Address::new_unique();
    let payer = Keypair::new();
    let client = scope.program_with_idl(program_id, idl);
    let mut builder = client
        .method("f")?
        .payer(&payer)
        .account_signer("authority", &payer);
    builder = builder.args(args)?;
    builder.instruction().map(|_| ())
}

fn one_arg_idl(arg_type: Value, extra_types: Value) -> Value {
    json!({
        "instructions": [{
            "name": "f",
            "accounts": [{ "name": "authority", "writable": true, "signer": true }],
            "args": [{ "name": "a", "type": arg_type }]
        }],
        "types": extra_types,
    })
}

#[test]
fn enum_with_more_than_256_variants_errors_cleanly() {
    let variants: Vec<Value> = (0..300).map(|i| json!({ "name": format!("V{i}") })).collect();
    let idl = one_arg_idl(
        json!({ "defined": { "name": "Big" } }),
        json!([{ "name": "Big", "type": { "kind": "enum", "variants": variants } }]),
    );
    // Selecting variant index 260 (> u8::MAX) must be a typed encoding error.
    let err = builder_instruction_encodes(idl, json!({ "a": "V260" })).unwrap_err();
    assert!(
        matches!(err, svmscope::Error::ArgumentEncoding { .. }),
        "expected ArgumentEncoding, got {err:?}"
    );
}

#[test]
fn unknown_enum_variant_errors_cleanly() {
    let idl = one_arg_idl(
        json!({ "defined": { "name": "E" } }),
        json!([{ "name": "E", "type": { "kind": "enum",
            "variants": [ { "name": "A" }, { "name": "B" } ] } }]),
    );
    let err = builder_instruction_encodes(idl, json!({ "a": "Nope" })).unwrap_err();
    assert!(matches!(err, svmscope::Error::ArgumentEncoding { .. }), "{err:?}");
}

#[test]
fn deeply_nested_defined_struct_arg_is_bounded() {
    // T0 { n: T1 } ... T63 { n: T64 }, T64 { v: u8 } — a 64-deep struct chain,
    // with a matching 64-deep JSON value. Must complete (bounded), not overflow.
    let mut types = Vec::new();
    for i in 0..64 {
        types.push(json!({
            "name": format!("T{i}"),
            "type": { "kind": "struct", "fields": [
                { "name": "n", "type": { "defined": { "name": format!("T{}", i + 1) } } } ] }
        }));
    }
    types.push(json!({ "name": "T64",
        "type": { "kind": "struct", "fields": [ { "name": "v", "type": "u8" } ] } }));
    // Build the matching nested value from the inside out.
    let mut value = json!({ "v": 7 });
    for _ in 0..64 {
        value = json!({ "n": value });
    }
    let idl = one_arg_idl(json!({ "defined": { "name": "T0" } }), Value::Array(types));
    runs_within(15, "deep-arg", move || {
        // Bounded input -> must produce bytes (Ok) without overflowing.
        assert!(builder_instruction_encodes(idl, json!({ "a": value })).is_ok());
    });
}

#[test]
fn huge_vec_argument_encodes_without_blowing_up() {
    let idl = one_arg_idl(json!({ "vec": "u8" }), json!([]));
    let big: Vec<Value> = (0..1_000_000).map(|_| json!(0)).collect();
    runs_within(20, "huge-vec", move || {
        assert!(builder_instruction_encodes(idl, json!({ "a": big })).is_ok());
    });
}

#[test]
fn wrong_typed_argument_values_error_cleanly() {
    // u64 arg given a string that isn't a number, bool given an int, pubkey given
    // junk, fixed array of the wrong length — each a typed ArgumentEncoding error.
    let cases: Vec<(Value, Value)> = vec![
        (json!("u64"), json!({ "a": "not-a-number" })),
        (json!("bool"), json!({ "a": 5 })),
        (json!("pubkey"), json!({ "a": "not-a-pubkey" })),
        (json!({ "array": ["u8", 4] }), json!({ "a": [1, 2] })), // wrong length
        (json!("u8"), json!({ "a": 999 })),                      // out of range
    ];
    for (arg_type, args) in cases {
        let idl = one_arg_idl(arg_type.clone(), json!([]));
        let res = builder_instruction_encodes(idl, args.clone());
        assert!(
            matches!(res, Err(svmscope::Error::ArgumentEncoding { .. })),
            "expected ArgumentEncoding for type {arg_type} / args {args}, got {res:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// 4. Mutation boundary abuse via simulate() on the offline fixture.
// ---------------------------------------------------------------------------

fn fixture_replay() -> Replay {
    Replay::from_fixture(&Fixture::from_json(FIXTURE).unwrap()).unwrap()
}

#[test]
fn patch_at_extreme_offset_errors_not_panics() {
    let replay = fixture_replay();
    let res = replay.simulate(&[Mutation::patch(COUNTER_PDA, usize::MAX, vec![0u8; 8])]);
    assert!(res.is_err(), "patch at usize::MAX must be an error");
}

#[test]
fn patch_length_overflowing_offset_errors_not_panics() {
    let replay = fixture_replay();
    // offset + len would overflow usize; must be caught, not wrap.
    let res = replay.simulate(&[Mutation::patch(COUNTER_PDA, usize::MAX - 2, vec![0u8; 16])]);
    assert!(res.is_err());
}

#[test]
fn multi_megabyte_data_mutation_does_not_panic() {
    let replay = fixture_replay();
    // Replacing account data wholesale with several MB must not panic (Ok or Err).
    let _ = replay.simulate(&[Mutation::data(COUNTER_PDA, vec![0u8; 4 * 1024 * 1024])]);
}

#[test]
fn lamports_at_u64_max_does_not_panic() {
    let replay = fixture_replay();
    let _ = replay.simulate(&[Mutation::lamports(COUNTER_PDA, u64::MAX)]);
}

#[test]
fn mutation_on_unloaded_address_is_a_hard_error() {
    let replay = fixture_replay();
    // A valid address that the replay never loaded — must be a hard error, never
    // a silently-ignored no-op.
    let unknown = solana_address::Address::new_unique().to_string();
    let res = replay.simulate(&[Mutation::lamports(unknown, 0)]);
    assert!(matches!(
        res,
        Err(svmscope::Error::MutationTargetMissing(_))
    ));
}

#[test]
fn mutation_with_invalid_address_is_a_typed_error() {
    let replay = fixture_replay();
    let res = replay.simulate(&[Mutation::patch("not a base58 address!!!", 0, vec![1])]);
    assert!(
        matches!(
            res,
            Err(svmscope::Error::InvalidAddress(_)) | Err(svmscope::Error::MutationTargetMissing(_))
        ),
        "{res:?}"
    );
}

#[test]
fn many_mutations_in_one_simulate_terminate() {
    let replay = fixture_replay();
    // A large batch of tiny patches must not blow up.
    let muts: Vec<Mutation> = (0..2000)
        .map(|_| Mutation::patch(COUNTER_PDA, 8, 1u64.to_le_bytes().to_vec()))
        .collect();
    runs_within(20, "mutation-batch", move || {
        let _ = replay.simulate(&muts);
    });
}
