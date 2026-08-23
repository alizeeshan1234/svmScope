//! Wire types shared by the web server and the CLI test-runner.
//!
//! Mutations and scenarios arrive as JSON (from the browser, or a `.json` test
//! file); these `Deserialize` types convert them into the strongly-typed
//! `replay::{Mutation, ScenarioSpec}` the engine runs.

use serde::Deserialize;

use crate::replay::{AccountAssert, CmpOp, Expect, Mutation, ScenarioSpec, StateCheck};

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
    if s.is_empty() || s.len() % 2 != 0 {
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

/// A post-replay state assertion. `kind` is `"lamports"` or `"u64"` (a
/// little-endian u64 at `offset`, e.g. an SPL token amount at offset 64).
/// `op` is one of `== != < <= > >=` (default `==`).
#[derive(Deserialize)]
pub struct AssertInput {
    pub address: String,
    #[serde(default = "default_kind")]
    pub kind: String,
    #[serde(default)]
    pub offset: usize,
    #[serde(default = "default_op")]
    pub op: String,
    pub value: u64,
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
        let check = match self.kind.as_str() {
            "lamports" => StateCheck::Lamports { op, value: self.value },
            "u64" => StateCheck::U64At { offset: self.offset, op, value: self.value },
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
    pub scenarios: Vec<ScenarioInput>,
}
