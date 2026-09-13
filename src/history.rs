//! Past account bytes from a public account-changes stream.
//!
//! Nothing on chain keeps an account's old bytes, and the recorder only
//! knows what it has watched since it started. For the months before that,
//! StreamingFast's Solana account-changes stream (Substreams, The Graph
//! Market) carries every changed account's bytes per slot on a rolling
//! window of about three months, with a free tier and no card. This module
//! asks it, on demand, for the last change of each wanted account before a
//! slot, by running the `substreams` client as a subprocess over a bounded
//! slot range. What it returns is exact at the target slot: the stream
//! records every change, so "no newer change before the slot" means the
//! bytes are the bytes at that slot.
//!
//! Every version fetched is handed back to the caller to write into its own
//! record store, so the stream is asked once per account and slot, never
//! again.

use {
    crate::error::{Error, Result},
    crate::records::Version,
    crate::AccountState,
    std::{collections::HashMap, path::PathBuf, process::Command},
};

/// The account-changes stream, reachable through the `substreams` client.
#[derive(Debug, Clone)]
pub struct HistoryStream {
    /// Path to the `substreams` binary.
    pub bin: PathBuf,
    /// The account-changes endpoint.
    pub endpoint: String,
    /// The package and module that filter by account.
    pub package: String,
    /// Slots streamed before the target on the first try: enough for any
    /// account that changes every few minutes.
    pub lookback: u64,
    /// Slots streamed on the second try, for accounts the first missed.
    /// In production mode the server sends nothing for blocks without a
    /// matching account and skips executing them, so a long range over a
    /// quiet account costs seconds and near-zero egress (about 100,000
    /// slots, eleven hours, in under a minute).
    pub deep_lookback: u64,
}

/// The endpoint the client speaks to; the key travels in `SUBSTREAMS_API_KEY`.
pub const ENDPOINT: &str = "accounts.mainnet.sol.streamingfast.io:443";
/// The published package that filters account changes by address or owner.
pub const PACKAGE: &str = "solana-accounts-foundational";
/// Accounts per client call: the filter is one expression, kept short.
const BATCH: usize = 60;
/// The free tier allows two concurrent streams per key; one process keeps
/// to one at a time so parallel replays queue instead of failing.
static STREAM_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
/// After the provider answers "concurrent stream limit exceeded", stay off
/// it for this long: the client's own retries against that answer are what
/// keep the slots occupied, so backing off is the only way they free up.
static LIMIT_HIT_UNTIL: std::sync::Mutex<Option<std::time::Instant>> = std::sync::Mutex::new(None);
const LIMIT_BACKOFF: std::time::Duration = std::time::Duration::from_secs(300);

impl HistoryStream {
    /// From the environment: on when `SUBSTREAMS_API_KEY` is set and the
    /// client is found (`SVMSCOPE_SUBSTREAMS_BIN`, default `substreams` on
    /// `PATH`). `SVMSCOPE_HISTORY_LOOKBACK` and
    /// `SVMSCOPE_HISTORY_DEEP_LOOKBACK` tune the ranges (slots).
    pub fn from_env() -> Option<HistoryStream> {
        std::env::var("SUBSTREAMS_API_KEY")
            .ok()
            .filter(|k| !k.trim().is_empty())?;
        let bin = PathBuf::from(
            std::env::var("SVMSCOPE_SUBSTREAMS_BIN").unwrap_or_else(|_| "substreams".to_string()),
        );
        let found = bin.is_file()
            || std::env::var_os("PATH")
                .is_some_and(|p| std::env::split_paths(&p).any(|d| d.join(&bin).is_file()));
        if !found {
            eprintln!("history stream: `{}` not found; disabled", bin.display());
            return None;
        }
        let num = |k: &str, d: u64| {
            std::env::var(k)
                .ok()
                .and_then(|v| v.trim().parse().ok())
                .unwrap_or(d)
        };
        // A local `.spkg` (`SVMSCOPE_SUBSTREAMS_PACKAGE`) skips the registry
        // lookup the client otherwise makes on every run.
        let package = std::env::var("SVMSCOPE_SUBSTREAMS_PACKAGE")
            .ok()
            .filter(|p| !p.trim().is_empty())
            .unwrap_or_else(|| PACKAGE.to_string());
        Some(HistoryStream {
            bin,
            endpoint: ENDPOINT.to_string(),
            package,
            lookback: num("SVMSCOPE_HISTORY_LOOKBACK", 2_000),
            deep_lookback: num("SVMSCOPE_HISTORY_DEEP_LOOKBACK", 100_000),
        })
    }

    /// The last change of each of `accounts` strictly before `slot`, looking
    /// back `lookback` slots. Accounts with no change in the range are absent
    /// from the result: they may be older than the range, or never written.
    pub fn latest_before(
        &self,
        accounts: &[String],
        slot: u64,
        lookback: u64,
    ) -> Result<HashMap<String, Version>> {
        let mut out = HashMap::new();
        if accounts.is_empty() || slot == 0 {
            return Ok(out);
        }
        let start = slot.saturating_sub(lookback).max(1);
        for chunk in accounts.chunks(BATCH) {
            let filter = chunk
                .iter()
                .map(|a| format!("account:{a}"))
                .collect::<Vec<_>>()
                .join(" || ");
            let run = || {
                Command::new(&self.bin)
                    .args([
                        "run",
                        "-e",
                        &self.endpoint,
                        &self.package,
                        "filtered_accounts",
                        "-s",
                        &start.to_string(),
                        "-t",
                        &slot.to_string(),
                        "-o",
                        "jsonl",
                        "--final-blocks-only",
                        "--production-mode",
                        "--limit-processed-blocks",
                        "0",
                        "-p",
                        &format!("filtered_accounts={filter}"),
                    ])
                    .output()
                    .map_err(|e| Error::Fixture(format!("history stream: run substreams: {e}")))
            };
            // One retry: the client's connection to the registry or the
            // stream occasionally resets, and a second attempt succeeds.
            let _serial = STREAM_LOCK.lock().unwrap_or_else(|e| e.into_inner());
            {
                let until = LIMIT_HIT_UNTIL.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(t) = *until {
                    if std::time::Instant::now() < t {
                        return Err(Error::Fixture(
                            "history stream: backing off after the provider's concurrency limit"
                                .to_string(),
                        ));
                    }
                }
            }
            let mut output = run()?;
            let limit_hit = |o: &std::process::Output| {
                String::from_utf8_lossy(&o.stderr).contains("Concurrent stream limit exceeded")
            };
            if !output.status.success() && !limit_hit(&output) {
                std::thread::sleep(std::time::Duration::from_secs(2));
                output = run()?;
            }
            if !output.status.success() {
                if limit_hit(&output) {
                    *LIMIT_HIT_UNTIL.lock().unwrap_or_else(|e| e.into_inner()) =
                        Some(std::time::Instant::now() + LIMIT_BACKOFF);
                }
                let err = String::from_utf8_lossy(&output.stderr);
                let last = err
                    .lines()
                    .rev()
                    .find(|l| !l.trim().is_empty())
                    .unwrap_or("");
                return Err(Error::Fixture(format!(
                    "history stream: substreams exited {}: {last}",
                    output.status
                )));
            }
            out.extend(parse_jsonl(&String::from_utf8_lossy(&output.stdout), slot));
        }
        Ok(out)
    }
}

/// The last version per account from the client's `jsonl` output, for
/// blocks strictly before `slot`. Account bytes come base58-encoded.
pub(crate) fn parse_jsonl(text: &str, slot: u64) -> HashMap<String, Version> {
    let mut out: HashMap<String, Version> = HashMap::new();
    for line in text.lines() {
        let Ok(rec) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let Some(block) = rec["@block"].as_u64() else {
            continue;
        };
        if block >= slot {
            continue;
        }
        let Some(accounts) = rec["@data"]["accounts"].as_array() else {
            continue;
        };
        for a in accounts {
            let Some(address) = a["address"].as_str() else {
                continue;
            };
            let deleted = a["deleted"].as_bool().unwrap_or(false);
            let state = if deleted {
                None
            } else {
                let data = a["data"]
                    .as_str()
                    .and_then(|s| bs58::decode(s).into_vec().ok())
                    .unwrap_or_default();
                Some(AccountState {
                    data,
                    // The stream carries no lamports; the caller fills them in
                    // from the account's current balance (see `apply`).
                    lamports: 0,
                    owner: a["owner"].as_str().unwrap_or_default().to_string(),
                })
            };
            let newer = out.get(address).is_none_or(|v| block >= v.slot);
            if newer {
                out.insert(address.to_string(), Version { slot: block, state });
            }
        }
    }
    out
}

/// Lamports for a version the stream returned without them: the account's
/// current balance when it still exists (data accounts almost never change
/// theirs), else the rent-exempt minimum for its size, which is what a live
/// account of that size must have held.
pub(crate) fn lamports_for(current: Option<u64>, data_len: usize) -> u64 {
    current.unwrap_or(((data_len + 128) as u64) * 6_960)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_the_last_change_before_the_slot_per_account() {
        let data = bs58::encode([1u8, 2, 3]).into_string();
        let later = bs58::encode([9u8]).into_string();
        let text = format!(
            "{{\"@block\":100,\"@data\":{{\"accounts\":[{{\"address\":\"A\",\"owner\":\"O\",\"data\":\"{data}\"}}]}}}}\n\
             {{\"@block\":150,\"@data\":{{\"accounts\":[{{\"address\":\"A\",\"owner\":\"O\",\"data\":\"{later}\"}},{{\"address\":\"B\",\"owner\":\"O\",\"deleted\":true}}]}}}}\n\
             {{\"@block\":200,\"@data\":{{\"accounts\":[{{\"address\":\"A\",\"owner\":\"O\",\"data\":\"{data}\"}}]}}}}\n\
             not json\n"
        );
        let out = parse_jsonl(&text, 200);
        let a = &out["A"];
        assert_eq!(a.slot, 150);
        assert_eq!(a.state.as_ref().unwrap().data, vec![9]);
        assert_eq!(a.state.as_ref().unwrap().owner, "O");
        assert!(out["B"].state.is_none());
        assert_eq!(out["B"].slot, 150);
    }

    #[test]
    fn lamports_fall_back_to_the_rent_minimum() {
        assert_eq!(lamports_for(Some(5), 100), 5);
        assert_eq!(lamports_for(None, 100), 228 * 6_960);
    }
}
