//! Wire types shared by the web server and the CLI test-runner.
//!
//! Mutations and scenarios arrive as JSON (from the browser, or a `.json` test
//! file); these `Deserialize` types convert them into the strongly-typed
//! `replay::{Mutation, ScenarioSpec}` the engine runs.

use serde::Deserialize;

use crate::replay::{AccountAssert, CmpOp, Expect, Mutation, ScenarioSpec, StateCheck, TimeTravel};

/// One what-if mutation. `kind` selects the variant:
/// `{"kind":"lamports","address":..,"lamports":..}` or
/// `{"kind":"data","address":..,"offset":..,"bytes_hex":".."}` (patch at offset).
#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum MutationInput {
    Lamports { address: String, lamports: u64 },
    Data { address: String, offset: usize, bytes_hex: String },
}

/// Decode a hex string, tolerating `0x`, spaces, and underscores.
pub fn hex_decode(s: &str) -> Result<Vec<u8>, String> {
    let s = s.trim().trim_start_matches("0x").replace([' ', '_'], "");
    if s.is_empty() || !s.len().is_multiple_of(2) {
        return Err("hex bytes must be a non-empty, even-length hex string".into());
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).map_err(|_| format!("invalid hex: {s}")))
        .collect()
}

impl MutationInput {
    pub fn into_mutation(self) -> Result<Mutation, String> {
        Ok(match self {
            MutationInput::Lamports { address, lamports } => {
                Mutation::Lamports { address, value: lamports }
            }
            MutationInput::Data { address, offset, bytes_hex } => {
                Mutation::DataPatch { address, offset, bytes: hex_decode(&bytes_hex)? }
            }
        })
    }
}

/// A post-replay state assertion. `kind` is one of:
/// - `"u64"` — little-endian u64 at `offset`.
/// - `"lamports"` — the account's lamports.
/// - `"token_amount"` — SPL token amount (u64 @ 64); shorthand for `u64` at offset 64.
/// - `"lamports_delta"` — change in lamports (post − pre); `value` may be negative.
/// - `"token_delta"` — change in SPL token amount (post − pre); `value` may be negative.
/// - `"field"` — the named `field` of the account's decoded layout (SPL layouts,
///   or the owner program's IDL), e.g. `"field":"pool.reserveA"`. Matched by
///   exact name or final dot-segment; integer/bool fields only, signed included.
/// - `"field_delta"` — change in the named `field` (post − pre); may be negative.
///
/// `op` is one of `== != < <= > >=` (default `==`).
#[derive(Deserialize)]
pub struct AssertInput {
    pub address: String,
    #[serde(default = "default_kind")]
    pub kind: String,
    #[serde(default)]
    pub offset: usize,
    /// Field name for the `field` / `field_delta` kinds.
    #[serde(default)]
    pub field: Option<String>,
    #[serde(default = "default_op")]
    pub op: String,
    /// Signed so deltas can be negative; non-delta kinds require it to be ≥ 0.
    pub value: i64,
}

fn default_kind() -> String {
    "u64".into()
}
fn default_op() -> String {
    "==".into()
}

impl AssertInput {
    fn into_assert(self) -> Result<AccountAssert, String> {
        let op = match self.op.as_str() {
            "==" | "eq" => CmpOp::Eq,
            "!=" | "ne" => CmpOp::Ne,
            "<" | "lt" => CmpOp::Lt,
            "<=" | "le" => CmpOp::Le,
            ">" | "gt" => CmpOp::Gt,
            ">=" | "ge" => CmpOp::Ge,
            other => return Err(format!("unknown assert op: {other}")),
        };
        // Non-delta kinds compare against an unsigned value.
        let unsigned = || -> Result<u64, String> {
            u64::try_from(self.value).map_err(|_| format!("{} value must be ≥ 0", self.kind))
        };
        // Field kinds compare signed (an i64 timestamp is a legitimate target).
        let field = || -> Result<String, String> {
            match &self.field {
                Some(f) if !f.trim().is_empty() => Ok(f.trim().to_string()),
                _ => Err(format!("assert kind \"{}\" needs a \"field\" name", self.kind)),
            }
        };
        let check = match self.kind.as_str() {
            "lamports" => StateCheck::Lamports { op, value: unsigned()? },
            "u64" => StateCheck::U64At { offset: self.offset, op, value: unsigned()? },
            "token_amount" => StateCheck::U64At { offset: 64, op, value: unsigned()? },
            "lamports_delta" => StateCheck::LamportsDelta { op, value: self.value as i128 },
            "token_delta" => StateCheck::TokenDelta { op, value: self.value as i128 },
            "field" => StateCheck::Field { name: field()?, op, value: self.value as i128 },
            "field_delta" => StateCheck::FieldDelta { name: field()?, op, value: self.value as i128 },
            other => return Err(format!("unknown assert kind: {other}")),
        };
        Ok(AccountAssert { address: self.address, check })
    }
}

/// One test scenario: a name, the mutations to apply, the transaction-level
/// outcome to assert, and optional post-replay state assertions.
///
/// `expect` is `"success"`, `"revert"` (or `"fail"`), or `"any"`. When reverting,
/// an optional `contains` requires the error/logs to include that text.
#[derive(Deserialize)]
pub struct ScenarioInput {
    pub name: String,
    #[serde(default = "default_expect")]
    pub expect: String,
    #[serde(default)]
    pub contains: Option<String>,
    #[serde(default)]
    pub mutations: Vec<MutationInput>,
    #[serde(default)]
    pub asserts: Vec<AssertInput>,
}

fn default_expect() -> String {
    "any".into()
}

impl ScenarioInput {
    pub fn into_spec(self) -> Result<ScenarioSpec, String> {
        let expect = match self.expect.as_str() {
            "success" | "pass" => Expect::Success,
            "revert" | "fail" => match self.contains {
                Some(s) if !s.is_empty() => Expect::RevertContains(s),
                _ => Expect::Revert,
            },
            _ => Expect::Any,
        };
        let mutations = self
            .mutations
            .into_iter()
            .map(MutationInput::into_mutation)
            .collect::<Result<_, _>>()?;
        let asserts = self
            .asserts
            .into_iter()
            .map(AssertInput::into_assert)
            .collect::<Result<_, _>>()?;
        Ok(ScenarioSpec { name: self.name, mutations, expect, asserts })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_decode_tolerates_prefix_spaces_underscores() {
        assert_eq!(hex_decode("0xDEAD_beef").unwrap(), vec![0xde, 0xad, 0xbe, 0xef]);
        assert_eq!(hex_decode("00 ff").unwrap(), vec![0x00, 0xff]);
    }

    #[test]
    fn hex_decode_rejects_bad_input() {
        assert!(hex_decode("").is_err());
        assert!(hex_decode("abc").is_err()); // odd length
        assert!(hex_decode("zz").is_err());
    }

    #[test]
    fn data_mutation_becomes_patch() {
        let m: MutationInput = serde_json::from_str(
            r#"{"kind":"data","address":"X","offset":64,"bytes_hex":"0000000000000000"}"#,
        )
        .unwrap();
        match m.into_mutation().unwrap() {
            Mutation::DataPatch { address, offset, bytes } => {
                assert_eq!(address, "X");
                assert_eq!(offset, 64);
                assert_eq!(bytes, vec![0u8; 8]);
            }
            _ => panic!("expected DataPatch"),
        }
    }

    #[test]
    fn assert_kinds_and_ops_resolve() {
        let a: AssertInput = serde_json::from_str(
            r#"{"address":"X","kind":"token_amount","op":">=","value":5}"#,
        )
        .unwrap();
        // token_amount is shorthand for u64 at the SPL amount offset.
        match a.into_assert().unwrap().check {
            StateCheck::U64At { offset: 64, op: CmpOp::Ge, value: 5 } => {}
            _ => panic!("expected U64At @64 >= 5"),
        }

        let d: AssertInput = serde_json::from_str(
            r#"{"address":"X","kind":"token_delta","op":"<","value":-3}"#,
        )
        .unwrap();
        match d.into_assert().unwrap().check {
            StateCheck::TokenDelta { op: CmpOp::Lt, value: -3 } => {}
            _ => panic!("expected TokenDelta < -3"),
        }
    }

    #[test]
    fn field_asserts_resolve_and_allow_negatives() {
        let a: AssertInput = serde_json::from_str(
            r#"{"address":"X","kind":"field","field":"pool.reserveA","op":">=","value":1000}"#,
        )
        .unwrap();
        match a.into_assert().unwrap().check {
            StateCheck::Field { name, op: CmpOp::Ge, value: 1000 } => assert_eq!(name, "pool.reserveA"),
            _ => panic!("expected Field >= 1000"),
        }

        // Deltas — and signed fields like i64 timestamps — may go negative.
        let d: AssertInput = serde_json::from_str(
            r#"{"address":"X","kind":"field_delta","field":"reserveA","value":-500}"#,
        )
        .unwrap();
        match d.into_assert().unwrap().check {
            StateCheck::FieldDelta { name, op: CmpOp::Eq, value: -500 } => assert_eq!(name, "reserveA"),
            _ => panic!("expected FieldDelta == -500"),
        }
    }

    #[test]
    fn field_assert_requires_a_field_name() {
        let a: AssertInput =
            serde_json::from_str(r#"{"address":"X","kind":"field","value":1}"#).unwrap();
        assert!(a.into_assert().unwrap_err().contains("field"));
    }

    #[test]
    fn non_delta_assert_rejects_negative_value() {
        let a: AssertInput =
            serde_json::from_str(r#"{"address":"X","kind":"lamports","value":-1}"#).unwrap();
        assert!(a.into_assert().is_err());
    }

    #[test]
    fn unknown_op_and_kind_error() {
        let a: AssertInput =
            serde_json::from_str(r#"{"address":"X","op":"~=","value":1}"#).unwrap();
        assert!(a.into_assert().is_err());
        let k: AssertInput =
            serde_json::from_str(r#"{"address":"X","kind":"balancez","value":1}"#).unwrap();
        assert!(k.into_assert().is_err());
    }

    #[test]
    fn scenario_expect_revert_with_contains() {
        let s: ScenarioInput = serde_json::from_str(
            r#"{"name":"drain","expect":"revert","contains":"Slippage","mutations":[],"asserts":[]}"#,
        )
        .unwrap();
        match s.into_spec().unwrap().expect {
            Expect::RevertContains(t) => assert_eq!(t, "Slippage"),
            _ => panic!("expected RevertContains"),
        }
    }
}

/// POST body for `/simulate_suite`, and the on-disk format for `svmscope test`.
///
/// Provide `fixture` (a path to a frozen fixture file) for a deterministic,
/// offline run — the CI-safe path — or `signature` to fetch live state via RPC.
#[derive(Deserialize)]
pub struct SuiteRequest {
    #[serde(default)]
    pub signature: Option<String>,
    #[serde(default)]
    pub fixture: Option<String>,
    /// Cluster name (mainnet/devnet/testnet/localnet) for the live-signature path.
    #[serde(default)]
    pub cluster: Option<String>,
    /// Explicit RPC URL override for the live-signature path.
    #[serde(default)]
    pub rpc: Option<String>,
    /// Optional clock warp applied to every scenario in the suite.
    #[serde(default)]
    pub time_travel: TimeTravel,
    pub scenarios: Vec<ScenarioInput>,
}
