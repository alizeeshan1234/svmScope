//! Hermetic fixtures — the foundation of deterministic testing.
//!
//! A [`Fixture`] is a self-contained snapshot of everything needed to replay a
//! transaction: the transaction wire bytes and every account and program it
//! touched, already resolved. Once frozen to a file it replays identically
//! forever, with **no RPC and no state drift** — so a scenario suite built on a
//! fixture is a real, hermetic regression test you can commit and run in CI.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

use crate::scope::OnchainRecord;

/// The highest fixture schema version this build reads and writes.
pub const FIXTURE_VERSION: u32 = 2;

fn default_version() -> u32 {
    1 // a fixture written before the version field existed
}

/// One captured account: either a data account (loaded verbatim) or a program
/// (its resolved ELF bytecode, ready for LiteSVM's loader).
#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum FixtureEntry {
    /// A data account, captured verbatim.
    Data {
        /// The account's address, as base58.
        address: String,
        /// The program that owns the account.
        owner: String,
        /// The account's SOL balance in lamports.
        lamports: u64,
        /// base64 of the raw account data.
        data_b64: String,
    },
    /// A program, captured as its executable bytecode.
    Program {
        /// The program's address, as base58.
        address: String,
        /// base64 of the program's ELF bytecode.
        elf_b64: String,
    },
}

/// A portable, self-contained snapshot of a transaction and its world.
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Fixture {
    /// Schema version. v1 files (no field) still load; this build writes v2,
    /// which adds `idls` and `recorded`.
    #[serde(default = "default_version")]
    pub version: u32,
    /// The frozen transaction's signature, as base58.
    pub signature: String,
    /// The slot the live context actually ran at — the SVM clock is set to this on
    /// reload, so a frozen replay uses the *same* slot the live one did (not the
    /// transaction's original slot, which would contradict the captured accounts).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub captured_slot: Option<u64>,
    /// The wall-clock time (unix seconds) that same slot ran at. Captured so a
    /// frozen replay's clock matches the live context that produced it — without
    /// it, any date-based program logic behaves differently after freezing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub captured_block_time: Option<i64>,
    /// base64 of the bincode-serialized `VersionedTransaction`.
    pub tx_b64: String,
    /// Every captured account and program the transaction touches.
    pub entries: Vec<FixtureEntry>,
    /// On-chain Anchor IDLs by program id, captured so named-field asserts,
    /// error names, and decoded diffs all resolve **offline**. (v2+)
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub idls: BTreeMap<String, serde_json::Value>,
    /// The transaction's actual on-chain outcome, for comparing frozen replays
    /// against reality (`Check::matches_onchain`). (v2+)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recorded: Option<OnchainRecord>,
}

impl Fixture {
    /// Serialize to pretty JSON — the on-disk fixture format.
    pub fn to_json(&self) -> crate::error::Result<String> {
        serde_json::to_string_pretty(self)
            .map_err(|e| crate::error::Error::Fixture(format!("serialize: {e}")))
    }

    /// Parse a fixture from its JSON form. Reads any version up to
    /// [`FIXTURE_VERSION`]; a newer file is an error, not a misread.
    pub fn from_json(s: &str) -> crate::error::Result<Fixture> {
        let fx: Fixture = serde_json::from_str(s)
            .map_err(|e| crate::error::Error::Fixture(format!("parse: {e}")))?;
        if fx.version > FIXTURE_VERSION {
            return Err(crate::error::Error::Fixture(format!(
                "unsupported fixture version {} (this build reads <= {FIXTURE_VERSION}) — \
                 update svmscope, or re-capture the fixture",
                fx.version
            )));
        }
        Ok(fx)
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
