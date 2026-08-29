//! Private usage tracking for the public server.
//!
//! In-process and dependency-free, in the same spirit as `guard`: a single
//! `Mutex<Stats>` counts real API usage (how many analyses ran, from how many
//! distinct clients) so the operator can tell whether anyone is actually using
//! svmscope. It is only ever exposed through the token-gated `/stats` route, so
//! the numbers are never visible to the public.
//!
//! Client identities are stored **hashed**, never as raw IPs, so the on-disk
//! file holds no personal data — only distinct-client counts.
//!
//! Persistence is best-effort to `SVMSCOPE_STATS_FILE` (default
//! `svmscope-stats.json`). On a host with a durable disk the counts accumulate
//! across restarts; on an ephemeral free tier they reset on redeploy, which is
//! fine for a "is anyone using it" pulse.

use serde::{Deserialize, Serialize};
use std::collections::{hash_map::DefaultHasher, BTreeMap, HashMap};
use std::hash::{Hash, Hasher};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// One anonymous client's activity.
#[derive(Serialize, Deserialize, Default, Clone)]
struct ClientRec {
    count: u64,
    first: u64,
    last: u64,
}

/// The whole tally. Serialized verbatim to disk.
#[derive(Serialize, Deserialize, Default)]
struct Stats {
    /// Total metered API requests served.
    total: u64,
    /// Requests broken down by endpoint label ("analyze", "replay", ...).
    per_endpoint: BTreeMap<String, u64>,
    /// Requests per UTC day ("YYYY-MM-DD" -> count).
    per_day: BTreeMap<String, u64>,
    /// Hashed client id -> activity. Length is the unique-client count.
    clients: HashMap<String, ClientRec>,
    /// Unix seconds of the first and most recent metered request.
    first_seen: u64,
    last_seen: u64,
}

fn stats() -> &'static Mutex<Stats> {
    static STATS: OnceLock<Mutex<Stats>> = OnceLock::new();
    STATS.get_or_init(|| Mutex::new(Stats::default()))
}

fn file_path() -> String {
    std::env::var("SVMSCOPE_STATS_FILE").unwrap_or_else(|_| "svmscope-stats.json".to_string())
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// A non-reversible short hash of the client id, so raw IPs never hit disk.
fn hash_client(raw: &str) -> String {
    let mut h = DefaultHasher::new();
    raw.hash(&mut h);
    format!("{:016x}", h.finish())
}

/// UTC calendar date for a unix timestamp, as "YYYY-MM-DD".
/// Uses Howard Hinnant's days-from-civil algorithm — no chrono dependency.
fn utc_date(secs: u64) -> String {
    let z = (secs / 86_400) as i64 + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}")
}

/// Load any persisted tally at startup. Silent if the file is absent or unreadable.
pub fn load() {
    let Ok(text) = std::fs::read_to_string(file_path()) else {
        return;
    };
    if let Ok(loaded) = serde_json::from_str::<Stats>(&text) {
        if let Ok(mut s) = stats().lock() {
            *s = loaded;
        }
    }
}

/// Persist at most once every few seconds, so a burst of requests doesn't thrash
/// the disk. Called while the stats lock is held.
fn maybe_save(s: &Stats) {
    static LAST: OnceLock<Mutex<Instant>> = OnceLock::new();
    let last = LAST.get_or_init(|| Mutex::new(Instant::now() - Duration::from_secs(3600)));
    if let Ok(mut l) = last.lock() {
        if l.elapsed() >= Duration::from_secs(5) {
            if let Ok(text) = serde_json::to_string(s) {
                let _ = std::fs::write(file_path(), text);
            }
            *l = Instant::now();
        }
    }
}

/// Record one metered API request from `client_raw` against `endpoint`.
pub fn record(endpoint: &str, client_raw: &str) {
    let ts = now();
    let Ok(mut s) = stats().lock() else {
        return;
    };

    s.total += 1;
    *s.per_endpoint.entry(endpoint.to_string()).or_insert(0) += 1;
    *s.per_day.entry(utc_date(ts)).or_insert(0) += 1;
    if s.first_seen == 0 {
        s.first_seen = ts;
    }
    s.last_seen = ts;

    let rec = s.clients.entry(hash_client(client_raw)).or_default();
    rec.count += 1;
    if rec.first == 0 {
        rec.first = ts;
    }
    rec.last = ts;

    maybe_save(&s);
}

/// A private JSON summary for the token-gated `/stats` endpoint.
pub fn snapshot_json() -> serde_json::Value {
    let ts = now();
    let Ok(s) = stats().lock() else {
        return serde_json::json!({ "error": "stats unavailable" });
    };

    let day_ago = ts.saturating_sub(86_400);
    let week_ago = ts.saturating_sub(7 * 86_400);
    let active_24h = s.clients.values().filter(|c| c.last >= day_ago).count();
    let active_7d = s.clients.values().filter(|c| c.last >= week_ago).count();

    // Keep only the last 30 days of the daily breakdown.
    let recent_days: BTreeMap<_, _> = s
        .per_day
        .iter()
        .rev()
        .take(30)
        .map(|(k, v)| (k.clone(), *v))
        .collect();

    serde_json::json!({
        "total_requests": s.total,
        "unique_clients": s.clients.len(),
        "active_clients_24h": active_24h,
        "active_clients_7d": active_7d,
        "per_endpoint": s.per_endpoint,
        "per_day_last_30": recent_days,
        "first_seen_unix": s.first_seen,
        "last_seen_unix": s.last_seen,
        "generated_at_unix": ts,
    })
}
