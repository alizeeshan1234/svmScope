#![deny(unreachable_pub)]
#![warn(missing_docs)]

//! Decode, reconstruct, replay, and mutate Solana transactions.
//!
//! Point svmscope at a mainnet signature and it (1) **decodes** the transaction —
//! the full cross-program invocation tree with every instruction named from its
//! on-chain Anchor IDL or known native layout, balance and token changes, and
//! compute units per program; (2) **replays** it locally in an embedded SVM
//! ([LiteSVM](https://docs.rs/litesvm)), re-executing the real program binaries
//! against the transaction's reconstructed state; and (3) **mutates** — change an
//! account's lamports or data, warp the clock, flip feature gates, then replay
//! again to ask *"what if this had been different?"*.
//!
//! # Installation
//!
//! svmscope is a testing tool, so add it as a dev-dependency:
//!
//! ```text
//! cargo add --dev svmscope
//! ```
//!
//! or in `Cargo.toml`:
//!
//! ```toml
//! [dev-dependencies]
//! svmscope = "0.3"
//! ```
//!
//! # Quickstart
//!
//! Two nouns: a [`Scope`] fetches and caches, a [`Replay`] is one transaction's
//! reconstructed world. All RPC happens in [`Scope::replay`]; every run after
//! that is local and free.
//!
//! ```no_run
//! use svmscope::{Mutation, Scope};
//!
//! let scope = Scope::new("https://api.mainnet-beta.solana.com");
//! let sig = "your transaction signature";
//!
//! // 1. Decode: CPI tree, named instructions, balance/token diffs, CU per program.
//! let analysis = scope.analyze(sig)?;
//! println!("fee: {} lamports, {} top-level instructions",
//!          analysis.overview.fee, analysis.cpi_tree.len());
//!
//! // 2. Replay the real programs locally in LiteSVM.
//! let mut replay = scope.replay(sig)?;
//! println!("replay success: {}", replay.run()?.result.success);
//!
//! // 3. What-if: zero out an account, jump 30 days ahead, and replay again —
//! //    zero further RPC.
//! replay.advance_seconds(30 * 86_400);
//! let what_if = replay.simulate(&[Mutation::lamports("SomeAccount111...", 0)])?;
//! println!("mutated replay success: {}", what_if.result.success);
//! # Ok::<(), svmscope::Error>(())
//! ```
//!
//! # Building and submitting transactions
//!
//! You don't have to start from an existing signature. Against a local
//! validator, [`Scope::program_with_idl`] gives an IDL-driven builder: pick a
//! method, supply accounts and JSON arguments, and
//! [`MethodBuilder::send_and_capture`] signs, submits, waits for the
//! transaction to land, and returns a [`CapturedTransaction`] whose replay
//! holds the exact pre-transaction world — ready for mutation and time travel
//! with no further RPC.
//!
//! ```no_run
//! # use serde_json::json;
//! # use std::str::FromStr;
//! # let scope = svmscope::Scope::new("http://127.0.0.1:8899");
//! # let program_id = solana_address::Address::from_str("11111111111111111111111111111111")?;
//! # let idl = json!({});
//! # let payer = solana_keypair::Keypair::new();
//! # let state = program_id;
//! let captured = scope
//!     .program_with_idl(program_id, idl)
//!     .method("setValue")?
//!     .payer(&payer)
//!     .account("state", state)
//!     .args(json!({ "value": 42 }))?
//!     .send_and_capture()?;
//! println!("landed: {}", captured.signature);
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```
//!
//! # Hermetic testing
//!
//! Replays against live RPC state drift as the chain moves on. To pin a
//! transaction's world forever, [`Scope::capture`] snapshots the transaction,
//! every account it touched, and every program ELF into a [`Fixture`];
//! [`Replay::from_fixture`] then rebuilds the world and runs what-if scenario
//! suites (mutations + expectations + named-field assertions) against that
//! frozen state — offline, deterministic, CI-friendly.
//!
//! # Public API
//!
//! Consumer-facing types are re-exported from the crate root. Prefer imports
//! such as `svmscope::{Scope, Replay, Mutation, Check, Fixture}`; implementation
//! modules are private. The [`idl`], [`report`], and [`spec`] modules expose the
//! specialized IDL, HTML-report, and JSON-suite APIs.
//!
//! # Errors
//!
//! A **reverting replay is not an error** — it comes back as a successful
//! observation with `result.success == false`. [`Error`] always means svmscope
//! itself couldn't do what was asked (RPC failure, unknown transaction, a
//! mutation targeting an account that isn't loaded, an unknown field name…).
//!
//! # Feature flags
//!
//! None — the crate is a plain library. The HTTP API server lives in a
//! separate (unpublished) workspace crate, so library consumers never compile
//! axum/tokio.
//!
//! This same library powers the svmscope CLI, the HTTP API, and the hosted UI
//! at <https://svmscope.vercel.app> — identical results in all four.

mod analyze;
mod check;
mod compute;
mod cpi_tree;
mod decode;
mod diagnose;
mod diffs;
mod error;
mod fixture;
pub mod idl;
mod idl_encode;
mod idl_model;
mod invariant;
pub(crate) mod ixname;
mod preflight;
mod program;
pub mod reconstruct;
mod replay;
pub mod report;
pub mod scan;
mod scope;
mod search;
pub mod spec;
mod submit;
mod trace;
pub(crate) mod utils;
#[cfg(test)]
mod wire_format_tests;

pub use analyze::{
    AccountDiff, AccountOverview, Analysis, Explanation, FieldDiff, Overview, ProgramInfo, SigInfo,
    SimulationReport,
};
pub use check::{AccountCheck, Check, Cmp, Scenario};
pub use compute::CuUsage;
pub use cpi_tree::{CpiEntry, IxAccount, IxArg};
pub use decode::{AccountInfo, DecodedAccount, Field};
pub use diagnose::Diagnosis;
pub use diffs::{BalanceChange, TokenChange};
pub use error::{Error, Result};
pub use fixture::{Fixture, FixtureEntry, FIXTURE_VERSION};
pub use invariant::Invariant;
pub use preflight::{compute_breakdown, AccountRole, PreflightIx, PreflightOverview};
pub use program::{MethodBuilder, ProgramClient};
pub use replay::{
    AssertOutcome, FeatureToggle, Mutation, ReplayResult, ScenarioOutcome, TimeTravel,
};
pub use scan::{scan_breaking_points, BreakingPoint, ScanOptions};
pub use scope::{
    AccountProvenance, AccountState, Fidelity, FidelityCertificate, OnchainRecord, PatchComparison,
    Provenance, Replay, Replayed, Scope,
};
pub use search::Threshold;
pub use submit::CapturedTransaction;
pub use trace::{Step, StepDiff, StepError, StepSummary, Trace, TraceDiff};

/// Compile-checks every Rust example in the README as part of `cargo test`.
#[cfg(doctest)]
#[doc = include_str!("../README.md")]
pub struct ReadmeDoctests;

/// Resolve a cluster name or explicit RPC URL to an endpoint. Precedence:
/// explicit `rpc` URL > `cluster` name > `default`.
///
/// Clusters: `mainnet`, `devnet`, `testnet`, `localnet` (127.0.0.1:8899). A value
/// starting with `http` in either field is used verbatim; an unknown cluster
/// name is an error.
pub fn resolve_rpc_url(cluster: Option<&str>, rpc: Option<&str>, default: &str) -> Result<String> {
    if let Some(u) = rpc {
        if u.starts_with("http") {
            return Ok(u.to_string());
        }
    }
    match cluster.map(|c| c.trim().to_ascii_lowercase()).as_deref() {
        None | Some("") => Ok(default.to_string()),
        Some("mainnet") | Some("mainnet-beta") | Some("m") => {
            Ok("https://api.mainnet-beta.solana.com".into())
        }
        Some("devnet") | Some("d") => Ok("https://api.devnet.solana.com".into()),
        Some("testnet") | Some("t") => Ok("https://api.testnet.solana.com".into()),
        Some("localnet") | Some("local") | Some("localhost") | Some("l") => {
            Ok("http://127.0.0.1:8899".into())
        }
        Some(other) if other.starts_with("http") => Ok(other.to_string()),
        Some(other) => Err(Error::InvalidSpec(format!(
            "unknown cluster '{other}' (expected mainnet, devnet, testnet, or localnet)"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_rpc_url_precedence() {
        // Explicit URL beats everything.
        assert_eq!(
            resolve_rpc_url(Some("devnet"), Some("http://my"), "http://def").unwrap(),
            "http://my"
        );
        // Cluster names map to public endpoints.
        assert_eq!(
            resolve_rpc_url(Some("devnet"), None, "http://def").unwrap(),
            "https://api.devnet.solana.com"
        );
        assert_eq!(
            resolve_rpc_url(Some("m"), None, "http://def").unwrap(),
            "https://api.mainnet-beta.solana.com"
        );
        assert_eq!(
            resolve_rpc_url(Some("localnet"), None, "http://def").unwrap(),
            "http://127.0.0.1:8899"
        );
        // Nothing specified → the default; an unknown cluster is an error.
        assert_eq!(
            resolve_rpc_url(None, None, "http://def").unwrap(),
            "http://def"
        );
        assert!(matches!(
            resolve_rpc_url(Some("nope"), None, "http://def"),
            Err(Error::InvalidSpec(_))
        ));
        // A non-http "rpc" value is ignored, not used verbatim.
        assert_eq!(
            resolve_rpc_url(None, Some("garbage"), "http://def").unwrap(),
            "http://def"
        );
    }
}
