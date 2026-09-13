//! Past account bytes from a public account-changes stream.
//!
//! Nothing on chain keeps an account's old bytes, and the recorder only
//! knows what it has watched since it started. For the months before that,
//! StreamingFast's Solana account-changes stream (Substreams, The Graph
//! Market) carries every changed account's bytes per slot on a rolling
//! window of about three months, with a free tier and no card. This module
//! asks it, on demand, for the last change of each wanted account before a
//! slot, over a native gRPC client (see [`crate::substreams`]) and a
//! bounded slot range. What it returns is exact at the target slot: the
//! stream records every change, so "no newer change before the slot" means
//! the bytes are the bytes at that slot.
//!
//! Every version fetched is handed back to the caller to write into its own
//! record store, so the stream is asked once per account and slot, never
//! again.

use {
    crate::error::{Error, Result},
    crate::records::Version,
    crate::substreams::{self, AccountsAt, Modules},
    crate::AccountState,
    std::{collections::HashMap, sync::Arc, time::Duration},
};

/// The account-changes stream.
#[derive(Debug, Clone)]
pub struct HistoryStream {
    /// The account-changes endpoint, `https://host:port`.
    pub endpoint: String,
    /// The package's module graph, sent with every call.
    pub modules: Arc<Modules>,
    /// Slots streamed before the target on the first try: enough for any
    /// small account that changes every few minutes.
    pub lookback: u64,
    /// The furthest the search looks back, in slots. In production mode the
    /// server sends nothing for blocks without a matching account and skips
    /// executing them, so a long range over a quiet account costs seconds
    /// and near-zero egress.
    pub deep_lookback: u64,
    /// API keys, tried in order. The provider's concurrency limit is per
    /// account, so keys from different accounts are independent slots; a
    /// key that answers "limit exceeded" is skipped for a while and the
    /// next one used.
    pub keys: Vec<String>,
}

/// The endpoint the client speaks to; the key travels in an `x-api-key` header.
pub const ENDPOINT: &str = "https://accounts.mainnet.sol.streamingfast.io:443";
/// The published package that filters account changes by address or owner,
/// bundled so no registry lookup ever happens.
pub const PACKAGE: &[u8] = include_bytes!("../assets/solana-accounts-foundational-v0.1.1.spkg");
/// The package's map module.
const MODULE: &str = "filtered_accounts";
/// Accounts per call: the filter is one expression, kept short.
const BATCH: usize = 60;
/// Accounts at least this large are searched in short windows: the stream
/// sends every change in the range, and a busy order book changes every
/// slot, so a wide window over it would cost its size times the width.
pub const LARGE_ACCOUNT_BYTES: usize = 64 * 1024;
/// First window for large accounts, in slots.
pub const LARGE_FIRST_WINDOW: u64 = 16;
/// One call may take this long before it is abandoned.
const CALL_DEADLINE: Duration = Duration::from_secs(300);
/// The free tier allows two concurrent streams per account, so one process
/// keeps at most that many in flight (`SVMSCOPE_STREAM_PARALLEL`, default
/// 2) and further replays queue instead of failing.
static STREAM_SLOTS: std::sync::Mutex<usize> = std::sync::Mutex::new(0);
static STREAM_FREED: std::sync::Condvar = std::sync::Condvar::new();

fn stream_parallel() -> usize {
    std::env::var("SVMSCOPE_STREAM_PARALLEL")
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .filter(|n| *n >= 1)
        .unwrap_or(2)
}

/// A held stream slot; released on drop.
struct StreamSlot;

impl StreamSlot {
    fn acquire() -> StreamSlot {
        let limit = stream_parallel();
        let mut held = STREAM_SLOTS.lock().unwrap_or_else(|e| e.into_inner());
        while *held >= limit {
            held = STREAM_FREED.wait(held).unwrap_or_else(|e| e.into_inner());
        }
        *held += 1;
        StreamSlot
    }
}

impl Drop for StreamSlot {
    fn drop(&mut self) {
        let mut held = STREAM_SLOTS.lock().unwrap_or_else(|e| e.into_inner());
        *held = held.saturating_sub(1);
        STREAM_FREED.notify_one();
    }
}
/// After a key answers "concurrent stream limit exceeded", stay off it for
/// this long so whatever holds its slots can drain.
static LIMIT_HIT_UNTIL: std::sync::Mutex<Option<HashMap<String, std::time::Instant>>> =
    std::sync::Mutex::new(None);
const LIMIT_BACKOFF: Duration = Duration::from_secs(300);
/// Waits between retries of a call the provider answered with its limit,
/// before the key is backed off: a finished stream stays counted for a
/// few seconds, so back-to-back calls collide with our own last one.
const LIMIT_RETRY_WAITS: &[Duration] = &[
    Duration::from_secs(5),
    Duration::from_secs(10),
    Duration::from_secs(20),
];

fn key_backed_off(key: &str) -> bool {
    let guard = LIMIT_HIT_UNTIL.lock().unwrap_or_else(|e| e.into_inner());
    guard
        .as_ref()
        .and_then(|m| m.get(key))
        .is_some_and(|t| std::time::Instant::now() < *t)
}

fn back_off_key(key: &str) {
    let mut guard = LIMIT_HIT_UNTIL.lock().unwrap_or_else(|e| e.into_inner());
    guard
        .get_or_insert_with(HashMap::new)
        .insert(key.to_string(), std::time::Instant::now() + LIMIT_BACKOFF);
}

impl HistoryStream {
    /// From the environment: on when `SUBSTREAMS_API_KEY` is set (one or
    /// more keys, comma-separated). `SVMSCOPE_SUBSTREAMS_PACKAGE` names a
    /// local `.spkg` to use instead of the bundled one;
    /// `SVMSCOPE_SUBSTREAMS_ENDPOINT` another endpoint.
    /// `SVMSCOPE_HISTORY_LOOKBACK` and `SVMSCOPE_HISTORY_DEEP_LOOKBACK`
    /// tune the ranges (slots).
    pub fn from_env() -> Option<HistoryStream> {
        let keys: Vec<String> = std::env::var("SUBSTREAMS_API_KEY")
            .ok()?
            .split(',')
            .map(str::trim)
            .filter(|k| !k.is_empty())
            .map(String::from)
            .collect();
        if keys.is_empty() {
            return None;
        }
        let num = |k: &str, d: u64| {
            std::env::var(k)
                .ok()
                .and_then(|v| v.trim().parse().ok())
                .unwrap_or(d)
        };
        let package = match std::env::var("SVMSCOPE_SUBSTREAMS_PACKAGE") {
            Ok(path) if !path.trim().is_empty() => match std::fs::read(path.trim()) {
                Ok(bytes) => bytes,
                Err(e) => {
                    eprintln!("history stream: read {path}: {e}; using the bundled package");
                    PACKAGE.to_vec()
                }
            },
            _ => PACKAGE.to_vec(),
        };
        let modules = match substreams::package_modules(&package) {
            Ok(m) => m,
            Err(e) => {
                eprintln!("history stream: {e}; disabled");
                return None;
            }
        };
        let endpoint = std::env::var("SVMSCOPE_SUBSTREAMS_ENDPOINT")
            .ok()
            .filter(|e| !e.trim().is_empty())
            .unwrap_or_else(|| ENDPOINT.to_string());
        Some(HistoryStream {
            endpoint,
            modules: Arc::new(modules),
            lookback: num("SVMSCOPE_HISTORY_LOOKBACK", 5_000),
            deep_lookback: num("SVMSCOPE_HISTORY_DEEP_LOOKBACK", 100_000),
            keys,
        })
    }

    /// The last change of each of `accounts` strictly before `slot`, searching
    /// backwards in widening windows: the first `first` slots, then the next
    /// `4 * first`, and so on until every account is found or `max` slots
    /// have been covered. Each window asks only for the accounts still
    /// missing, so a hot account costs one small window and a quiet one a
    /// few cheap empty ones; nothing ever streams a busy account across a
    /// long range.
    pub fn latest_before_windows(
        &self,
        accounts: &[String],
        slot: u64,
        first: u64,
        max: u64,
    ) -> Result<HashMap<String, Version>> {
        let mut found: HashMap<String, Version> = HashMap::new();
        let mut missing: Vec<String> = accounts.to_vec();
        let mut end = slot;
        let mut width = first.max(1);
        let floor = slot.saturating_sub(max).max(1);
        while !missing.is_empty() && end > floor {
            let start = end.saturating_sub(width).max(floor);
            // A window that fails after earlier ones succeeded does not
            // lose what they found: those versions are exact regardless.
            let got = match self.latest_before_in(&missing, start, end) {
                Ok(got) => got,
                Err(e) if found.is_empty() => return Err(e),
                Err(e) => {
                    eprintln!("history stream: {e}; keeping {} found so far", found.len());
                    break;
                }
            };
            missing.retain(|a| !got.contains_key(a));
            found.extend(got);
            end = start;
            // Each call costs seconds of fixed overhead, so widen fast:
            // 5k, 20k, 80k covers a day in three calls.
            width = width.saturating_mul(4);
        }
        Ok(found)
    }

    /// [`Self::latest_before_windows`] for accounts of known size: small
    /// ones start at `self.lookback`, large ones (at least
    /// [`LARGE_ACCOUNT_BYTES`]) at [`LARGE_FIRST_WINDOW`] slots, both
    /// widening to `self.deep_lookback`. Errors on one group do not lose
    /// the other's results.
    pub fn latest_before_sized(
        &self,
        accounts: &[(String, usize)],
        slot: u64,
    ) -> Result<HashMap<String, Version>> {
        let mut large: Vec<String> = Vec::new();
        let mut small: Vec<String> = Vec::new();
        for (account, size) in accounts {
            if *size >= LARGE_ACCOUNT_BYTES {
                large.push(account.clone());
            } else {
                small.push(account.clone());
            }
        }
        let mut found = HashMap::new();
        let mut first_error = None;
        for (group, first) in [(small, self.lookback), (large, LARGE_FIRST_WINDOW)] {
            if group.is_empty() {
                continue;
            }
            match self.latest_before_windows(&group, slot, first, self.deep_lookback) {
                Ok(got) => found.extend(got),
                Err(e) => {
                    first_error.get_or_insert(e);
                }
            }
        }
        match first_error {
            Some(e) if found.is_empty() => Err(e),
            Some(e) => {
                eprintln!("history stream: {e}");
                Ok(found)
            }
            None => Ok(found),
        }
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
        if accounts.is_empty() || slot == 0 {
            return Ok(HashMap::new());
        }
        let start = slot.saturating_sub(lookback).max(1);
        self.latest_before_in(accounts, start, slot)
    }

    /// The last change of each of `accounts` in `[start, slot)`.
    fn latest_before_in(
        &self,
        accounts: &[String],
        start: u64,
        slot: u64,
    ) -> Result<HashMap<String, Version>> {
        let mut out = HashMap::new();
        if accounts.is_empty() || slot == 0 || start >= slot {
            return Ok(out);
        }
        for chunk in accounts.chunks(BATCH) {
            let filter = chunk
                .iter()
                .map(|a| format!("account:{a}"))
                .collect::<Vec<_>>()
                .join(" || ");
            let _slot = StreamSlot::acquire();
            // Keys in order, skipping any that hit the limit recently. A
            // transient failure gets one retry on the same key; a limit
            // answer backs that key off and moves to the next.
            let mut last_error = Error::Fixture("history stream: every key is backing off".into());
            let mut result: Option<Vec<AccountsAt>> = None;
            for key in self.keys.iter().filter(|k| !key_backed_off(k)) {
                let call = substreams::Call {
                    endpoint: &self.endpoint,
                    api_key: key,
                    modules: &self.modules,
                    output_module: MODULE,
                    params: &filter,
                    start,
                    stop: slot,
                    deadline: CALL_DEADLINE,
                };
                let mut attempt = substreams::stream_accounts(&call);
                // A limit answer right after our own previous call is the
                // server still counting that finished stream: wait a few
                // seconds and ask again before giving the key up.
                for wait in LIMIT_RETRY_WAITS {
                    match &attempt {
                        Err(e) if substreams::is_stream_limit(&e.to_string()) => {
                            std::thread::sleep(*wait);
                            attempt = substreams::stream_accounts(&call);
                        }
                        Err(_) => {
                            std::thread::sleep(Duration::from_secs(2));
                            attempt = substreams::stream_accounts(&call);
                            break;
                        }
                        Ok(_) => break,
                    }
                }
                match attempt {
                    Ok(blocks) => {
                        result = Some(blocks);
                        break;
                    }
                    Err(e) => {
                        if substreams::is_stream_limit(&e.to_string()) {
                            back_off_key(key);
                        }
                        last_error = Error::Fixture(format!("history stream: {e}"));
                    }
                }
            }
            let Some(blocks) = result else {
                return Err(last_error);
            };
            out.extend(collect_versions(blocks, slot));
        }
        Ok(out)
    }
}

/// The last version per account from streamed blocks, for blocks strictly
/// before `slot`.
pub(crate) fn collect_versions(blocks: Vec<AccountsAt>, slot: u64) -> HashMap<String, Version> {
    let mut out: HashMap<String, Version> = HashMap::new();
    for block in blocks {
        if block.slot >= slot {
            continue;
        }
        for a in block.accounts {
            let address = bs58::encode(&a.address).into_string();
            let state = if a.deleted {
                None
            } else {
                Some(AccountState {
                    data: a.data,
                    // The stream carries no lamports; the caller fills them in
                    // from the account's current balance (see `apply`).
                    lamports: 0,
                    owner: bs58::encode(&a.owner).into_string(),
                })
            };
            let newer = out.get(&address).is_none_or(|v| block.slot >= v.slot);
            if newer {
                out.insert(
                    address,
                    Version {
                        slot: block.slot,
                        state,
                    },
                );
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
    use {super::*, crate::substreams::Account};

    fn acct(address: u8, data: &[u8], deleted: bool) -> Account {
        Account {
            address: vec![address; 32],
            owner: vec![7; 32],
            data: data.to_vec(),
            deleted,
        }
    }

    #[test]
    fn keeps_the_last_change_before_the_slot_per_account() {
        let blocks = vec![
            AccountsAt {
                slot: 100,
                accounts: vec![acct(1, &[1, 2, 3], false)],
            },
            AccountsAt {
                slot: 150,
                accounts: vec![acct(1, &[9], false), acct(2, &[], true)],
            },
            AccountsAt {
                slot: 200,
                accounts: vec![acct(1, &[1, 2, 3], false)],
            },
        ];
        let out = collect_versions(blocks, 200);
        let a = &out[&bs58::encode([1u8; 32]).into_string()];
        assert_eq!(a.slot, 150);
        assert_eq!(a.state.as_ref().unwrap().data, vec![9]);
        assert_eq!(
            a.state.as_ref().unwrap().owner,
            bs58::encode([7u8; 32]).into_string()
        );
        let b = &out[&bs58::encode([2u8; 32]).into_string()];
        assert!(b.state.is_none());
        assert_eq!(b.slot, 150);
    }

    #[test]
    fn lamports_fall_back_to_the_rent_minimum() {
        assert_eq!(lamports_for(Some(5), 100), 5);
        assert_eq!(lamports_for(None, 100), 228 * 6_960);
    }

    #[test]
    fn the_bundled_package_loads() {
        let modules = substreams::package_modules(PACKAGE).unwrap();
        assert!(modules.modules.iter().any(|m| m.name == MODULE));
    }
}
