//! Wire types shared by the web server and the CLI test-runner.
//!
//! Mutations and scenarios arrive as JSON (from the browser, or a `.json` test
//! file); these `Deserialize` types convert them into the strongly-typed
//! `replay::{Mutation, ScenarioSpec}` the engine runs.

use serde::Deserialize;

use crate::replay::{Expect, Mutation, ScenarioSpec};

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

/// One test scenario: a name, the mutations to apply, and the outcome to assert.
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
        Ok(ScenarioSpec { name: self.name, mutations, expect })
    }
}

/// POST body for `/simulate_suite`, and the on-disk format for `svmscope test`.
#[derive(Deserialize)]
pub struct SuiteRequest {
    pub signature: String,
    pub scenarios: Vec<ScenarioInput>,
}
