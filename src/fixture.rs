//! Hermetic fixtures — the foundation of deterministic testing.
//!
//! A [`Fixture`] is a self-contained snapshot of everything needed to replay a
//! transaction: the transaction wire bytes and every account and program it
//! touched, already resolved. Once frozen to a file it replays identically
//! forever, with **no RPC and no state drift** — so a scenario suite built on a
//! fixture is a real, hermetic regression test you can commit and run in CI.

use serde::{Deserialize, Serialize};

/// One captured account: either a data account (loaded verbatim) or a program
/// (its resolved ELF bytecode, ready for LiteSVM's loader).
#[derive(Serialize, Deserialize, Clone)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum FixtureEntry {
    Data {
        address: String,
        owner: String,
        lamports: u64,
        /// base64 of the raw account data.
        data_b64: String,
    },
    Program {
        address: String,
        /// base64 of the program's ELF bytecode.
        elf_b64: String,
    },
}

/// A portable, self-contained snapshot of a transaction and its world.
#[derive(Serialize, Deserialize, Clone)]
pub struct Fixture {
    pub signature: String,
    /// The slot the state was captured at, when known (informational).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub captured_slot: Option<u64>,
    /// base64 of the bincode-serialized `VersionedTransaction`.
    pub tx_b64: String,
    pub entries: Vec<FixtureEntry>,
}

impl Fixture {
    pub fn to_json(&self) -> Result<String, String> {
        serde_json::to_string_pretty(self).map_err(|e| format!("serialize fixture: {e}"))
    }

    pub fn from_json(s: &str) -> Result<Fixture, String> {
        serde_json::from_str(s).map_err(|e| format!("parse fixture: {e}"))
    }

    /// A one-line human summary, e.g. for CLI output.
    pub fn summary(&self) -> String {
        let programs = self
            .entries
            .iter()
            .filter(|e| matches!(e, FixtureEntry::Program { .. }))
            .count();
        let data = self.entries.len() - programs;
        format!("{data} accounts, {programs} programs")
    }
}
