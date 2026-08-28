//! EXTREME regression tests — external-consumer view of svmscope's PUBLIC API.
//!
//! Everything here is OFFLINE (no network, no validator) and exercises only what
//! a `cargo add --dev svmscope` user can reach: the crate-root helpers, the
//! `svmscope::spec` JSON wire types, the `svmscope::idl` extractor, the
//! Check/Cmp/Scenario builders, and the error type. A recurring goal is to feed
//! garbage at every parser and prove it returns `Ok` or a typed `Err` — never a
//! panic.
//!
//! Visibility note (verified against the crate source): `AssertInput::into_assert`
//! is private and `Check`'s inner `CheckKind` / `StateCheck` / `Expect` / `CmpOp`
//! are not re-exported, so assert-kind/op resolution is exercised through the
//! PUBLIC `ScenarioInput::into_scenario`, and Checks are built via the public
//! builder methods. That is exactly the surface a real consumer has.

use svmscope::spec::{AssertInput, FeatureInput, MutationInput, ScenarioInput, SuiteRequest};
use svmscope::{resolve_rpc_url, Check, Cmp, Error, Mutation, Scenario};

// --------------------------------------------------------------------------
// resolve_rpc_url — every branch, precedence, normalization
// --------------------------------------------------------------------------

const DEF: &str = "http://default.example";

#[test]
fn resolve_rpc_url_cluster_names() {
    let m = "https://api.mainnet-beta.solana.com";
    for c in ["mainnet", "mainnet-beta", "m", "MAINNET", "  Mainnet  "] {
        assert_eq!(resolve_rpc_url(Some(c), None, DEF).unwrap(), m, "cluster {c:?}");
    }
    for c in ["devnet", "d", "DevNet"] {
        assert_eq!(
            resolve_rpc_url(Some(c), None, DEF).unwrap(),
            "https://api.devnet.solana.com",
            "cluster {c:?}"
        );
    }
    for c in ["testnet", "t"] {
        assert_eq!(
            resolve_rpc_url(Some(c), None, DEF).unwrap(),
            "https://api.testnet.solana.com"
        );
    }
    for c in ["localnet", "local", "localhost", "l"] {
        assert_eq!(
            resolve_rpc_url(Some(c), None, DEF).unwrap(),
            "http://127.0.0.1:8899",
            "cluster {c:?}"
        );
    }
}

#[test]
fn resolve_rpc_url_default_when_unset() {
    assert_eq!(resolve_rpc_url(None, None, DEF).unwrap(), DEF);
    assert_eq!(resolve_rpc_url(Some(""), None, DEF).unwrap(), DEF);
    assert_eq!(resolve_rpc_url(Some("   "), None, DEF).unwrap(), DEF);
}

#[test]
fn resolve_rpc_url_explicit_http_rpc_wins() {
    // An explicit http(s) rpc beats any cluster.
    assert_eq!(
        resolve_rpc_url(Some("devnet"), Some("http://my-node:8899"), DEF).unwrap(),
        "http://my-node:8899"
    );
    assert_eq!(
        resolve_rpc_url(Some("mainnet"), Some("https://rpc.example/x"), DEF).unwrap(),
        "https://rpc.example/x"
    );
}

#[test]
fn resolve_rpc_url_cluster_starting_with_http_is_verbatim() {
    assert_eq!(
        resolve_rpc_url(Some("http://cluster-as-url:1234"), None, DEF).unwrap(),
        "http://cluster-as-url:1234"
    );
}

#[test]
fn resolve_rpc_url_non_http_rpc_is_ignored() {
    // Documented behavior: a non-http `rpc` value is IGNORED (not used verbatim),
    // falling through to the cluster/default.
    assert_eq!(resolve_rpc_url(None, Some("garbage"), DEF).unwrap(), DEF);
    assert_eq!(
        resolve_rpc_url(Some("devnet"), Some("localhost:8899"), DEF).unwrap(),
        "https://api.devnet.solana.com"
    );
}

#[test]
fn resolve_rpc_url_unknown_cluster_is_typed_error() {
    let err = resolve_rpc_url(Some("nope"), None, DEF).unwrap_err();
    assert!(matches!(err, Error::InvalidSpec(_)));
    assert!(!err.to_string().is_empty());
}

// --------------------------------------------------------------------------
// MutationInput — parse + into_mutation, hex tolerance & rejection
// --------------------------------------------------------------------------

fn mutation(json: &str) -> Result<Mutation, Error> {
    serde_json::from_str::<MutationInput>(json)
        .expect("valid MutationInput JSON")
        .into_mutation()
}

#[test]
fn mutation_lamports_roundtrips() {
    match mutation(r#"{"kind":"lamports","address":"AAA","lamports":42}"#).unwrap() {
        Mutation::Lamports { address, value } => {
            assert_eq!(address, "AAA");
            assert_eq!(value, 42);
        }
        other => panic!("expected Lamports, got {other:?}"),
    }
}

#[test]
fn mutation_lamports_accepts_u64_max() {
    let j = format!(r#"{{"kind":"lamports","address":"A","lamports":{}}}"#, u64::MAX);
    match mutation(&j).unwrap() {
        Mutation::Lamports { value, .. } => assert_eq!(value, u64::MAX),
        other => panic!("expected Lamports, got {other:?}"),
    }
}

#[test]
fn mutation_data_patch_roundtrips() {
    match mutation(r#"{"kind":"data","address":"B","offset":64,"bytes_hex":"00ff10"}"#).unwrap() {
        Mutation::DataPatch { address, offset, bytes } => {
            assert_eq!(address, "B");
            assert_eq!(offset, 64);
            assert_eq!(bytes, vec![0x00, 0xff, 0x10]);
        }
        other => panic!("expected DataPatch, got {other:?}"),
    }
}

#[test]
fn mutation_hex_tolerates_prefix_spaces_underscores_and_case() {
    for hx in ["0xDEADbeef", "DE AD BE EF", "de_ad_be_ef", "0xDE_AD BE_ef"] {
        let j = format!(r#"{{"kind":"data","address":"A","offset":0,"bytes_hex":"{hx}"}}"#);
        match mutation(&j).unwrap() {
            Mutation::DataPatch { bytes, .. } => {
                assert_eq!(bytes, vec![0xde, 0xad, 0xbe, 0xef], "hex {hx:?}")
            }
            other => panic!("expected DataPatch, got {other:?}"),
        }
    }
}

#[test]
fn mutation_hex_rejects_bad_input_without_panicking() {
    for hx in ["", "abc", "zz", "0x", "0xG0", "  "] {
        let j = format!(r#"{{"kind":"data","address":"A","offset":0,"bytes_hex":"{hx}"}}"#);
        assert!(mutation(&j).is_err(), "hex {hx:?} should be a typed error");
    }
}

#[test]
fn mutation_hex_multibyte_is_error_not_panic() {
    // The audit-fixed panic: a multibyte char has even byte length but no ASCII
    // char boundary at the 2-byte slice points. Must be an Err, never a panic.
    for hx in ["a\u{03a9}b", "0x\u{20ac}\u{20ac}", "\u{00ff}\u{00ff}", "\u{1f600}\u{1f600}"] {
        let j = serde_json::json!({
            "kind": "data", "address": "A", "offset": 0, "bytes_hex": hx
        })
        .to_string();
        assert!(mutation(&j).is_err(), "multibyte hex {hx:?} must be Err");
    }
}

#[test]
fn mutation_rejects_out_of_range_and_negative_lamports_at_parse() {
    // u64 field: negative or > u64::MAX is a clean serde error, not a panic.
    assert!(serde_json::from_str::<MutationInput>(
        r#"{"kind":"lamports","address":"A","lamports":-1}"#
    )
    .is_err());
    assert!(serde_json::from_str::<MutationInput>(
        r#"{"kind":"lamports","address":"A","lamports":99999999999999999999999}"#
    )
    .is_err());
}

#[test]
fn mutation_unknown_kind_is_parse_error() {
    assert!(serde_json::from_str::<MutationInput>(
        r#"{"kind":"teleport","address":"A"}"#
    )
    .is_err());
}

// --------------------------------------------------------------------------
// AssertInput — parse layer (public fields) + defaults
// --------------------------------------------------------------------------

#[test]
fn assert_input_parses_public_fields_and_defaults() {
    let a: AssertInput =
        serde_json::from_str(r#"{"address":"X","kind":"field","field":"pool.a","op":">=","value":7}"#)
            .unwrap();
    assert_eq!(a.address, "X");
    assert_eq!(a.kind, "field");
    assert_eq!(a.field.as_deref(), Some("pool.a"));
    assert_eq!(a.op, ">=");
    assert_eq!(a.value, 7);

    // Defaults: kind -> "u64", op -> "==", offset -> 0, field -> None.
    let d: AssertInput = serde_json::from_str(r#"{"address":"Y","value":3}"#).unwrap();
    assert_eq!(d.kind, "u64");
    assert_eq!(d.op, "==");
    assert_eq!(d.offset, 0);
    assert!(d.field.is_none());
}

#[test]
fn assert_input_value_beyond_i64_is_serde_error() {
    assert!(serde_json::from_str::<AssertInput>(
        r#"{"address":"X","value":99999999999999999999999}"#
    )
    .is_err());
    // Missing required `value`.
    assert!(serde_json::from_str::<AssertInput>(r#"{"address":"X"}"#).is_err());
}

// --------------------------------------------------------------------------
// Assert kind/op resolution via the PUBLIC ScenarioInput::into_scenario
// --------------------------------------------------------------------------

/// Build a one-assert scenario and return whether conversion succeeded.
fn scenario_with_assert(assert_json: &str) -> Result<Scenario, Error> {
    let j = format!(r#"{{"name":"s","asserts":[{assert_json}]}}"#);
    serde_json::from_str::<ScenarioInput>(&j)
        .expect("valid ScenarioInput JSON")
        .into_scenario()
}

#[test]
fn every_assert_kind_converts() {
    let ok = [
        r#"{"address":"X","kind":"lamports","value":1}"#,
        r#"{"address":"X","kind":"u64","offset":8,"value":1}"#,
        r#"{"address":"X","kind":"token_amount","value":1}"#,
        r#"{"address":"X","kind":"lamports_delta","value":-1}"#,
        r#"{"address":"X","kind":"token_delta","value":-1}"#,
        r#"{"address":"X","kind":"field","field":"count","value":1}"#,
        r#"{"address":"X","kind":"field_delta","field":"count","value":-1}"#,
    ];
    for a in ok {
        let scenario = scenario_with_assert(a).unwrap_or_else(|e| panic!("{a} -> {e}"));
        // 1 implicit outcome check + 1 account check.
        assert_eq!(scenario.checks.len(), 2, "{a}");
    }
}

#[test]
fn every_comparison_operator_and_alias_converts() {
    for op in ["==", "!=", "<", "<=", ">", ">=", "eq", "ne", "lt", "le", "gt", "ge"] {
        let a = format!(r#"{{"address":"X","kind":"u64","op":"{op}","value":5}}"#);
        assert!(scenario_with_assert(&a).is_ok(), "op {op:?}");
    }
}

#[test]
fn unknown_op_and_unknown_kind_are_errors() {
    assert!(matches!(
        scenario_with_assert(r#"{"address":"X","op":"~=","value":1}"#),
        Err(Error::InvalidSpec(_))
    ));
    assert!(matches!(
        scenario_with_assert(r#"{"address":"X","kind":"balancez","value":1}"#),
        Err(Error::InvalidSpec(_))
    ));
}

#[test]
fn field_kinds_require_a_field_name() {
    assert!(scenario_with_assert(r#"{"address":"X","kind":"field","value":1}"#).is_err());
    assert!(scenario_with_assert(r#"{"address":"X","kind":"field_delta","value":1}"#).is_err());
    // A blank/whitespace field name is also rejected.
    assert!(scenario_with_assert(r#"{"address":"X","kind":"field","field":"  ","value":1}"#).is_err());
}

#[test]
fn non_delta_kinds_reject_negative_values_deltas_accept_them() {
    // Non-delta: negative -> Err.
    for kind in ["lamports", "u64", "token_amount"] {
        let a = format!(r#"{{"address":"X","kind":"{kind}","value":-1}}"#);
        assert!(scenario_with_assert(&a).is_err(), "kind {kind} negative");
    }
    // Delta: negative -> Ok.
    for kind in ["lamports_delta", "token_delta", "field_delta"] {
        let field = if kind == "field_delta" { r#","field":"c""# } else { "" };
        let a = format!(r#"{{"address":"X","kind":"{kind}"{field},"value":-9}}"#);
        assert!(scenario_with_assert(&a).is_ok(), "kind {kind} negative");
    }
}

#[test]
fn assert_value_boundaries_convert() {
    // i64::MAX for an unsigned kind fits u64; i64::MIN for a delta kind is fine.
    let hi = format!(r#"{{"address":"X","kind":"u64","value":{}}}"#, i64::MAX);
    assert!(scenario_with_assert(&hi).is_ok());
    let lo = format!(r#"{{"address":"X","kind":"lamports_delta","value":{}}}"#, i64::MIN);
    assert!(scenario_with_assert(&lo).is_ok());
}

// --------------------------------------------------------------------------
// ScenarioInput — expect handling, mutations, defaults
// --------------------------------------------------------------------------

fn scenario(json: &str) -> Result<Scenario, Error> {
    serde_json::from_str::<ScenarioInput>(json)
        .expect("valid ScenarioInput JSON")
        .into_scenario()
}

#[test]
fn expect_values_map_and_default_is_any() {
    for e in ["success", "pass", "revert", "fail", "any"] {
        let j = format!(r#"{{"name":"s","expect":"{e}"}}"#);
        assert!(scenario(&j).is_ok(), "expect {e:?}");
    }
    // Omitted expect -> default "any" -> Ok.
    assert!(scenario(r#"{"name":"s"}"#).is_ok());
    // Surrounding whitespace tolerated.
    assert!(scenario(r#"{"name":"s","expect":"  revert  "}"#).is_ok());
}

#[test]
fn unknown_expect_is_error_not_a_vacuous_pass() {
    // The headline no-silent-pass guarantee.
    for bad in ["sucess", "reverts", "true", "SUCCESS", ""] {
        let j = format!(r#"{{"name":"s","expect":"{bad}"}}"#);
        assert!(
            matches!(scenario(&j), Err(Error::InvalidSpec(_))),
            "expect {bad:?} must be an error"
        );
    }
}

#[test]
fn revert_with_contains_and_mutations_convert() {
    let s = scenario(
        r#"{"name":"drain","expect":"revert","contains":"Slippage",
            "mutations":[{"kind":"lamports","address":"V","lamports":0}]}"#,
    )
    .unwrap();
    assert_eq!(s.name, "drain");
    assert_eq!(s.mutations.len(), 1);
    // outcome check present.
    assert_eq!(s.checks.len(), 1);
}

#[test]
fn scenario_missing_name_is_parse_error() {
    assert!(serde_json::from_str::<ScenarioInput>(r#"{"expect":"any"}"#).is_err());
}

#[test]
fn scenario_with_a_bad_nested_mutation_fails_conversion() {
    // Parses fine, but the multibyte hex fails at into_mutation time.
    let j = serde_json::json!({
        "name": "s",
        "mutations": [{"kind":"data","address":"A","offset":0,"bytes_hex":"a\u{03a9}b"}]
    })
    .to_string();
    assert!(scenario(&j).is_err());
}

// --------------------------------------------------------------------------
// SuiteRequest — full-shape parsing (runner-time validation is separate)
// --------------------------------------------------------------------------

#[test]
fn suite_request_parses_full_shape() {
    let req: SuiteRequest = serde_json::from_str(
        r#"{
            "signature": "sig123",
            "cluster": "devnet",
            "time_travel": {"seconds": 100},
            "features": [{"id":"So11111111111111111111111111111111111111112","active":true}],
            "scenarios": [{"name":"a","expect":"success"}]
        }"#,
    )
    .unwrap();
    assert_eq!(req.signature.as_deref(), Some("sig123"));
    assert_eq!(req.cluster.as_deref(), Some("devnet"));
    assert!(req.fixture.is_none());
    assert_eq!(req.features.len(), 1);
    assert_eq!(req.scenarios.len(), 1);
}

#[test]
fn suite_request_neither_fixture_nor_signature_still_parses() {
    // The "must specify fixture or signature" error is raised by the runner, not
    // the parser — parsing an under-specified suite must still succeed.
    let req: SuiteRequest = serde_json::from_str(r#"{"scenarios":[]}"#).unwrap();
    assert!(req.signature.is_none() && req.fixture.is_none());
    assert!(req.scenarios.is_empty());
}

#[test]
fn suite_request_requires_scenarios_field() {
    // `scenarios` has no default -> a suite without it is a parse error.
    assert!(serde_json::from_str::<SuiteRequest>(r#"{"signature":"x"}"#).is_err());
}

// --------------------------------------------------------------------------
// FeatureInput / feature_toggles
// --------------------------------------------------------------------------

#[test]
fn feature_input_valid_pubkey_toggles() {
    let f: FeatureInput =
        serde_json::from_str(r#"{"id":"So11111111111111111111111111111111111111112","active":true}"#)
            .unwrap();
    assert!(f.into_toggle().is_ok());
    // active defaults to false when omitted.
    let f2: FeatureInput =
        serde_json::from_str(r#"{"id":"So11111111111111111111111111111111111111112"}"#).unwrap();
    assert!(f2.into_toggle().is_ok());
}

#[test]
fn feature_input_bad_pubkey_is_error() {
    for bad in ["not-a-pubkey", "", "!!!", "0"] {
        let f: FeatureInput =
            serde_json::from_str(&serde_json::json!({"id": bad}).to_string()).unwrap();
        assert!(matches!(f.into_toggle(), Err(Error::InvalidSpec(_))), "id {bad:?}");
    }
}

#[test]
fn feature_toggles_fails_on_first_bad_id() {
    let features: Vec<FeatureInput> = serde_json::from_str(
        r#"[{"id":"So11111111111111111111111111111111111111112"},{"id":"garbage"}]"#,
    )
    .unwrap();
    assert!(svmscope::spec::feature_toggles(features).is_err());

    let good: Vec<FeatureInput> = serde_json::from_str(
        r#"[{"id":"So11111111111111111111111111111111111111112","active":true}]"#,
    )
    .unwrap();
    assert_eq!(svmscope::spec::feature_toggles(good).unwrap().len(), 1);
}

// --------------------------------------------------------------------------
// svmscope::idl::instructions — real IDL + malformed IDLs
// --------------------------------------------------------------------------

const REAL_IDL: &str = include_str!("../../target/idl/svmscope_crate.json");

#[test]
fn real_idl_extracts_all_five_instructions() {
    let idl: serde_json::Value = serde_json::from_str(REAL_IDL).unwrap();
    let ixs = svmscope::idl::instructions(&idl);
    assert_eq!(ixs.len(), 5, "expected 5 instructions");

    let names: Vec<&str> = ixs.iter().map(|i| i.name.as_str()).collect();
    for expected in [
        "claim_vested",
        "close_vesting",
        "create_vesting",
        "increment_counter",
        "initialize_counter",
    ] {
        assert!(names.contains(&expected), "missing {expected}");
    }
    // Every Anchor discriminator is exactly 8 bytes.
    for ix in &ixs {
        assert_eq!(ix.discriminator.len(), 8, "{}", ix.name);
    }
}

#[test]
fn real_idl_create_vesting_args_and_accounts() {
    let idl: serde_json::Value = serde_json::from_str(REAL_IDL).unwrap();
    let ixs = svmscope::idl::instructions(&idl);
    let cv = ixs.iter().find(|i| i.name == "create_vesting").unwrap();

    let arg_names: Vec<&str> = cv.args.iter().map(|a| a.name.as_str()).collect();
    assert_eq!(
        arg_names,
        vec!["schedule_id", "amount", "start_ts", "cliff_ts", "end_ts"]
    );
    // Types resolve to their labels.
    assert_eq!(cv.args[0].ty, "u64");
    assert_eq!(cv.args[2].ty, "i64");

    let acct_names: Vec<&str> = cv.accounts.iter().map(|a| a.name.as_str()).collect();
    assert!(acct_names.contains(&"creator"));
    assert!(acct_names.contains(&"schedule"));
    // The creator signs.
    let creator = cv.accounts.iter().find(|a| a.name == "creator").unwrap();
    assert!(creator.signer, "creator should be a signer");
}

#[test]
fn malformed_idls_never_panic() {
    let cases = [
        serde_json::json!({}),
        serde_json::json!(null),
        serde_json::json!([]),
        serde_json::json!("a string"),
        serde_json::json!(42),
        serde_json::json!({"instructions": "not-an-array"}),
        serde_json::json!({"instructions": 123}),
        serde_json::json!({"instructions": [{}]}), // no name/discriminator -> filtered
        serde_json::json!({"instructions": [{"name": "x"}]}), // no discriminator -> filtered
        serde_json::json!({"instructions": [{"discriminator": [1, 2, 3]}]}), // no name -> filtered
        serde_json::json!({"instructions": [{"name": "x", "discriminator": "bad"}]}),
        serde_json::json!({"instructions": [{"name": 5, "discriminator": [1]}]}),
        serde_json::json!({"instructions": [null, 1, "two"]}),
        serde_json::json!({"instructions": [{"name":"ok","discriminator":[1,2,3,4,5,6,7,8],
            "accounts": "not-array", "args": {"not":"array"}}]}),
    ];
    for c in cases {
        // Must not panic; returns a Vec (possibly empty).
        let out = svmscope::idl::instructions(&c);
        let _ = out.len();
    }

    // The instructions that are missing name or discriminator are dropped, so a
    // list of only-broken entries yields an empty vec.
    let broken = serde_json::json!({"instructions": [{}, {"name":"x"}, {"discriminator":[1]}]});
    assert!(svmscope::idl::instructions(&broken).is_empty());

    // A well-formed minimal instruction survives.
    let ok = serde_json::json!({"instructions": [
        {"name":"go","discriminator":[1,2,3,4,5,6,7,8],"args":[],"accounts":[]}
    ]});
    let out = svmscope::idl::instructions(&ok);
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].name, "go");
    assert_eq!(out[0].discriminator, vec![1, 2, 3, 4, 5, 6, 7, 8]);
}

// --------------------------------------------------------------------------
// Check / Cmp / Scenario builders — construct every variant, no panic
// --------------------------------------------------------------------------

#[test]
fn cmp_constructors_all_build() {
    let _ = [
        Cmp::eq(1),
        Cmp::ne(2),
        Cmp::lt(3),
        Cmp::le(4),
        Cmp::gt(5),
        Cmp::ge(6),
        Cmp::eq(-100),
        Cmp::ge(i64::MIN),
        Cmp::le(i64::MAX),
    ];
}

#[test]
fn every_check_variant_builds() {
    let _checks = vec![
        Check::success(),
        Check::revert(),
        Check::revert_contains("Custom(6001)"),
        Check::any_outcome(),
        Check::log_contains("hello"),
        Check::matches_onchain(),
        Check::compute_units(Cmp::le(200_000)),
        Check::account("PDA").lamports(Cmp::ge(1)).build(),
        Check::account("PDA").lamports_delta(Cmp::eq(0)).build(),
        Check::account("PDA").token_amount(Cmp::gt(0)).build(),
        Check::account("PDA").token_delta(Cmp::lt(0)).build(),
        Check::account("PDA").u64_at(8, Cmp::eq(42)).build(),
        Check::account("PDA").field("count", Cmp::eq(100)).build(),
        Check::account("PDA").field_delta("count", Cmp::eq(1)).build(),
        // Chained multi-assert account check.
        Check::account("PDA")
            .lamports(Cmp::ge(1))
            .field("count", Cmp::eq(2))
            .u64_at(0, Cmp::ne(0))
            .build(),
    ];
}

#[test]
fn scenario_builder_accumulates() {
    let s = Scenario::new("drain")
        .mutate(Mutation::lamports("V", 0))
        .mutate(Mutation::patch("V", 8, vec![0u8; 8]))
        .mutate(Mutation::data("V", vec![1, 2, 3]))
        .check(Check::revert())
        .check(Check::account("U").token_delta(Cmp::eq(0)).build());
    assert_eq!(s.name, "drain");
    assert_eq!(s.mutations.len(), 3);
    assert_eq!(s.checks.len(), 2);
}

#[test]
fn mutation_constructors_match_variants() {
    assert!(matches!(Mutation::lamports("A", 5), Mutation::Lamports { .. }));
    assert!(matches!(Mutation::data("A", vec![1]), Mutation::Data { .. }));
    assert!(matches!(Mutation::patch("A", 0, vec![1]), Mutation::DataPatch { .. }));
}

// --------------------------------------------------------------------------
// Error taxonomy — trait bounds, Display, non_exhaustive
// --------------------------------------------------------------------------

fn assert_std_error<E: std::error::Error>() {}

#[test]
fn error_implements_std_error_debug_display() {
    assert_std_error::<Error>();
    let e = resolve_rpc_url(Some("bogus-cluster"), None, DEF).unwrap_err();
    assert!(!e.to_string().is_empty()); // Display
    assert!(!format!("{e:?}").is_empty()); // Debug
    // Error is #[non_exhaustive], so an external match needs a wildcard arm;
    // here we just assert the expected variant.
    assert!(matches!(e, Error::InvalidSpec(_)));
}

#[test]
fn several_paths_produce_typed_invalid_spec_errors() {
    let paths: Vec<Error> = vec![
        resolve_rpc_url(Some("xyz"), None, DEF).unwrap_err(),
        scenario(r#"{"name":"s","expect":"nope"}"#).unwrap_err(),
        scenario_with_assert(r#"{"address":"X","kind":"weird","value":1}"#).unwrap_err(),
        serde_json::from_str::<FeatureInput>(r#"{"id":"bad"}"#)
            .unwrap()
            .into_toggle()
            .unwrap_err(),
    ];
    for e in paths {
        assert!(matches!(e, Error::InvalidSpec(_)));
        assert!(!e.to_string().is_empty());
    }
}

// --------------------------------------------------------------------------
// TRY TO BREAK IT — malformed JSON at every parser, nothing may panic
// --------------------------------------------------------------------------

#[test]
fn garbage_json_is_always_err_never_panic() {
    let garbage = [
        "",
        "{",
        "}",
        "[]",
        "null",
        "true",
        "123",
        "\"a string\"",
        "{\"kind\":}",
        "{ nested: { deeply: { wrong: [1,2,3 ",
        "\u{feff}{}", // BOM
    ];
    for g in garbage {
        // Every spec parser must return Err (or a well-formed value), never panic.
        let _ = serde_json::from_str::<MutationInput>(g);
        let _ = serde_json::from_str::<AssertInput>(g);
        let _ = serde_json::from_str::<ScenarioInput>(g);
        let _ = serde_json::from_str::<SuiteRequest>(g);
        let _ = serde_json::from_str::<FeatureInput>(g);
    }
}

#[test]
fn wrong_types_for_fields_are_errors() {
    // lamports as a string, offset as a bool, value as an object, etc.
    assert!(serde_json::from_str::<MutationInput>(
        r#"{"kind":"lamports","address":"A","lamports":"five"}"#
    )
    .is_err());
    assert!(serde_json::from_str::<MutationInput>(
        r#"{"kind":"data","address":"A","offset":true,"bytes_hex":"00"}"#
    )
    .is_err());
    assert!(serde_json::from_str::<AssertInput>(
        r#"{"address":"X","value":{"nested":1}}"#
    )
    .is_err());
    assert!(serde_json::from_str::<AssertInput>(r#"{"address":123,"value":1}"#).is_err());
    assert!(serde_json::from_str::<ScenarioInput>(r#"{"name":["not","a","string"]}"#).is_err());
}

#[test]
fn deeply_nested_and_unicode_payloads_do_not_panic() {
    let deep = format!("{}1{}", "[".repeat(200), "]".repeat(200));
    let _ = serde_json::from_str::<AssertInput>(&deep);

    let unicode = serde_json::json!({
        "name": "\u{1f600}\u{4e2d}\u{6587}",
        "expect": "any",
        "asserts": [{"address":"\u{00e9}\u{00e8}","kind":"field","field":"\u{20ac}","value":1}]
    })
    .to_string();
    // Valid structurally; unicode strings must flow through without panic.
    let s = serde_json::from_str::<ScenarioInput>(&unicode).unwrap();
    let _ = s.into_scenario(); // Ok or Err, but no panic.
}
