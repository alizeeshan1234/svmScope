//! Private usage tracking for the public server: who is using svmscope, how
//! much, and which parts.
//!
//! In-process and dependency-free, in the same spirit as `guard`: a single
//! `Mutex<Stats>` counts the work-doing API requests the rate limiter lets
//! through and the page views the UI reports on load, both per anonymous
//! client. It is only ever exposed through the token-gated `/stats` route
//! and the `/analytics` page that reads it, so the numbers are never public.
//!
//! Client identities are stored **hashed**, never as raw IPs, so nothing on
//! disk or in the tally is personal data — only distinct-client counts.
//!
//! Persistence has two layers. The file at `SVMSCOPE_STATS_FILE` (default
//! `svmscope-stats.json`) is written on a short debounce; on a host with a
//! durable disk that alone accumulates across restarts. The free tier's disk
//! is not durable and the instance restarts many times a day, so when
//! `SVMSCOPE_STATS_GITHUB=owner/repo` (or, failing that, the records queue's
//! `SVMSCOPE_RECORD_GITHUB`) and `SVMSCOPE_GITHUB_TOKEN` are set, the tally is
//! also kept as one asset on a release tagged `stats`: restored at boot,
//! pushed every few minutes while it changes, and pushed once more on
//! shutdown.

use serde::{Deserialize, Serialize};
use std::collections::{hash_map::DefaultHasher, BTreeMap, HashMap};
use std::hash::{Hash, Hasher};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use svmscope::records::github::GithubQueue;

/// The release tag and asset name the durable copy lives under.
const RELEASE_TAG: &str = "stats";
const ASSET_NAME: &str = "svmscope-stats.json";
/// How long per-day detail (clients per day, endpoints per day) is kept.
const DETAIL_DAYS: u64 = 90;
/// Distinct referrer hosts remembered before new ones are folded into "other".
const MAX_REFERRERS: usize = 500;
/// Clients listed in a snapshot (newest activity first).
const SNAPSHOT_CLIENTS: usize = 300;

/// One anonymous client's activity.
#[derive(Serialize, Deserialize, Default, Clone)]
struct ClientRec {
    /// Metered API requests.
    count: u64,
    first: u64,
    last: u64,
    /// Page loads reported by the UI.
    #[serde(default)]
    views: u64,
    /// Requests by endpoint label.
    #[serde(default)]
    endpoints: BTreeMap<String, u64>,
    /// The coarse user-agent family last seen ("browser", "curl", ...).
    #[serde(default)]
    agent: String,
}

/// One client's activity on one day.
#[derive(Serialize, Deserialize, Default, Clone, Copy)]
struct DayCount {
    /// Metered API requests.
    r: u64,
    /// Page loads.
    v: u64,
}

/// The whole tally. Serialized verbatim to disk and to the release asset.
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
    /// Unix seconds of the first and most recent metered request or view.
    first_seen: u64,
    last_seen: u64,
    /// Total page loads reported by the UI.
    #[serde(default)]
    views: u64,
    /// Page loads by page kind ("index", "tx", "debug", ...).
    #[serde(default)]
    per_page: BTreeMap<String, u64>,
    /// Page loads per UTC day.
    #[serde(default)]
    views_per_day: BTreeMap<String, u64>,
    /// Page loads by the host that linked here ("direct" when none).
    #[serde(default)]
    referrers: BTreeMap<String, u64>,
    /// Requests per UTC day, by endpoint label.
    #[serde(default)]
    per_day_endpoint: BTreeMap<String, BTreeMap<String, u64>>,
    /// Requests and views by user-agent family.
    #[serde(default)]
    agents: BTreeMap<String, u64>,
    /// Each UTC day's activity by hashed client, so the page can filter
    /// people out (the operator, scripts) and still draw the days.
    #[serde(default)]
    clients_per_day: BTreeMap<String, BTreeMap<String, DayCount>>,
}

/// What the last durable push did, for the snapshot's health line.
#[derive(Default, Clone)]
struct PushState {
    last_attempt: u64,
    last_ok: u64,
    last_error: Option<String>,
}

fn stats() -> &'static Mutex<Stats> {
    static STATS: OnceLock<Mutex<Stats>> = OnceLock::new();
    STATS.get_or_init(|| Mutex::new(Stats::default()))
}

fn push_state() -> &'static Mutex<PushState> {
    static P: OnceLock<Mutex<PushState>> = OnceLock::new();
    P.get_or_init(|| Mutex::new(PushState::default()))
}

/// Set whenever the tally changes; cleared by a successful durable push.
static DIRTY: AtomicBool = AtomicBool::new(false);

fn file_path() -> String {
    std::env::var("SVMSCOPE_STATS_FILE").unwrap_or_else(|_| "svmscope-stats.json".to_string())
}

/// The `owner/repo` the durable copy lives in, if configured.
fn github_repo() -> Option<String> {
    std::env::var("SVMSCOPE_STATS_GITHUB")
        .or_else(|_| std::env::var("SVMSCOPE_RECORD_GITHUB"))
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| s.contains('/'))
}

fn github_queue() -> Option<GithubQueue> {
    let repo = github_repo()?;
    let (owner, name) = repo.split_once('/')?;
    let token = std::env::var("SVMSCOPE_GITHUB_TOKEN").ok()?;
    GithubQueue::new(owner, name, token.trim()).ok()
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

/// The coarse family of a `User-Agent`, enough to tell a person in a browser
/// from a script, a bot, or the SDK — never the string itself.
pub fn agent_family(ua: &str) -> &'static str {
    let ua = ua.trim();
    if ua.is_empty() {
        return "none";
    }
    let l = ua.to_ascii_lowercase();
    if l.contains("svmscope") {
        return "sdk";
    }
    if l.starts_with("curl/") || l.starts_with("wget/") || l.starts_with("httpie/") {
        return "curl";
    }
    const BOTS: [&str; 12] = [
        "bot",
        "crawler",
        "spider",
        "slurp",
        "facebookexternalhit",
        "preview",
        "embedly",
        "quora link",
        "headlesschrome",
        "lighthouse",
        "pingdom",
        "uptimerobot",
    ];
    if BOTS.iter().any(|b| l.contains(b)) {
        return "bot";
    }
    const SCRIPTS: [&str; 13] = [
        "python",
        "aiohttp",
        "httpx",
        "go-http-client",
        "node-fetch",
        "undici",
        "axios",
        "reqwest",
        "okhttp",
        "java/",
        "libwww",
        "postman",
        "insomnia",
    ];
    if SCRIPTS.iter().any(|s| l.contains(s)) {
        return "script";
    }
    if l.starts_with("mozilla/") {
        return "browser";
    }
    "other"
}

/// The page kinds the UI may report; anything else is "other".
pub fn page_label(page: &str) -> &'static str {
    match page {
        "index" => "index",
        "tx" => "tx",
        "debug" => "debug",
        "flame" => "flame",
        "address" => "address",
        "watch" => "watch",
        "analytics" => "analytics",
        _ => "other",
    }
}

/// A referrer host as the UI reported it, reduced to something safe to key a
/// map on: lowercase host characters only, bounded, "direct" when absent.
pub fn referrer_label(host: &str) -> String {
    let h: String = host
        .trim()
        .to_ascii_lowercase()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | ':'))
        .take(80)
        .collect();
    if h.is_empty() {
        "direct".to_string()
    } else {
        h
    }
}

/// Load any persisted tally at startup: the local file first, then — when a
/// repository is configured — the release asset, which wins if it is newer
/// (after a redeploy the local file is empty or stale). Silent on failure.
pub fn init() {
    if let Ok(text) = std::fs::read_to_string(file_path()) {
        if let Ok(loaded) = serde_json::from_str::<Stats>(&text) {
            if let Ok(mut s) = stats().lock() {
                *s = loaded;
            }
        }
    }
    let Some(q) = github_queue() else {
        return;
    };
    let remote = q
        .ensure_tagged_release(
            RELEASE_TAG,
            "usage tally",
            "svmscope's private usage counters, replaced on every push. No personal data: clients are hashed.",
        )
        .and_then(|id| q.download_asset(id, ASSET_NAME));
    match remote {
        Ok(Some(bytes)) => match serde_json::from_slice::<Stats>(&bytes) {
            Ok(remote) => {
                if let Ok(mut s) = stats().lock() {
                    if remote.last_seen >= s.last_seen {
                        *s = remote;
                        eprintln!(
                            "stats: restored {} requests, {} views, {} clients from {}",
                            s.total,
                            s.views,
                            s.clients.len(),
                            github_repo().unwrap_or_default()
                        );
                    }
                }
            }
            Err(e) => eprintln!("stats: durable copy unreadable: {e}"),
        },
        Ok(None) => eprintln!("stats: no durable copy yet; starting from the local file"),
        Err(e) => eprintln!("stats: restore failed: {e}"),
    }
}

/// Persist to the local file at most once every few seconds, so a burst of
/// requests doesn't thrash the disk. Called while the stats lock is held.
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

/// Drop per-day detail older than [`DETAIL_DAYS`]. The daily totals stay.
fn prune(s: &mut Stats, ts: u64) {
    let cutoff = utc_date(ts.saturating_sub(DETAIL_DAYS * 86_400));
    s.clients_per_day.retain(|d, _| *d >= cutoff);
    s.per_day_endpoint.retain(|d, _| *d >= cutoff);
}

/// The bookkeeping every event shares: the day, the client, the agent family.
fn touch(s: &mut Stats, ts: u64, client_raw: &str, agent: &str) -> String {
    let day = utc_date(ts);
    if s.first_seen == 0 {
        s.first_seen = ts;
    }
    s.last_seen = ts;
    let family = agent_family(agent);
    *s.agents.entry(family.to_string()).or_insert(0) += 1;
    let id = hash_client(client_raw);
    s.clients_per_day
        .entry(day)
        .or_default()
        .entry(id.clone())
        .or_default();
    let rec = s.clients.entry(id.clone()).or_default();
    if rec.first == 0 {
        rec.first = ts;
    }
    rec.last = ts;
    rec.agent = family.to_string();
    DIRTY.store(true, Ordering::SeqCst);
    id
}

/// Record one metered API request from `client_raw` against `endpoint`.
pub fn record(endpoint: &str, client_raw: &str, agent: &str) {
    let ts = now();
    let Ok(mut s) = stats().lock() else {
        return;
    };
    let day = utc_date(ts);
    s.total += 1;
    *s.per_endpoint.entry(endpoint.to_string()).or_insert(0) += 1;
    *s.per_day.entry(day.clone()).or_insert(0) += 1;
    *s.per_day_endpoint
        .entry(day)
        .or_default()
        .entry(endpoint.to_string())
        .or_insert(0) += 1;
    let id = touch(&mut s, ts, client_raw, agent);
    if let Some(rec) = s.clients.get_mut(&id) {
        rec.count += 1;
        *rec.endpoints.entry(endpoint.to_string()).or_insert(0) += 1;
    }
    if let Some(dc) = s
        .clients_per_day
        .get_mut(&utc_date(ts))
        .and_then(|d| d.get_mut(&id))
    {
        dc.r += 1;
    }
    if s.per_day_endpoint.len() > DETAIL_DAYS as usize + 1 {
        prune(&mut s, ts);
    }
    maybe_save(&s);
}

/// Record one page load of `page` (see [`page_label`]) that arrived from
/// `referrer` (see [`referrer_label`]).
pub fn record_view(page: &str, referrer: &str, client_raw: &str, agent: &str) {
    let ts = now();
    let Ok(mut s) = stats().lock() else {
        return;
    };
    let day = utc_date(ts);
    s.views += 1;
    *s.per_page.entry(page_label(page).to_string()).or_insert(0) += 1;
    *s.views_per_day.entry(day).or_insert(0) += 1;
    let referrer = referrer_label(referrer);
    let key = if s.referrers.len() >= MAX_REFERRERS && !s.referrers.contains_key(&referrer) {
        "other".to_string()
    } else {
        referrer
    };
    *s.referrers.entry(key).or_insert(0) += 1;
    let id = touch(&mut s, ts, client_raw, agent);
    if let Some(rec) = s.clients.get_mut(&id) {
        rec.views += 1;
    }
    if let Some(dc) = s
        .clients_per_day
        .get_mut(&utc_date(ts))
        .and_then(|d| d.get_mut(&id))
    {
        dc.v += 1;
    }
    if s.clients_per_day.len() > DETAIL_DAYS as usize + 1 {
        prune(&mut s, ts);
    }
    maybe_save(&s);
}

/// Write the tally to the local file and, when configured, to the release
/// asset. Blocking: network in the GitHub case. A no-op unless something
/// changed since the last successful push, or `force`.
pub fn push(force: bool) {
    if !force && !DIRTY.load(Ordering::SeqCst) {
        return;
    }
    let text = {
        let Ok(s) = stats().lock() else {
            return;
        };
        match serde_json::to_string(&*s) {
            Ok(t) => t,
            Err(_) => return,
        }
    };
    let _ = std::fs::write(file_path(), &text);
    let Some(q) = github_queue() else {
        // Nowhere durable to go; the file is all there is.
        DIRTY.store(false, Ordering::SeqCst);
        return;
    };
    let ts = now();
    let result = q
        .ensure_tagged_release(
            RELEASE_TAG,
            "usage tally",
            "svmscope's private usage counters, replaced on every push. No personal data: clients are hashed.",
        )
        .and_then(|id| q.upload(id, ASSET_NAME, text.into_bytes()));
    if let Ok(mut p) = push_state().lock() {
        p.last_attempt = ts;
        match &result {
            Ok(()) => {
                p.last_ok = ts;
                p.last_error = None;
            }
            Err(e) => p.last_error = Some(e.to_string()),
        }
    }
    match result {
        Ok(()) => DIRTY.store(false, Ordering::SeqCst),
        Err(e) => eprintln!("stats: push failed: {e}"),
    }
}

/// A private JSON summary for the token-gated `/stats` endpoint. `you_raw` is
/// the caller's own client id, so the page can mark and exclude the operator.
pub fn snapshot_json(you_raw: &str) -> serde_json::Value {
    let ts = now();
    let Ok(s) = stats().lock() else {
        return serde_json::json!({ "error": "stats unavailable" });
    };
    let you = hash_client(you_raw);

    let day_ago = ts.saturating_sub(86_400);
    let week_ago = ts.saturating_sub(7 * 86_400);
    let active_24h = s.clients.values().filter(|c| c.last >= day_ago).count();
    let active_7d = s.clients.values().filter(|c| c.last >= week_ago).count();
    let new_7d = s.clients.values().filter(|c| c.first >= week_ago).count();

    // The last 30 UTC days, every day present, oldest first.
    let today = ts / 86_400;
    let days: Vec<serde_json::Value> = (0..30)
        .rev()
        .map(|back| {
            let date = utc_date((today - back) * 86_400);
            serde_json::json!({
                "date": date,
                "requests": s.per_day.get(&date).copied().unwrap_or(0),
                "views": s.views_per_day.get(&date).copied().unwrap_or(0),
                "clients": s.clients_per_day.get(&date).map(|c| c.len()).unwrap_or(0),
                "by_client": s.clients_per_day.get(&date).cloned().unwrap_or_default(),
                "endpoints": s.per_day_endpoint.get(&date).cloned().unwrap_or_default(),
            })
        })
        .collect();

    let mut clients: Vec<(&String, &ClientRec)> = s.clients.iter().collect();
    clients.sort_by(|a, b| b.1.last.cmp(&a.1.last).then(a.0.cmp(b.0)));
    let clients: Vec<serde_json::Value> = clients
        .into_iter()
        .take(SNAPSHOT_CLIENTS)
        .map(|(id, c)| {
            serde_json::json!({
                "id": id,
                "requests": c.count,
                "views": c.views,
                "first": c.first,
                "last": c.last,
                "agent": c.agent,
                "endpoints": c.endpoints,
            })
        })
        .collect();

    let push = push_state().lock().map(|p| p.clone()).unwrap_or_default();

    serde_json::json!({
        "total_requests": s.total,
        "total_views": s.views,
        "unique_clients": s.clients.len(),
        "active_clients_24h": active_24h,
        "active_clients_7d": active_7d,
        "new_clients_7d": new_7d,
        "per_endpoint": s.per_endpoint,
        "per_page": s.per_page,
        "referrers": s.referrers,
        "agents": s.agents,
        "days": days,
        "clients": clients,
        "you": you,
        "first_seen_unix": s.first_seen,
        "last_seen_unix": s.last_seen,
        "generated_at_unix": ts,
        "persistence": {
            "file": file_path(),
            "github": github_repo(),
            "dirty": DIRTY.load(Ordering::SeqCst),
            "last_push_attempt_unix": push.last_attempt,
            "last_push_ok_unix": push.last_ok,
            "last_push_error": push.last_error,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utc_dates() {
        assert_eq!(utc_date(0), "1970-01-01");
        assert_eq!(utc_date(1_790_000_000), "2026-09-21");
    }

    #[test]
    fn agent_families() {
        assert_eq!(agent_family(""), "none");
        assert_eq!(agent_family("curl/8.4.0"), "curl");
        assert_eq!(
            agent_family("Mozilla/5.0 (Macintosh) Chrome/130"),
            "browser"
        );
        assert_eq!(
            agent_family("Mozilla/5.0 (compatible; Googlebot/2.1)"),
            "bot"
        );
        assert_eq!(agent_family("python-requests/2.32"), "script");
        assert_eq!(agent_family("node-fetch/1.0"), "script");
        assert_eq!(agent_family("svmscope-sdk/0.6"), "sdk");
        assert_eq!(agent_family("Twitterbot/1.0"), "bot");
    }

    #[test]
    fn labels_are_bounded() {
        assert_eq!(page_label("tx"), "tx");
        assert_eq!(page_label("<script>"), "other");
        assert_eq!(referrer_label(""), "direct");
        assert_eq!(referrer_label("  T.co/"), "t.co");
        assert_eq!(referrer_label(&"a".repeat(200)).len(), 80);
    }

    #[test]
    fn tally_round_trips_and_counts() {
        // The tests share one process-wide tally, so count deltas.
        let before = stats().lock().unwrap().total;
        record("analyze", "10.0.0.1", "curl/8");
        record("analyze", "10.0.0.1", "curl/8");
        record("trace", "10.0.0.2", "Mozilla/5.0 x");
        record_view("tx", "t.co", "10.0.0.3", "Mozilla/5.0 x");
        let snap = snapshot_json("10.0.0.1");
        assert_eq!(snap["total_requests"].as_u64().unwrap(), before + 3);
        assert_eq!(snap["you"].as_str().unwrap(), hash_client("10.0.0.1"));
        assert!(snap["unique_clients"].as_u64().unwrap() >= 3);
        assert_eq!(snap["days"].as_array().unwrap().len(), 30);
        assert!(snap["per_page"]["tx"].as_u64().unwrap() >= 1);
        assert!(snap["referrers"]["t.co"].as_u64().unwrap() >= 1);
        let text = serde_json::to_string(&*stats().lock().unwrap()).unwrap();
        let back: Stats = serde_json::from_str(&text).unwrap();
        assert_eq!(back.total, before + 3);
        assert!(back.clients[&hash_client("10.0.0.1")].endpoints["analyze"] >= 2);
        let today = snap["days"].as_array().unwrap().last().unwrap();
        assert!(
            today["by_client"][hash_client("10.0.0.1")]["r"]
                .as_u64()
                .unwrap()
                >= 2
        );
        assert!(
            today["by_client"][hash_client("10.0.0.3")]["v"]
                .as_u64()
                .unwrap()
                >= 1
        );
    }

    #[test]
    fn old_files_still_load() {
        let old = r#"{"total":1,"per_endpoint":{"analyze":1},"per_day":{"2026-08-27":1},"clients":{"abc":{"count":1,"first":1,"last":2}},"first_seen":1,"last_seen":2}"#;
        let s: Stats = serde_json::from_str(old).unwrap();
        assert_eq!(s.views, 0);
        assert_eq!(s.clients["abc"].agent, "");
    }
}
