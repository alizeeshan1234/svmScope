//! Caching + rate limiting for the public server.
//!
//! Both are in-process and dependency-free: this is a single-instance service, so
//! a `Mutex<HashMap>` is the right amount of machinery. If it ever runs multiple
//! replicas these become per-replica, which is still correct — just less effective.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

// ---------------------------------------------------------------------------
// Response cache
// ---------------------------------------------------------------------------

/// A cached response body plus when it was stored.
struct Entry {
    body: String,
    stored: Instant,
}

fn cache() -> &'static Mutex<HashMap<String, Entry>> {
    static CACHE: OnceLock<Mutex<HashMap<String, Entry>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// How long a cached response stays fresh. A landed transaction's decode never
/// changes, but replay depends on current chain state, so keep it modest.
const TTL: Duration = Duration::from_secs(120);
/// Cap entries so a flood of distinct signatures can't grow memory without bound.
const MAX_ENTRIES: usize = 500;

/// Look up a fresh cached body for `key`.
pub fn cache_get(key: &str) -> Option<String> {
    let map = cache().lock().ok()?;
    let e = map.get(key)?;
    (e.stored.elapsed() < TTL).then(|| e.body.clone())
}

/// Store a response body, evicting expired entries (and, if still full, the
/// oldest) so the map stays bounded.
pub fn cache_put(key: String, body: String) {
    let Ok(mut map) = cache().lock() else { return };
    map.retain(|_, e| e.stored.elapsed() < TTL);
    if map.len() >= MAX_ENTRIES {
        if let Some(oldest) = map
            .iter()
            .min_by_key(|(_, e)| e.stored)
            .map(|(k, _)| k.clone())
        {
            map.remove(&oldest);
        }
    }
    map.insert(
        key,
        Entry {
            body,
            stored: Instant::now(),
        },
    );
}

// ---------------------------------------------------------------------------
// Rate limiting
// ---------------------------------------------------------------------------

/// Request timestamps per client, newest last.
fn hits() -> &'static Mutex<HashMap<String, Vec<Instant>>> {
    static HITS: OnceLock<Mutex<HashMap<String, Vec<Instant>>>> = OnceLock::new();
    HITS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Sliding window and its allowance. Simulation is expensive, so this is tuned to
/// let a person browse freely while stopping a script from monopolising the box.
const WINDOW: Duration = Duration::from_secs(60);
const MAX_PER_WINDOW: usize = 40;

/// Record a hit for `client` and report whether it is within the limit.
/// Returns `Err(retry_after_seconds)` when the client is over.
pub fn rate_check(client: &str) -> Result<(), u64> {
    let Ok(mut map) = hits().lock() else {
        return Ok(());
    };

    // Drop clients that have gone quiet, so the map doesn't grow forever.
    map.retain(|_, v| v.last().is_some_and(|t| t.elapsed() < WINDOW));

    let now = Instant::now();
    let v = map.entry(client.to_string()).or_default();
    v.retain(|t| t.elapsed() < WINDOW);

    if v.len() >= MAX_PER_WINDOW {
        let oldest = v.first().copied().unwrap_or(now);
        let wait = WINDOW.saturating_sub(oldest.elapsed()).as_secs().max(1);
        return Err(wait);
    }
    v.push(now);
    Ok(())
}
