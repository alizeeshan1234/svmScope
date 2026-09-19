//! svmscope web server — serves the frontend and a JSON analysis endpoint.
//!
//! Run with `cargo run --bin server`, then open http://127.0.0.1:3000.

mod guard;
mod relay;
mod stats;

use axum::{
    body::Body,
    extract::ws::{Message, WebSocket, WebSocketUpgrade},
    extract::{ConnectInfo, Path, Query, Request},
    http::{header, StatusCode},
    middleware::{self, Next},
    response::{Html, IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::net::{IpAddr, SocketAddr, ToSocketAddrs};
use svmscope::spec::{MutationInput, SuiteRequest};
use svmscope::{Analysis, Mutation, ReplayResult, ScenarioOutcome, Scope, TimeTravel};

const DEFAULT_RPC: &str = "https://api.mainnet-beta.solana.com";

/// The default RPC, overridable via `SVMSCOPE_RPC_URL` / `RPC_URL`, used when a
/// request doesn't specify a cluster.
fn rpc_url() -> String {
    std::env::var("SVMSCOPE_RPC_URL")
        .or_else(|_| std::env::var("RPC_URL"))
        .unwrap_or_else(|_| DEFAULT_RPC.to_string())
}

/// Read an env var only if it holds an http(s) URL.
fn env_http(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| v.starts_with("http"))
}

/// A per-cluster RPC override, so a deployment can use its own (faster, higher
/// rate-limit) endpoint for each cluster:
///   SVMSCOPE_RPC_URL_MAINNET / _DEVNET / _TESTNET
///
/// Mainnet also falls back to the generic `SVMSCOPE_RPC_URL` / `RPC_URL`, because
/// the UI always tags its requests `cluster=mainnet` — so setting that single var
/// to a paid endpoint "just works" for the traffic that actually flows, instead of
/// being silently ignored in favour of the public node.
fn cluster_env_rpc(cluster: Option<&str>) -> Option<String> {
    match cluster.map(|c| c.trim().to_ascii_lowercase()).as_deref() {
        Some("devnet") | Some("d") => env_http("SVMSCOPE_RPC_URL_DEVNET"),
        Some("testnet") | Some("t") => env_http("SVMSCOPE_RPC_URL_TESTNET"),
        Some("mainnet") | Some("mainnet-beta") | Some("m") => env_http("SVMSCOPE_RPC_URL_MAINNET")
            .or_else(|| env_http("SVMSCOPE_RPC_URL"))
            .or_else(|| env_http("RPC_URL")),
        // A validator on another port or another box on the desk: the operator
        // names it, never the caller, so this stays outside the SSRF surface.
        Some("localnet") | Some("local") | Some("localhost") | Some("l") => {
            env_http("SVMSCOPE_RPC_URL_LOCALNET")
        }
        _ => None,
    }
}

/// Per-request cluster selection: `?cluster=devnet` (or mainnet/testnet/localnet)
/// or `?rpc=<url>`, so one instance serves every cluster.
#[derive(Deserialize)]
struct ClusterQuery {
    cluster: Option<String>,
    rpc: Option<String>,
    /// A page offering to make this request's RPC calls from the reader's own
    /// browser, so a validator on their machine is reachable. See [`relay`].
    relay: Option<String>,
    /// Optional archival RPC for exact historical state; vetted like `rpc`.
    archive: Option<String>,
    /// For the what-if endpoints: build on the replay as of this slot.
    slot: Option<u64>,
}

/// True if `ip` is one the public server must never be tricked into fetching —
/// loopback, private, link-local (incl. the cloud metadata address 169.254.169.254),
/// or unspecified. This is the core of the SSRF guard on caller-supplied `?rpc=`.
fn is_blocked_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_unspecified()
                || v4.is_broadcast()
                // 100.64.0.0/10 carrier-grade NAT / cloud internal
                || (v4.octets()[0] == 100 && (64..128).contains(&v4.octets()[1]))
        }
        IpAddr::V6(v6) => {
            v6.is_loopback()
                || v6.is_unspecified()
                // unique-local fc00::/7 and link-local fe80::/10
                || (v6.segments()[0] & 0xfe00) == 0xfc00
                || (v6.segments()[0] & 0xffc0) == 0xfe80
                // IPv4-mapped: unwrap and re-check against the v4 rules
                || v6.to_ipv4_mapped().is_some_and(|m| is_blocked_ip(IpAddr::V4(m)))
        }
    }
}

/// Validate a caller-supplied RPC URL before the server will fetch through it.
/// Requires http(s), and resolves the host so a public deployment can't be aimed
/// at localhost, private networks, or the cloud metadata endpoint (SSRF). Returns
/// the URL unchanged when safe. DNS that resolves to *any* blocked address is
/// rejected (defends the obvious rebind-to-metadata trick).
fn vet_custom_rpc(url: &str) -> Option<String> {
    let rest = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))?;
    // authority is everything up to the first '/', '?' or '#'; drop any
    // userinfo (`user:pass@`) first, or `evil.com:80@127.0.0.1` reads as
    // `evil.com` here while the client connects to 127.0.0.1.
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    let hostport = authority
        .rsplit_once('@')
        .map(|(_, h)| h)
        .unwrap_or(authority);
    let host = hostport
        .rsplit_once(':')
        .map(|(h, _)| h)
        .unwrap_or(hostport);
    let host = host.trim_matches(['[', ']']); // strip IPv6 brackets
    if host.is_empty() {
        return None;
    }
    // A bare IP literal is checked directly; a hostname is resolved (all A/AAAA).
    if let Ok(ip) = host.parse::<IpAddr>() {
        return (!is_blocked_ip(ip)).then(|| url.to_string());
    }
    let addrs = (host, 443u16).to_socket_addrs().ok()?;
    let mut any = false;
    for a in addrs {
        any = true;
        if is_blocked_ip(a.ip()) {
            return None;
        }
    }
    any.then(|| url.to_string())
}

/// Whether the server honors a caller-supplied `?rpc=` at all.
///
/// A *shared public* instance must not proxy arbitrary RPC URLs — even IP-vetted,
/// a re-resolved hostname (DNS rebinding) or an HTTP redirect can still reach an
/// internal target between the check and the fetch. So custom RPC is **off unless
/// the operator opts in** with `SVMSCOPE_ALLOW_CUSTOM_RPC=1` (the safe default for
/// hosted; self-hosters running locally can enable it). The built-in cluster
/// presets and env-configured endpoints are always available.
fn custom_rpc_allowed() -> bool {
    matches!(
        std::env::var("SVMSCOPE_ALLOW_CUSTOM_RPC").ok().as_deref(),
        Some("1") | Some("true") | Some("yes")
    )
}

/// The only `cluster` values a public instance will act on. Anything else — a
/// URL-shaped cluster (`cluster=http://169.254.169.254/…`) or `localnet`
/// (127.0.0.1) — is an SSRF vector, since `resolve_rpc` otherwise honors both
/// verbatim. Self-hosters (custom RPC enabled) additionally get `localnet`, but a
/// URL-shaped cluster is *never* accepted — custom endpoints must come through the
/// vetted `rpc` field, never through `cluster`.
fn public_cluster_ok(c: &str) -> bool {
    matches!(
        c.trim().to_ascii_lowercase().as_str(),
        "mainnet" | "mainnet-beta" | "m" | "devnet" | "d" | "testnet" | "t"
    )
}

/// A named localnet alias — allowed only when the operator has enabled custom RPC
/// (local dev). Still a *name*, never a URL, so it can't be an SSRF vector.
fn localnet_alias(c: &str) -> bool {
    matches!(
        c.trim().to_ascii_lowercase().as_str(),
        "localnet" | "local" | "localhost" | "l"
    )
}

/// Resolve a per-request RPC. Precedence: explicit ?rpc= (only when custom RPC is
/// enabled AND it passes the SSRF check) > per-cluster env var > cluster's public
/// endpoint > the generic env default. A caller `rpc` that is disabled or unsafe is
/// ignored, falling through to trusted sources.
/// A `Scope` for `url`, with `archive` attached when present — every replay
/// (analyze, trace, profile, replay_at_slot) then gets exact historical state.
/// Seed accounts recorded from the first poll: `seeds/mainnet.txt`, one
/// address per line, `#` comments, most-shared first; `SVMSCOPE_RECORD_MAX_SEEDS`
/// bounds how many are taken.
const BUNDLED_SEEDS: &str = include_str!("../../seeds/mainnet.txt");

/// The record store behind this instance (see `svmscope::records`), opened
/// from `SVMSCOPE_RECORD_DIR` at startup; `None` when recording is off.
static RECORDS: std::sync::LazyLock<Option<std::sync::Arc<svmscope::records::LogStore>>> =
    std::sync::LazyLock::new(|| {
        let dir = std::env::var("SVMSCOPE_RECORD_DIR").ok()?;
        let dir = dir.trim();
        if dir.is_empty() {
            return None;
        }
        match svmscope::records::LogStore::open(dir) {
            Ok(store) => {
                let store = std::sync::Arc::new(store);
                use svmscope::records::StateStore;
                // The bundled seed list: the accounts the busiest programs'
                // transactions share (pools, markets, vaults), regenerated
                // with `cargo run --example seed_list`. Recording them from
                // day one is what makes a first replay of a popular pool
                // exact instead of "watched from now on".
                // Each hot account costs about 1 MB a day in the dense tier,
                // so the bundled list is taken from the top, most-shared first,
                // up to `SVMSCOPE_RECORD_MAX_SEEDS` (default 400; 0 = none).
                let max_seeds: usize = std::env::var("SVMSCOPE_RECORD_MAX_SEEDS")
                    .ok()
                    .and_then(|v| v.trim().parse().ok())
                    .unwrap_or(400);
                let mut seeded = 0;
                for line in BUNDLED_SEEDS.lines() {
                    if seeded >= max_seeds {
                        break;
                    }
                    let addr = line.split('#').next().unwrap_or("").trim();
                    if !addr.is_empty() && store.watch(addr).is_ok() {
                        seeded += 1;
                    }
                }
                for seed in std::env::var("SVMSCOPE_RECORD_SEEDS")
                    .unwrap_or_default()
                    .split(',')
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                {
                    if store.watch(seed).is_ok() {
                        seeded += 1;
                    }
                }
                eprintln!(
                    "record store: {} watched ({seeded} seeds)",
                    store.watched().map(|w| w.len()).unwrap_or(0)
                );
                Some(store)
            }
            Err(e) => {
                eprintln!("record store disabled: {e}");
                None
            }
        }
    });

/// The durable queue behind the record store, from
/// `SVMSCOPE_RECORD_GITHUB=owner/repo` and `SVMSCOPE_GITHUB_TOKEN`.
fn github_queue() -> Option<svmscope::records::github::GithubQueue> {
    let repo = std::env::var("SVMSCOPE_RECORD_GITHUB").ok()?;
    let (owner, name) = repo.trim().split_once('/')?;
    let token = std::env::var("SVMSCOPE_GITHUB_TOKEN").ok()?;
    match svmscope::records::github::GithubQueue::new(owner, name, token.trim()) {
        Ok(q) => Some(q),
        Err(e) => {
            eprintln!("records queue disabled: {e}");
            None
        }
    }
}

/// Poll the watched set forever, recording every changed version. One
/// blocking thread; the interval (`SVMSCOPE_RECORD_INTERVAL_MS`, default
/// 2000) bounds how many slots a reconstruction has to replay forward from
/// the nearest recording. Hourly: push the new versions to the durable
/// queue, drop the day that left the window, thin the two tiers.
/// Set on SIGTERM / Ctrl-C: the recorder pushes its unpushed tail to the
/// queue and stops, so a redeploy loses nothing that was recorded.
static STOPPING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Finished as-of replays, keyed by endpoint, signature and slot. The world
/// at a past slot does not change, so the second person to open the same
/// link, or the same person coming back, gets the answer at once instead of
/// waiting minutes for the stream again. Bounded and time-limited.
type AtCache = std::collections::HashMap<
    String,
    (std::time::Instant, usize, std::sync::Arc<serde_json::Value>),
>;
static AT_CACHE: std::sync::LazyLock<std::sync::Mutex<AtCache>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashMap::new()));
const AT_CACHE_TTL: std::time::Duration = std::time::Duration::from_secs(6 * 3600);
const AT_CACHE_MAX: usize = 200;

/// Answers are held by bytes as well as by count: a whole-page `analyze_at`
/// for a busy transaction is far larger than a bare replay, so 200 of them
/// can outweigh everything else the process holds.
/// `SVMSCOPE_AT_CACHE_MB` overrides the budget.
fn at_cache_budget() -> usize {
    std::env::var("SVMSCOPE_AT_CACHE_MB")
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .unwrap_or(24)
        .saturating_mul(1024 * 1024)
}

/// The concurrency gate for the two as-of routes. Each in-flight replay
/// holds a whole reconstructed world in memory, so a burst of them is how a
/// small instance runs out of memory and answers 502 to everyone, including
/// the requests that were nearly done. Beyond this many at once, a request
/// waits for a slot instead. `SVMSCOPE_AT_CONCURRENCY` overrides it.
static AT_GATE: std::sync::LazyLock<tokio::sync::Semaphore> = std::sync::LazyLock::new(|| {
    let permits = std::env::var("SVMSCOPE_AT_CONCURRENCY")
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .filter(|n| *n > 0)
        .unwrap_or(2);
    tokio::sync::Semaphore::new(permits)
});

fn at_cache_get(key: &str) -> Option<std::sync::Arc<serde_json::Value>> {
    let mut cache = AT_CACHE.lock().ok()?;
    let (at, _, v) = cache.get_mut(key)?;
    if at.elapsed() >= AT_CACHE_TTL {
        return None;
    }
    let hit = std::sync::Arc::clone(v);
    // Reading refreshes the entry: eviction and expiry then fall on answers
    // nobody is asking for, not on the one link a crowd is following. The
    // world at a past slot does not change, so an old answer stays right.
    *at = std::time::Instant::now();
    Some(hit)
}

fn at_cache_put(key: String, value: serde_json::Value) -> std::sync::Arc<serde_json::Value> {
    let size = serde_json::to_vec(&value).map(|v| v.len()).unwrap_or(0);
    let value = std::sync::Arc::new(value);
    if let Ok(mut cache) = AT_CACHE.lock() {
        cache.retain(|_, (at, _, _)| at.elapsed() < AT_CACHE_TTL);
        cache.insert(
            key,
            (
                std::time::Instant::now(),
                size,
                std::sync::Arc::clone(&value),
            ),
        );
        let budget = at_cache_budget();
        let mut held: usize = cache.values().map(|(_, n, _)| *n).sum();
        while (cache.len() > AT_CACHE_MAX || held > budget) && cache.len() > 1 {
            let Some(oldest) = cache
                .iter()
                .min_by_key(|(_, (at, _, _))| *at)
                .map(|(k, _)| k.clone())
            else {
                break;
            };
            if let Some((_, n, _)) = cache.remove(&oldest) {
                held = held.saturating_sub(n);
            }
        }
    }
    value
}
/// Set by the recorder thread once its final push is done (or it never ran).
static RECORDER_DONE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);

fn unix_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// Resolves on SIGTERM (what Render sends before a redeploy) or Ctrl-C and
/// flags the recorder to flush.
async fn shutdown_signal() {
    #[cfg(unix)]
    {
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("install SIGTERM handler");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = term.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
    eprintln!("svmscope: shutting down");
    STOPPING.store(true, std::sync::atomic::Ordering::SeqCst);
}

fn spawn_recorder() {
    let Some(store) = RECORDS.as_ref() else {
        return;
    };
    RECORDER_DONE.store(false, std::sync::atomic::Ordering::SeqCst);
    let log_store = Some(std::sync::Arc::clone(store));
    let store: std::sync::Arc<dyn svmscope::records::StateStore> =
        std::sync::Arc::<svmscope::records::LogStore>::clone(store);
    let interval = std::env::var("SVMSCOPE_RECORD_INTERVAL_MS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(2000);
    // The recorder polls the public node by default: one getMultipleAccounts
    // per 100 accounts per round is well inside its limits, and it must never
    // spend a paid plan's credits. `SVMSCOPE_RECORD_RPC_URL` overrides.
    let url = std::env::var("SVMSCOPE_RECORD_RPC_URL")
        .ok()
        .filter(|u| !u.trim().is_empty())
        .unwrap_or_else(|| "https://api.mainnet-beta.solana.com".to_string());
    std::thread::Builder::new()
        .name("svmscope-recorder".into())
        .spawn(move || {
            // The server has no direct client dependency; a Scope's is enough.
            let scope = Scope::new(url);
            let client = scope.client();
            let queue = github_queue();
            // Restore the window from the queue after a (re)deploy.
            if let (Some(q), Some(log)) = (queue.as_ref(), log_store.as_ref()) {
                match q.restore(log) {
                    Ok(n) => eprintln!("records queue: restored {n} versions"),
                    Err(e) => eprintln!("records queue: restore failed: {e}"),
                }
            }
            // Push only what this process records: everything restored is
            // already in the queue. If the slot lookup fails the first push
            // re-sends the window once; imports dedupe by slot, so that costs
            // bandwidth, not correctness.
            let mut last_push_slot: u64 = client.get_slot().unwrap_or(0);
            // Pushes happen when the UTC hour changes (not every N rounds, so
            // restarts do not keep postponing them) and once more on shutdown.
            let mut last_push_hour = unix_secs() / 3_600;
            loop {
                match store.watched() {
                    Ok(watched) if !watched.is_empty() => {
                        if let Err(e) =
                            svmscope::records::poll_once(client, store.as_ref(), &watched)
                        {
                            eprintln!("recorder: {e}");
                        }
                    }
                    Ok(_) => {}
                    Err(e) => eprintln!("recorder: {e}"),
                }
                let stopping = STOPPING.load(std::sync::atomic::Ordering::SeqCst);
                let hour = unix_secs() / 3_600;
                let hour_changed = hour != last_push_hour;
                if let (true, Some(log)) = (stopping || hour_changed, log_store.as_ref()) {
                    match client.get_slot() {
                        Ok(slot) => {
                            if let Some(q) = queue.as_ref() {
                                match q.push_hour(log, last_push_slot) {
                                    Ok(n) => {
                                        eprintln!("records queue: pushed {n} bytes");
                                        last_push_slot = slot;
                                    }
                                    Err(e) => eprintln!("records queue: push failed: {e}"),
                                }
                            }
                            // Hourly housekeeping, not on the way out: drop
                            // the day that fell out of the window and apply
                            // the two-tier retention as of the newest slot.
                            if hour_changed && !stopping {
                                if let Some(q) = queue.as_ref() {
                                    match q.pop_old() {
                                        Ok(n) if n > 0 => {
                                            eprintln!("records queue: popped {n} day(s)")
                                        }
                                        Ok(_) => {}
                                        Err(e) => eprintln!("records queue: pop failed: {e}"),
                                    }
                                }
                                match log.thin(slot) {
                                    Ok(n) => eprintln!("recorder: thinned {n} versions"),
                                    Err(e) => eprintln!("recorder: thin failed: {e}"),
                                }
                            }
                        }
                        Err(e) => eprintln!("recorder: no slot, push deferred: {e}"),
                    }
                    last_push_hour = hour;
                }
                if stopping {
                    RECORDER_DONE.store(true, std::sync::atomic::Ordering::SeqCst);
                    eprintln!("recorder: stopped");
                    break;
                }
                // Sleep in short slices so a shutdown is noticed promptly.
                let deadline =
                    std::time::Instant::now() + std::time::Duration::from_millis(interval);
                while std::time::Instant::now() < deadline
                    && !STOPPING.load(std::sync::atomic::Ordering::SeqCst)
                {
                    std::thread::sleep(std::time::Duration::from_millis(250));
                }
            }
        })
        .expect("spawn recorder thread");
}

fn scope_for(rpc: Rpc, archive: Option<String>) -> Scope {
    // A relayed scope talks to whatever chain the reader's page is pointed at,
    // usually a validator that started minutes ago. The mainnet record store,
    // the account-changes stream and an archival endpoint all describe a
    // different chain, so none of them is attached: feeding mainnet account
    // versions into a localnet replay would be worse than having no history.
    let url = match rpc {
        Rpc::Relay(id) => {
            return match relay::client_for(&id) {
                Some(client) => Scope::from_client(client),
                // The page vanished between the check and here; a scope on the
                // default endpoint is wrong, so fail every call instead.
                None => Scope::new(String::new()),
            };
        }
        Rpc::Url(url) => url,
    };
    // Free historical reconstruction replays old transactions per drifting
    // account; opt in with a replay budget once the RPC behind the instance can
    // take the extra calls. `0` (default) keeps the current-state tier.
    let budget = std::env::var("SVMSCOPE_RECONSTRUCT_BUDGET")
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .unwrap_or(0);
    let scope = Scope::new(url).with_reconstruction_budget(budget);
    let scope = match svmscope::history::HistoryStream::from_env() {
        Some(stream) => scope.with_history_stream(stream),
        None => scope,
    };
    let scope = match RECORDS.as_ref() {
        Some(store) => {
            let dynamic: std::sync::Arc<dyn svmscope::records::StateStore> =
                std::sync::Arc::<svmscope::records::LogStore>::clone(store);
            scope.with_records(dynamic)
        }
        None => scope,
    };
    match archive {
        Some(a) => scope.with_archive(a),
        None => scope,
    }
}

/// Resolve the archival endpoint for a request. A caller-supplied `archive`
/// is honoured only under the same opt-in and SSRF vetting as a caller `rpc`
/// (it is a URL to an arbitrary host, so the rules are identical); otherwise
/// the operator's `SVMSCOPE_ARCHIVE_URL`, if set; otherwise none.
fn archive_for(caller: Option<&str>) -> Option<String> {
    if custom_rpc_allowed() {
        if let Some(safe) = caller.and_then(vet_custom_rpc) {
            return Some(safe);
        }
    }
    std::env::var("SVMSCOPE_ARCHIVE_URL")
        .ok()
        .map(|a| a.trim().to_string())
        .filter(|a| !a.is_empty())
}

/// Both per-request endpoints at once: the RPC (see [`rpc_for`]) and the
/// archive (see [`archive_for`]).
/// Where a request's RPC calls go: an endpoint this process fetches from, or a
/// connected page that fetches on its behalf. The second is how a validator on
/// the reader's own machine is reachable at all — see [`relay`].
#[derive(Clone, Debug, PartialEq)]
enum Rpc {
    Url(String),
    Relay(String),
}

impl std::fmt::Display for Rpc {
    /// Used in answer-cache keys, so each target must read differently. Two
    /// pages relaying are two different chains as far as this process knows,
    /// and neither is the public endpoint.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Rpc::Url(u) => write!(f, "{u}"),
            Rpc::Relay(id) => write!(f, "relay:{id}"),
        }
    }
}

fn endpoints_for(
    cluster: Option<&str>,
    rpc: Option<&str>,
    archive: Option<&str>,
    relay: Option<&str>,
) -> Result<(Rpc, Option<String>), (StatusCode, String)> {
    Ok((rpc_for(cluster, rpc, relay)?, archive_for(archive)))
}

/// What to tell a caller who asked for an endpoint this engine will not serve.
const NO_LOCAL_RPC: &str = "this engine cannot reach a validator on your \
machine by itself. Open the page's RPC relay and it will make those calls for \
you from your own browser, which is where your validator is. Failing that, run \
svmscope locally with SVMSCOPE_ALLOW_CUSTOM_RPC=1.";

/// The RPC endpoint for one request, or why it cannot be served.
///
/// Nothing here quietly substitutes a different chain. Answering a localnet
/// request with mainnet data looks exactly like success, which is how someone
/// ends up debugging their own program against somebody else's chain. A cluster
/// this engine will not serve is an error the caller can read.
fn rpc_for(
    cluster: Option<&str>,
    rpc: Option<&str>,
    relay: Option<&str>,
) -> Result<Rpc, (StatusCode, String)> {
    // A page that has offered to make the calls wins over everything else: it
    // is the only way to reach a validator on the reader's machine, and it is
    // also the safest, since this process then connects to nothing at all.
    if let Some(id) = relay.map(str::trim).filter(|s| !s.is_empty()) {
        if !relay::is_open(id) {
            return Err((
                StatusCode::BAD_REQUEST,
                "that RPC relay is not connected — reload the page to open a new one".to_string(),
            ));
        }
        return Ok(Rpc::Relay(id.to_string()));
    }

    // A caller-supplied RPC is only trusted under the operator's opt-in, and
    // then only if it passes the SSRF check. Neither rule is relaxed here.
    let allow = custom_rpc_allowed();
    if let Some(u) = rpc.map(str::trim).filter(|u| !u.is_empty()) {
        if !allow {
            return Err((StatusCode::BAD_REQUEST, NO_LOCAL_RPC.to_string()));
        }
        return vet_custom_rpc(u).map(Rpc::Url).ok_or_else(|| {
            (
                StatusCode::BAD_REQUEST,
                "that RPC endpoint was refused: it must be http(s) and must not \
                 resolve to a loopback, private, or link-local address."
                    .to_string(),
            )
        });
    }

    let Some(name) = cluster.map(str::trim).filter(|c| !c.is_empty()) else {
        return Ok(Rpc::Url(rpc_url()));
    };
    // A URL-shaped cluster is never honored, by anyone, so `cluster` cannot be
    // an SSRF vector: it selects among names, and the endpoint each name maps to
    // comes from this process's own configuration.
    if localnet_alias(name) {
        if !allow {
            return Err((StatusCode::BAD_REQUEST, NO_LOCAL_RPC.to_string()));
        }
    } else if !public_cluster_ok(name) {
        return Err((
            StatusCode::BAD_REQUEST,
            format!("unknown cluster '{name}' (expected mainnet, devnet, testnet, or localnet)"),
        ));
    }
    if let Some(u) = cluster_env_rpc(Some(name)) {
        return Ok(Rpc::Url(u));
    }
    svmscope::resolve_rpc_url(Some(name), None, &rpc_url())
        .map(Rpc::Url)
        .map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))
}

/// POST body for /simulate.
#[derive(Deserialize)]
struct SimRequest {
    signature: String,
    /// Replay at this slot (the transaction's own, or any other) before the
    /// mutations apply: the world as of then, then the what-if on top.
    #[serde(default)]
    slot: Option<u64>,
    mutations: Vec<MutationInput>,
    /// Optional clock warp — test time-gated logic without waiting.
    #[serde(default)]
    time_travel: TimeTravel,
    /// Optional runtime feature-gate toggles — replay as if a feature were (in)active.
    #[serde(default)]
    features: Vec<svmscope::spec::FeatureInput>,
    #[serde(default)]
    cluster: Option<String>,
    #[serde(default)]
    rpc: Option<String>,
    #[serde(default)]
    relay: Option<String>,
    /// Optional archival RPC (one that honours a historical `slot` on
    /// `getAccountInfo`, e.g. Alchemy's Account Archive) for exact state at
    /// the transaction's slot. Vetted like `rpc`; the caller's key stays in
    /// the caller's URL and is never stored or logged.
    #[serde(default)]
    archive: Option<String>,
}

/// POST body for /trace — the step debugger. Exactly one of `signature`
/// (a landed transaction) or `transaction` (base64, unsigned) is required.
#[derive(Deserialize)]
struct TraceRequest {
    #[serde(default)]
    signature: Option<String>,
    #[serde(default)]
    transaction: Option<String>,
    #[serde(default)]
    mutations: Vec<MutationInput>,
    #[serde(default)]
    time_travel: TimeTravel,
    #[serde(default)]
    features: Vec<svmscope::spec::FeatureInput>,
    #[serde(default)]
    cluster: Option<String>,
    #[serde(default)]
    rpc: Option<String>,
    #[serde(default)]
    relay: Option<String>,
    /// Optional archival RPC (one that honours a historical `slot` on
    /// `getAccountInfo`, e.g. Alchemy's Account Archive) for exact state at
    /// the transaction's slot. Vetted like `rpc`; the caller's key stays in
    /// the caller's URL and is never stored or logged.
    #[serde(default)]
    archive: Option<String>,
    /// `at_slot` or `now`: pin the state tier (a mutated re-run must use the
    /// tier its base trace used). Omit to let the server choose: at-slot
    /// first, current state if that diverges from the on-chain outcome.
    #[serde(default)]
    tier: Option<String>,
}

/// Serve the static frontend page.
async fn index() -> Html<&'static str> {
    Html(include_str!("../../static/index.html"))
}

/// Map a library error onto an HTTP status + message. Not-found inputs are 404,
/// upstream RPC trouble is 502, everything else the caller can fix is 400.
fn lib_err(e: svmscope::Error) -> (StatusCode, String) {
    use svmscope::Error as E;
    match &e {
        E::TransactionNotFound(_) | E::NoSignatures(_) | E::AccountNotFound(_) => {
            (StatusCode::NOT_FOUND, e.to_string())
        }
        // An RPC error's Display can echo the upstream request, and a paid RPC
        // URL commonly carries an ?api-key=… secret. Never pass that back to an
        // anonymous caller — return a generic gateway message and keep the detail
        // server-side only.
        E::Rpc(_) | E::MalformedRpcResponse(_) => {
            eprintln!("upstream RPC error: {e}");
            (StatusCode::BAD_GATEWAY, "upstream RPC error".to_string())
        }
        _ => (StatusCode::BAD_REQUEST, e.to_string()),
    }
}

/// Days a historical replay reaches back (`SVMSCOPE_REPLAY_WINDOW_DAYS`,
/// default 30): the promise the recorder keeps, so slots older than it are
/// refused up front instead of answered from whatever a third party still
/// holds.
fn replay_window_days() -> u64 {
    std::env::var("SVMSCOPE_REPLAY_WINDOW_DAYS")
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(30)
}

/// `Err` with a plain message when `slot` landed more than the window ago,
/// by block time (a skipped slot borrows the next one's). Slots whose time
/// cannot be read pass: the engine labels what it cannot prove anyway.
fn check_replay_window(scope: &Scope, slot: u64) -> Result<(), svmscope::Error> {
    let days = replay_window_days();
    let block_time =
        (slot..slot.saturating_add(8)).find_map(|s| scope.client().get_block_time(s).ok());
    let Some(t) = block_time else {
        return Ok(());
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(t);
    // Compare in seconds: flooring to whole days let a target up to one day
    // past the cutoff through, so a 30-day window accepted nearly 31.
    let age_secs = now.saturating_sub(t);
    if age_secs > (days as i64).saturating_mul(86_400) {
        let age_days = age_secs as f64 / 86_400.0;
        return Err(svmscope::Error::InvalidSpec(format!(
            "slot {slot} is {age_days:.1} days old; replays cover the last {days} days"
        )));
    }
    Ok(())
}

/// The replay a what-if builds on: today's state with balances rewound
/// (no slot), or the world as of `slot` (inside the window).
fn replay_for(
    scope: &Scope,
    signature: &str,
    slot: Option<u64>,
) -> Result<svmscope::Replay, svmscope::Error> {
    let Some(slot) = slot else {
        return scope.replay(signature);
    };
    if slot == 0 {
        return Err(svmscope::Error::Fixture("slot must be positive".into()));
    }
    check_replay_window(scope, slot)?;
    let landed = scope
        .landed_slot(signature)?
        .ok_or_else(|| svmscope::Error::TransactionNotFound(signature.to_string()))?;
    if slot == landed {
        scope.replay_at_slot(signature)
    } else {
        scope.replay_at(signature, slot)
    }
}

/// `?time=` for `/slot_at`: unix seconds, or an RFC 3339 / ISO 8601 date.
#[derive(Deserialize)]
struct SlotAtQuery {
    time: String,
    cluster: Option<String>,
    rpc: Option<String>,
    relay: Option<String>,
}

/// GET /slot_at?time=… — the slot nearest a moment in time: estimated from
/// the tip at 400 ms per slot, then corrected against real block times a
/// few rounds until within a couple of seconds. Skipped slots borrow the
/// next block's time.
async fn slot_at_handler(
    Query(q): Query<SlotAtQuery>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let target = parse_time(&q.time).ok_or_else(|| {
        (
            StatusCode::BAD_REQUEST,
            "time must be unix seconds or an ISO 8601 date".to_string(),
        )
    })?;
    let (url, archive) = endpoints_for(
        q.cluster.as_deref(),
        q.rpc.as_deref(),
        None,
        q.relay.as_deref(),
    )?;
    let out = tokio::task::spawn_blocking(move || -> Result<serde_json::Value, svmscope::Error> {
        let scope = scope_for(url, archive);
        let client = scope.client();
        let tip = client
            .get_slot()
            .map_err(|_| svmscope::Error::Fixture("cannot read the current slot".into()))?;
        let block_time_of = |s: u64| {
            (s..s.saturating_add(8)).find_map(|x| client.get_block_time(x).ok().map(|t| (x, t)))
        };
        let (_, tip_time) = block_time_of(tip.saturating_sub(32))
            .ok_or_else(|| svmscope::Error::Fixture("cannot read the tip's block time".into()))?;
        if target > tip_time + 60 {
            return Err(svmscope::Error::Fixture(format!(
                "{} is in the future",
                q.time
            )));
        }
        let mut slot = tip
            .saturating_sub(32)
            .saturating_sub(((tip_time - target).max(0) as f64 / 0.4) as u64);
        // Keep the closest block seen, not merely the last one tried, and
        // walk until it is inside the tolerance. Returning whatever the sixth
        // estimate happened to hit is how a lookup lands minutes away.
        const TOLERANCE_SECS: i64 = 2;
        let mut best: Option<(u64, i64)> = None;
        for _ in 0..12 {
            let Some((s, t)) = block_time_of(slot.max(1)) else {
                break;
            };
            if best.is_none_or(|(_, bt)| (target - t).abs() < (target - bt).abs()) {
                best = Some((s, t));
            }
            let delta = target - t;
            if delta.abs() <= TOLERANCE_SECS {
                break;
            }
            let step = (delta as f64 / 0.4) as i64;
            let step = if step == 0 {
                if delta > 0 {
                    1
                } else {
                    -1
                }
            } else {
                step
            };
            slot = (s as i64 + step).max(1) as u64;
        }
        let (s, t) =
            best.ok_or_else(|| svmscope::Error::Fixture("no block near that time".into()))?;
        let off = (target - t).abs();
        if off > 60 {
            return Err(svmscope::Error::Fixture(format!(
                "no block within a minute of that time; the nearest found is slot {s}, {off} seconds away"
            )));
        }
        Ok(json!({ "slot": s, "block_time": t, "requested": target, "off_by_secs": target - t }))
    })
    .await
    .map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("task error: {e}"),
        )
    })?;
    out.map(Json).map_err(lib_err)
}

/// Unix seconds from `text`: digits as-is, else an ISO 8601 date with an
/// optional time, treated as UTC when no offset is given.
fn parse_time(text: &str) -> Option<i64> {
    let t = text.trim();
    if let Ok(n) = t.parse::<i64>() {
        return Some(if n > 1_000_000_000_000 { n / 1000 } else { n });
    }
    // YYYY-MM-DD[THH:MM[:SS]][Z|±HH:MM]
    let (date, rest) = t.split_at(t.find('T').unwrap_or(t.len()));
    let mut parts = date.split('-');
    let y: i64 = parts.next()?.parse().ok()?;
    let m: i64 = parts.next()?.parse().ok()?;
    let d: i64 = parts.next()?.parse().ok()?;
    // Reject a date that does not exist rather than letting the day-count
    // arithmetic roll it forward: February 30 is an error, not March 2.
    if !(1..=12).contains(&m) || d < 1 {
        return None;
    }
    let leap = (y % 4 == 0 && y % 100 != 0) || y % 400 == 0;
    let days_in_month = match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if leap => 29,
        2 => 28,
        _ => return None,
    };
    if d > days_in_month {
        return None;
    }
    let rest = rest.trim_start_matches('T');
    let (clock, offset) = match rest.find(['Z', '+', '-']) {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, ""),
    };
    // Fractional seconds are fine, and ignored.
    let clock = clock.split('.').next().unwrap_or(clock);
    let mut hms = clock.split(':');
    let hh: i64 = hms
        .next()
        .filter(|x| !x.is_empty())
        .map(|x| x.parse().ok())
        .unwrap_or(Some(0))?;
    let mm: i64 = hms.next().map(|x| x.parse().ok()).unwrap_or(Some(0))?;
    let ss: i64 = hms.next().map(|x| x.parse().ok()).unwrap_or(Some(0))?;
    // A clock reading outside its range is a typo, not a later time: 25:00
    // must be an error rather than one in the morning.
    if !(0..24).contains(&hh) || !(0..60).contains(&mm) || !(0..=60).contains(&ss) {
        return None;
    }
    // Days from civil (Howard Hinnant).
    let (y2, m2) = if m <= 2 { (y - 1, m + 9) } else { (y, m - 3) };
    let era = y2.div_euclid(400);
    let yoe = y2 - era * 400;
    let doy = (153 * m2 + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    let mut secs = days * 86_400 + hh * 3600 + mm * 60 + ss;
    if !offset.is_empty() && offset != "Z" {
        let sign = if offset.starts_with('-') { -1 } else { 1 };
        let mut o = offset[1..].split(':');
        let oh: i64 = o.next()?.parse().ok()?;
        let om: i64 = o.next().map(|x| x.parse().ok()).unwrap_or(Some(0))?;
        secs -= sign * (oh * 3600 + om * 60);
    }
    Some(secs)
}

/// IDLs the caller supplies for their own programs, keyed by program id.
/// Most Solana programs never publish an IDL on chain; without this a replay
/// shows their accounts as raw bytes. Sent as a JSON body so a full
/// `target/idl/<program>.json` does not have to fit in a query string.
#[derive(Deserialize, Default)]
struct IdlBody {
    #[serde(default)]
    idls: std::collections::HashMap<String, serde_json::Value>,
}

impl IdlBody {
    /// A short, stable fingerprint of the supplied IDLs, so a cached answer
    /// built without them is never served to a request that sent them.
    fn fingerprint(&self) -> String {
        if self.idls.is_empty() {
            return String::new();
        }
        use std::hash::{Hash, Hasher};
        let mut names: Vec<&String> = self.idls.keys().collect();
        names.sort();
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        for n in names {
            n.hash(&mut hasher);
            if let Ok(v) = serde_json::to_vec(&self.idls[n]) {
                v.hash(&mut hasher);
            }
        }
        format!("{:016x}", hasher.finish())
    }

    /// Register every supplied IDL on a scope before it analyses anything.
    fn apply(&self, scope: &Scope) {
        for (program, idl) in &self.idls {
            scope.add_idl(program.clone(), idl.clone());
        }
    }
}

/// GET /analyze/:signature — decode + replay a transaction, return JSON.
async fn analyze_handler(
    Path(signature): Path<String>,
    Query(q): Query<ClusterQuery>,
) -> Result<Json<Analysis>, (StatusCode, String)> {
    analyze_inner(signature, q, IdlBody::default()).await
}

/// `POST /analyze/{signature}` — the same analysis, with IDLs for programs
/// that never published one on chain.
async fn analyze_post_handler(
    Path(signature): Path<String>,
    Query(q): Query<ClusterQuery>,
    Json(body): Json<IdlBody>,
) -> Result<Json<Analysis>, (StatusCode, String)> {
    analyze_inner(signature, q, body).await
}

async fn analyze_inner(
    signature: String,
    q: ClusterQuery,
    body: IdlBody,
) -> Result<Json<Analysis>, (StatusCode, String)> {
    let (url, archive) = endpoints_for(
        q.cluster.as_deref(),
        q.rpc.as_deref(),
        q.archive.as_deref(),
        q.relay.as_deref(),
    )?;
    // `analyze` does blocking I/O (RPC) and heavy CPU work (replay), so run it on
    // the blocking thread pool instead of stalling the async runtime.
    let result = tokio::task::spawn_blocking(move || {
        let scope = scope_for(url, archive);
        body.apply(&scope);
        scope.analyze(&signature)
    })
    .await
    .map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("task error: {e}"),
        )
    })?;

    match result {
        Ok(analysis) => Ok(Json(analysis)),
        Err(e) => Err(lib_err(e)),
    }
}

/// Per-request work caps for the public server. Each scenario/mutation is a full
/// LiteSVM replay on a single blocking thread, so an unbounded batch is a CPU/
/// thread-pool DoS regardless of the 2MB body limit. Generous for real use.
const MAX_MUTATIONS_PER_REQUEST: usize = 256;
const MAX_SCENARIOS_PER_REQUEST: usize = 64;

fn cap(count: usize, limit: usize, what: &str) -> Result<(), (StatusCode, String)> {
    if count > limit {
        return Err((
            StatusCode::BAD_REQUEST,
            format!("too many {what}: {count} (limit {limit})"),
        ));
    }
    Ok(())
}

/// POST /simulate — apply what-if mutations and return the mutated replay result.
async fn simulate_handler(
    Json(req): Json<SimRequest>,
) -> Result<Json<ReplayResult>, (StatusCode, String)> {
    cap(req.mutations.len(), MAX_MUTATIONS_PER_REQUEST, "mutations")?;
    let mutations: Vec<Mutation> = req
        .mutations
        .into_iter()
        .map(MutationInput::into_mutation)
        .collect::<Result<_, _>>()
        .map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?;

    let features = svmscope::spec::feature_toggles(req.features)
        .map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?;

    let (url, archive) = endpoints_for(
        req.cluster.as_deref(),
        req.rpc.as_deref(),
        req.archive.as_deref(),
        req.relay.as_deref(),
    )?;
    let result = tokio::task::spawn_blocking(move || -> Result<ReplayResult, svmscope::Error> {
        let scope = scope_for(url, archive);
        let mut replay = replay_for(&scope, &req.signature, req.slot)?;
        replay.set_time_travel(req.time_travel);
        replay.set_features(features);
        Ok(replay.simulate(&mutations)?.result)
    })
    .await
    .map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("task error: {e}"),
        )
    })?;

    match result {
        Ok(replay) => Ok(Json(replay)),
        Err(e) => Err(lib_err(e)),
    }
}

/// POST /simulate_suite — run a suite of test scenarios, return per-scenario pass/fail.
async fn suite_handler(
    Json(req): Json<SuiteRequest>,
) -> Result<Json<Vec<ScenarioOutcome>>, (StatusCode, String)> {
    // Fixture-backed suites are a CLI feature (`svmscope test suite.json`) — the
    // server can't read a file on the caller's machine, so say so instead of
    // silently ignoring the field.
    if req.fixture.is_some() {
        return Err((
            StatusCode::BAD_REQUEST,
            "fixture suites run locally: `svmscope test suite.json`. The API needs a `signature`."
                .to_string(),
        ));
    }
    let signature = req
        .signature
        .clone()
        .ok_or((StatusCode::BAD_REQUEST, "signature is required".to_string()))?;
    cap(req.scenarios.len(), MAX_SCENARIOS_PER_REQUEST, "scenarios")?;
    let total_mutations: usize = req.scenarios.iter().map(|s| s.mutations.len()).sum();
    cap(total_mutations, MAX_MUTATIONS_PER_REQUEST, "mutations")?;
    let (url, archive) = endpoints_for(
        req.cluster.as_deref(),
        req.rpc.as_deref(),
        req.archive.as_deref(),
        req.relay.as_deref(),
    )?;
    let scenarios = req
        .scenarios
        .into_iter()
        .map(|s| s.into_scenario())
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?;
    let features = svmscope::spec::feature_toggles(req.features)
        .map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?;

    let result =
        tokio::task::spawn_blocking(move || -> Result<Vec<ScenarioOutcome>, svmscope::Error> {
            let scope = scope_for(url, archive);
            let mut replay = replay_for(&scope, &signature, req.slot)?;
            replay.set_time_travel(req.time_travel);
            replay.set_features(features);
            replay.run_suite(&scenarios)
        })
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("task error: {e}"),
            )
        })?;

    match result {
        Ok(outcomes) => Ok(Json(outcomes)),
        Err(e) => Err(lib_err(e)),
    }
}

/// POST body for /preflight.
#[derive(Deserialize)]
struct PreflightRequest {
    /// base64 wire bytes of an (unsigned) VersionedTransaction.
    transaction: String,
    #[serde(default)]
    mutations: Vec<MutationInput>,
    /// Optional clock warp — test time-gated logic without waiting.
    #[serde(default)]
    time_travel: TimeTravel,
    /// Optional runtime feature-gate toggles.
    #[serde(default)]
    features: Vec<svmscope::spec::FeatureInput>,
    #[serde(default)]
    cluster: Option<String>,
    #[serde(default)]
    rpc: Option<String>,
    #[serde(default)]
    relay: Option<String>,
    /// Optional archival RPC (one that honours a historical `slot` on
    /// `getAccountInfo`, e.g. Alchemy's Account Archive) for exact state at
    /// the transaction's slot. Vetted like `rpc`; the caller's key stays in
    /// the caller's URL and is never stored or logged.
    #[serde(default)]
    archive: Option<String>,
}

/// POST /preflight — simulate an unsigned transaction against current state before
/// it's sent. The pre-flight primitive a wallet/bot calls before signing.
async fn preflight_handler(
    Json(req): Json<PreflightRequest>,
) -> Result<Json<ReplayResult>, (StatusCode, String)> {
    cap(req.mutations.len(), MAX_MUTATIONS_PER_REQUEST, "mutations")?;
    let mutations: Vec<Mutation> = req
        .mutations
        .into_iter()
        .map(MutationInput::into_mutation)
        .collect::<Result<_, _>>()
        .map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?;

    let (url, archive) = endpoints_for(
        req.cluster.as_deref(),
        req.rpc.as_deref(),
        req.archive.as_deref(),
        req.relay.as_deref(),
    )?;
    let result = tokio::task::spawn_blocking(move || -> Result<ReplayResult, svmscope::Error> {
        let replay = scope_for(url, archive).preflight(&req.transaction)?;
        Ok(replay.simulate(&mutations)?.result)
    })
    .await
    .map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("task error: {e}"),
        )
    })?;

    match result {
        Ok(r) => Ok(Json(r)),
        Err(e) => Err(lib_err(e)),
    }
}

/// GET /account/:address — explorer-style overview of an account or program.
async fn account_handler(
    Path(address): Path<String>,
    Query(q): Query<ClusterQuery>,
) -> Result<Json<svmscope::AccountOverview>, (StatusCode, String)> {
    let (url, archive) = endpoints_for(
        q.cluster.as_deref(),
        q.rpc.as_deref(),
        q.archive.as_deref(),
        q.relay.as_deref(),
    )?;
    let result = tokio::task::spawn_blocking(move || scope_for(url, archive).account(&address))
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("task error: {e}"),
            )
        })?;

    match result {
        Ok(ov) => Ok(Json(ov)),
        Err(e) => Err(lib_err(e)),
    }
}

/// GET /signatures/:address — recent transactions for an account/program (explorer-style).
async fn signatures_handler(
    Path(address): Path<String>,
    Query(q): Query<ClusterQuery>,
) -> Result<Json<Vec<svmscope::SigInfo>>, (StatusCode, String)> {
    let (url, archive) = endpoints_for(
        q.cluster.as_deref(),
        q.rpc.as_deref(),
        q.archive.as_deref(),
        q.relay.as_deref(),
    )?;
    let result =
        tokio::task::spawn_blocking(move || scope_for(url, archive).signatures(&address, 25))
            .await
            .map_err(|e| {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("task error: {e}"),
                )
            })?;

    match result {
        Ok(sigs) => Ok(Json(sigs)),
        Err(e) => Err(lib_err(e)),
    }
}

/// POST /preflight_report — simulate an unsigned tx and return the full developer
/// report: outcome, human-readable failure reason, and the account diff.
async fn preflight_report_handler(
    Json(req): Json<PreflightRequest>,
) -> Result<Json<svmscope::SimulationReport>, (StatusCode, String)> {
    cap(req.mutations.len(), MAX_MUTATIONS_PER_REQUEST, "mutations")?;
    let mutations: Vec<Mutation> = req
        .mutations
        .into_iter()
        .map(MutationInput::into_mutation)
        .collect::<Result<_, _>>()
        .map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?;

    let features = svmscope::spec::feature_toggles(req.features)
        .map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?;
    let (url, archive) = endpoints_for(
        req.cluster.as_deref(),
        req.rpc.as_deref(),
        req.archive.as_deref(),
        req.relay.as_deref(),
    )?;
    let tt = req.time_travel.clone();
    let result = tokio::task::spawn_blocking(
        move || -> Result<svmscope::SimulationReport, svmscope::Error> {
            let scope = scope_for(url, archive);
            let tx = Scope::parse_unsigned_b64(&req.transaction)?;
            // Decode the pre-sign overview (size, fees, named instructions,
            // actions/warnings) before simulating — it explains the tx even
            // when the simulation itself fails.
            let mut overview = scope.preflight_overview(&tx);
            let mut replay = scope.preflight_tx(tx)?;
            replay.set_time_travel(tt);
            replay.set_features(features);
            let mut report = replay.simulate(&mutations)?.into_report();
            // The per-program compute breakdown needs the simulation's logs, so
            // fill it in now that the replay has run.
            overview.compute =
                svmscope::compute_breakdown(&report.replay.logs, report.replay.compute_units);
            report.preflight = Some(overview);
            Ok(report)
        },
    )
    .await
    .map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("task error: {e}"),
        )
    })?;

    result.map(Json).map_err(lib_err)
}

/// POST /replay_report — replay a landed tx (optionally mutated) with explanation
/// and account diff.
async fn replay_report_handler(
    Json(req): Json<SimRequest>,
) -> Result<Json<svmscope::SimulationReport>, (StatusCode, String)> {
    cap(req.mutations.len(), MAX_MUTATIONS_PER_REQUEST, "mutations")?;
    let mutations: Vec<Mutation> = req
        .mutations
        .into_iter()
        .map(MutationInput::into_mutation)
        .collect::<Result<_, _>>()
        .map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?;

    let features = svmscope::spec::feature_toggles(req.features)
        .map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?;
    let (url, archive) = endpoints_for(
        req.cluster.as_deref(),
        req.rpc.as_deref(),
        req.archive.as_deref(),
        req.relay.as_deref(),
    )?;
    let tt = req.time_travel.clone();
    let result = tokio::task::spawn_blocking(
        move || -> Result<svmscope::SimulationReport, svmscope::Error> {
            let mut replay = scope_for(url, archive).replay(&req.signature)?;
            replay.set_time_travel(tt);
            replay.set_features(features);
            Ok(replay.simulate(&mutations)?.into_report())
        },
    )
    .await
    .map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("task error: {e}"),
        )
    })?;

    result.map(Json).map_err(lib_err)
}

/// POST /trace — unroll a transaction into steps: every instruction and CPI with
/// what it changed, and the failing step pinpointed. Mutations apply first.
/// POST body for /profile — the compute profiler. `symbols` names a program's
/// functions from the unstripped ELF of the same build (base64 of the
/// `.debug` file `cargo build-sbf --debug` writes).
#[derive(Deserialize)]
struct ProfileRequest {
    signature: String,
    #[serde(default)]
    mutations: Vec<MutationInput>,
    #[serde(default)]
    time_travel: TimeTravel,
    #[serde(default)]
    features: Vec<svmscope::spec::FeatureInput>,
    #[serde(default)]
    symbols: Vec<SymbolInput>,
    #[serde(default)]
    cluster: Option<String>,
    #[serde(default)]
    rpc: Option<String>,
    #[serde(default)]
    relay: Option<String>,
    /// Optional archival RPC (one that honours a historical `slot` on
    /// `getAccountInfo`, e.g. Alchemy's Account Archive) for exact state at
    /// the transaction's slot. Vetted like `rpc`; the caller's key stays in
    /// the caller's URL and is never stored or logged.
    #[serde(default)]
    archive: Option<String>,
    /// `at_slot` (default) or `now`: which state to profile against.
    #[serde(default)]
    tier: Option<String>,
}

#[derive(Deserialize)]
struct SymbolInput {
    program: String,
    /// base64 of the `.debug` file (symbol table) of a build of the program.
    elf_b64: String,
    /// base64 of that build's `.so`. With it, functions are matched to the
    /// on-chain code by shape, so the build need not be the deployed one.
    #[serde(default)]
    so_b64: Option<String>,
}

#[derive(Serialize)]
struct ProfileResponse {
    result: svmscope::ReplayResult,
    profile: svmscope::profile::Profile,
    /// Per program: how many functions the uploaded symbols named.
    symbolized: Vec<(String, usize)>,
}

const MAX_SYMBOL_FILES: usize = 8;
const MAX_SYMBOL_BYTES: usize = 32 * 1024 * 1024;
/// Folded stacks kept per frame in the response (largest first). A 300k-CU
/// swap can produce tens of thousands of distinct stacks; the top slice is
/// what a flamegraph can show.
const MAX_STACKS_PER_FRAME: usize = 1500;

fn trim_profile(profile: &mut svmscope::profile::Profile) {
    for f in &mut profile.frames {
        f.stacks.truncate(MAX_STACKS_PER_FRAME);
        f.functions.truncate(400);
    }
}

fn run_profile(
    scope: Scope,
    signature: String,
    mutations: Vec<Mutation>,
    tt: TimeTravel,
    features: Vec<svmscope::FeatureToggle>,
    symbols: Vec<(String, Vec<u8>, Option<Vec<u8>>)>,
    tier: Option<String>,
) -> Result<ProfileResponse, svmscope::Error> {
    let mut replay = if tier.as_deref() == Some("now") {
        scope.replay(&signature)?
    } else {
        scope.replay_at_slot(&signature)?
    };
    replay.set_time_travel(tt);
    replay.set_features(features);
    let (result, mut profile) = replay.profile(&mutations)?;
    // Instruction names on every frame, decoded exactly as the Analyze tree does.
    if let Ok(analysis) = scope.analyze(&signature) {
        profile.attach_names(&analysis.cpi_tree, &result.logs);
    }
    let mut symbolized = Vec::new();
    for (program, debug, so) in symbols {
        let n = match so {
            Some(so) => {
                let r = profile.symbolize_from_build(&program, &so, &debug)?;
                r.exact + r.by_opcodes + r.by_similarity
            }
            None => profile.symbolize(&program, &debug)?,
        };
        symbolized.push((program, n));
    }
    trim_profile(&mut profile);
    Ok(ProfileResponse {
        result,
        profile,
        symbolized,
    })
}

async fn profile_handler(
    Json(req): Json<ProfileRequest>,
) -> Result<Json<ProfileResponse>, (StatusCode, String)> {
    use base64::Engine;
    cap(req.mutations.len(), MAX_MUTATIONS_PER_REQUEST, "mutations")?;
    cap(req.symbols.len(), MAX_SYMBOL_FILES, "symbols")?;
    let mutations: Vec<Mutation> = req
        .mutations
        .into_iter()
        .map(MutationInput::into_mutation)
        .collect::<Result<_, _>>()
        .map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?;
    let features = svmscope::spec::feature_toggles(req.features)
        .map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?;
    let mut symbols = Vec::new();
    for s in req.symbols {
        let decode = |b64: &str| {
            base64::engine::general_purpose::STANDARD
                .decode(b64.as_bytes())
                .map_err(|e| {
                    (
                        StatusCode::BAD_REQUEST,
                        format!("symbols for {}: {e}", s.program),
                    )
                })
                .and_then(|bytes| {
                    if bytes.len() > MAX_SYMBOL_BYTES {
                        Err((
                            StatusCode::PAYLOAD_TOO_LARGE,
                            "symbol file too large".into(),
                        ))
                    } else {
                        Ok(bytes)
                    }
                })
        };
        let debug = decode(&s.elf_b64)?;
        let so = match &s.so_b64 {
            Some(b) => Some(decode(b)?),
            None => None,
        };
        symbols.push((s.program, debug, so));
    }
    let (url, archive) = endpoints_for(
        req.cluster.as_deref(),
        req.rpc.as_deref(),
        req.archive.as_deref(),
        req.relay.as_deref(),
    )?;
    let tt = req.time_travel.clone();
    let tier = req.tier.clone();
    let sig = req.signature;
    tokio::task::spawn_blocking(move || {
        run_profile(
            scope_for(url, archive),
            sig,
            mutations,
            tt,
            features,
            symbols,
            tier,
        )
    })
    .await
    .map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("task error: {e}"),
        )
    })?
    .map(Json)
    .map_err(lib_err)
}

/// GET /profile/{signature} — the as-it-happened profile, no symbols, cacheable.
async fn profile_get_handler(
    Path(signature): Path<String>,
    Query(q): Query<ClusterQuery>,
) -> Result<Json<ProfileResponse>, (StatusCode, String)> {
    let (url, archive) = endpoints_for(
        q.cluster.as_deref(),
        q.rpc.as_deref(),
        q.archive.as_deref(),
        q.relay.as_deref(),
    )?;
    tokio::task::spawn_blocking(move || {
        run_profile(
            scope_for(url, archive),
            signature,
            vec![],
            TimeTravel::default(),
            vec![],
            vec![],
            None,
        )
    })
    .await
    .map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("task error: {e}"),
        )
    })?
    .map(Json)
    .map_err(lib_err)
}

async fn trace_handler(
    Json(req): Json<TraceRequest>,
) -> Result<Json<svmscope::Trace>, (StatusCode, String)> {
    cap(req.mutations.len(), MAX_MUTATIONS_PER_REQUEST, "mutations")?;
    if req.signature.is_none() == req.transaction.is_none() {
        return Err((
            StatusCode::BAD_REQUEST,
            "provide exactly one of `signature` or `transaction`".into(),
        ));
    }

    let mutations: Vec<Mutation> = req
        .mutations
        .into_iter()
        .map(MutationInput::into_mutation)
        .collect::<Result<_, _>>()
        .map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?;

    let features = svmscope::spec::feature_toggles(req.features)
        .map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?;
    let (url, archive) = endpoints_for(
        req.cluster.as_deref(),
        req.rpc.as_deref(),
        req.archive.as_deref(),
        req.relay.as_deref(),
    )?;
    let tt = req.time_travel.clone();
    let pinned = req.tier.clone();

    let result =
        tokio::task::spawn_blocking(move || -> Result<svmscope::Trace, svmscope::Error> {
            let scope = scope_for(url, archive);
            let (sig, b64) = (req.signature, req.transaction);
            let Some(sig) = sig else {
                let mut replay = scope.preflight(&b64.expect("validated above"))?;
                replay.set_time_travel(tt);
                replay.set_features(features);
                return replay.trace(&mutations);
            };
            trace_with_world(&scope, &sig, &mutations, &tt, &features, pinned.as_deref())
        })
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("task error: {e}"),
            )
        })?;

    result.map(Json).map_err(lib_err)
}

/// GET /trace/{signature} — the plain trace, cacheable, for share links.
/// Traces of landed transactions, kept for hours so a shared `/debug/<sig>`
/// link opens instantly long after the short response cache has expired. A
/// landed transaction's as-it-happened trace does not change, so the only
/// reason to expire is memory.
/// The trace of `sig` against the cached world for its tier: reconstructed
/// state first; if that verdict disagrees with the chain and current state
/// alone reproduces the on-chain outcome, current state. `pinned` forces a
/// tier (a mutated re-run must use the tier of its base trace). Both the GET
/// and the POST handlers go through here, so an initial trace and its re-runs
/// share one captured world.
fn trace_with_world(
    scope: &Scope,
    sig: &str,
    mutations: &[Mutation],
    tt: &TimeTravel,
    features: &[svmscope::FeatureToggle],
    pinned: Option<&str>,
) -> Result<svmscope::Trace, svmscope::Error> {
    let build = |tier: &str| -> Result<svmscope::Trace, svmscope::Error> {
        // The captured world is cached per (rpc, signature, tier), so a
        // mutated re-run compares against exactly the state its base
        // trace saw, not a fresh reconstruction that may have moved on.
        let key = format!(
            "{}|{}|{sig}|{tier}",
            scope.rpc_url(),
            scope.archive_url().unwrap_or_default()
        );
        let world = match world_get(&key) {
            Some(r) => r,
            None => {
                let r = std::sync::Arc::new(if tier == "now" {
                    scope.replay(sig)?
                } else {
                    scope.replay_at_slot(sig)?
                });
                world_put(key, std::sync::Arc::clone(&r));
                r
            }
        };
        // Time travel and feature toggles are per request: they go on a copy
        // of the shared world's handle, never on the cached world itself.
        let mut replay = (*world).clone();
        replay.set_time_travel(tt.clone());
        replay.set_features(features.to_vec());
        let mut t = replay.trace(mutations)?;
        t.tier = Some(tier.to_string());
        // A failure the chain did not have: name the failing step's
        // accounts that could not be rewound, with their last write.
        let cert = replay.certificate();
        let slot = match cert.fidelity {
            svmscope::Fidelity::Reconstructed { slot } | svmscope::Fidelity::Exact { slot } => {
                Some(slot)
            }
            _ => None,
        };
        t.state_slot = slot;
        let diverged = t.onchain_success.is_some_and(|on| on != t.result.success);
        if let (true, Some(fi), Some(slot)) = (diverged, t.failed_step, slot) {
            if let Some(step) = t.steps.get(fi) {
                let drifted: std::collections::HashSet<&str> =
                    cert.drifted.iter().map(String::as_str).collect();
                let mut seen = std::collections::HashSet::new();
                let mut out = Vec::new();
                for acc in &step.accounts {
                    if !drifted.contains(acc.address.as_str()) || !seen.insert(acc.address.clone())
                    {
                        continue;
                    }
                    if out.len() >= 16 {
                        break;
                    }
                    let last = scope.last_write_slot(&acc.address);
                    out.push(svmscope::DriftedAccount {
                        address: acc.address.clone(),
                        role: acc.name.clone(),
                        last_write_slot: last,
                        changed_since_slot: last.is_some_and(|l| l > slot),
                    });
                }
                t.drifted = out;
            }
        }
        Ok(t)
    };
    if let Some(t) = pinned {
        return build(t);
    }
    // Best of two tiers: reconstructed state at the slot first; if its
    // outcome disagrees with the chain and current state reproduces the
    // on-chain outcome, current state is the more faithful replay.
    let at_slot = build("at_slot")?;
    let diverged = at_slot
        .onchain_success
        .is_some_and(|on| on != at_slot.result.success);
    if diverged && mutations.is_empty() && tt.is_noop() {
        if let Ok(now) = build("now") {
            if now.onchain_success == Some(now.result.success) {
                let mut now = now;
                now.tier_note = Some(format!(
                    "State reconstructed at the transaction's slot {} where the chain {}; current state reproduces the on-chain outcome, so this trace ran against current state.",
                    if at_slot.result.success { "succeeded" } else { "failed" },
                    if at_slot.onchain_success == Some(true) { "succeeded" } else { "failed" }
                ));
                return Ok(now);
            }
        }
    }
    Ok(at_slot)
}

type WorldStore =
    std::collections::HashMap<String, (std::time::Instant, std::sync::Arc<svmscope::Replay>)>;
static WORLDS: std::sync::LazyLock<std::sync::Mutex<WorldStore>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashMap::new()));
const WORLD_TTL: std::time::Duration = std::time::Duration::from_secs(20 * 60);
const WORLD_MAX: usize = 48;
fn world_get(key: &str) -> Option<std::sync::Arc<svmscope::Replay>> {
    let store = WORLDS.lock().ok()?;
    store
        .get(key)
        .filter(|(t, _)| t.elapsed() < WORLD_TTL)
        .map(|(_, r)| std::sync::Arc::clone(r))
}
fn world_put(key: String, replay: std::sync::Arc<svmscope::Replay>) {
    if let Ok(mut store) = WORLDS.lock() {
        store.retain(|_, (t, _)| t.elapsed() < WORLD_TTL);
        if store.len() >= WORLD_MAX {
            store.clear();
        }
        store.insert(key, (std::time::Instant::now(), replay));
    }
}

// `Bytes` is reference-counted: a cache hit hands out a view of the stored
// buffer, never a copy of a multi-megabyte trace.
type TraceStore = std::collections::HashMap<String, (std::time::Instant, axum::body::Bytes)>;
static TRACE_STORE: std::sync::LazyLock<std::sync::Mutex<TraceStore>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashMap::new()));
const TRACE_STORE_TTL: std::time::Duration = std::time::Duration::from_secs(6 * 3600);
const TRACE_STORE_MAX: usize = 300;

async fn trace_get_handler(
    Path(signature): Path<String>,
    Query(q): Query<ClusterQuery>,
) -> Result<axum::response::Response, (StatusCode, String)> {
    use axum::response::IntoResponse;
    let json_response = |body: axum::body::Bytes| {
        (
            [(axum::http::header::CONTENT_TYPE, "application/json")],
            body,
        )
            .into_response()
    };
    let (url, archive) = endpoints_for(
        q.cluster.as_deref(),
        q.rpc.as_deref(),
        q.archive.as_deref(),
        q.relay.as_deref(),
    )?;
    // Keyed by every endpoint that shaped the world: two callers with
    // different archives must never share a cached trace.
    let key = format!("{}|{}|{}", signature, url, archive.as_deref().unwrap_or(""));
    let hit = TRACE_STORE.lock().ok().and_then(|store| {
        store
            .get(&key)
            .filter(|(at, _)| at.elapsed() < TRACE_STORE_TTL)
            .map(|(_, v)| v.clone())
    });
    if let Some(body) = hit {
        return Ok(json_response(body));
    }
    let result =
        tokio::task::spawn_blocking(move || -> Result<svmscope::Trace, svmscope::Error> {
            trace_with_world(
                &scope_for(url, archive),
                &signature,
                &[],
                &TimeTravel::default(),
                &[],
                None,
            )
        })
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("task error: {e}"),
            )
        })?;
    let trace = result.map_err(lib_err)?;
    let body = axum::body::Bytes::from(
        serde_json::to_string(&trace)
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("serialize: {e}")))?,
    );
    if let Ok(mut store) = TRACE_STORE.lock() {
        if store.len() >= TRACE_STORE_MAX {
            let now = std::time::Instant::now();
            store.retain(|_, (at, _)| now.duration_since(*at) < TRACE_STORE_TTL / 2);
            if store.len() >= TRACE_STORE_MAX {
                store.clear();
            }
        }
        store.insert(key, (std::time::Instant::now(), body.clone()));
    }
    Ok(json_response(body))
}

/// POST body for IDL-assisted decoding / instruction listing.
#[derive(Deserialize)]
struct IdlRequest {
    /// Account address (for /decode_account) or program id (for /idl_instructions).
    #[serde(default)]
    address: Option<String>,
    /// The IDL JSON, e.g. the contents of `target/idl/<program>.json`.
    idl: serde_json::Value,
    #[serde(default)]
    cluster: Option<String>,
    #[serde(default)]
    rpc: Option<String>,
    #[serde(default)]
    relay: Option<String>,
    /// Optional archival RPC (one that honours a historical `slot` on
    /// `getAccountInfo`, e.g. Alchemy's Account Archive) for exact state at
    /// the transaction's slot. Vetted like `rpc`; the caller's key stays in
    /// the caller's URL and is never stored or logged.
    #[serde(default)]
    archive: Option<String>,
}

/// POST /decode_account — decode an account, optionally using a supplied IDL.
/// Lets a developer decode their own program's accounts before publishing an IDL.
async fn decode_account_handler(
    Json(req): Json<IdlRequest>,
) -> Result<Json<svmscope::AccountInfo>, (StatusCode, String)> {
    let address = req
        .address
        .clone()
        .ok_or((StatusCode::BAD_REQUEST, "address is required".to_string()))?;
    let (url, archive) = endpoints_for(
        req.cluster.as_deref(),
        req.rpc.as_deref(),
        req.archive.as_deref(),
        req.relay.as_deref(),
    )?;
    let idl = (!req.idl.is_null()).then_some(req.idl);

    let result = tokio::task::spawn_blocking(move || {
        scope_for(url, archive).decode_account(&address, idl.as_ref())
    })
    .await
    .map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("task error: {e}"),
        )
    })?;

    result.map(Json).map_err(lib_err)
}

/// POST /idl_instructions — instructions from a supplied IDL (no on-chain publish needed).
async fn idl_instructions_handler(
    Json(req): Json<IdlRequest>,
) -> Json<Vec<svmscope::idl::IdlInstruction>> {
    Json(svmscope::idl::instructions(&req.idl))
}

/// GET /instructions/:program — the instructions a program exposes (from its IDL),
/// for the transaction builder.
async fn instructions_handler(
    Path(program): Path<String>,
    Query(q): Query<ClusterQuery>,
) -> Result<Json<Vec<svmscope::idl::IdlInstruction>>, (StatusCode, String)> {
    let (url, archive) = endpoints_for(
        q.cluster.as_deref(),
        q.rpc.as_deref(),
        q.archive.as_deref(),
        q.relay.as_deref(),
    )?;
    let result =
        tokio::task::spawn_blocking(move || scope_for(url, archive).program_instructions(&program))
            .await
            .map_err(|e| {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("task error: {e}"),
                )
            })?;

    result.map(Json).map_err(lib_err)
}

/// GET /replay/:signature — run the local replay on demand (analyze skips it).
async fn replay_handler(
    Path(signature): Path<String>,
    Query(q): Query<ClusterQuery>,
) -> Result<Json<ReplayResult>, (StatusCode, String)> {
    let (url, archive) = endpoints_for(
        q.cluster.as_deref(),
        q.rpc.as_deref(),
        q.archive.as_deref(),
        q.relay.as_deref(),
    )?;
    let result = tokio::task::spawn_blocking(move || -> Result<ReplayResult, svmscope::Error> {
        Ok(scope_for(url, archive).replay(&signature)?.run()?.result)
    })
    .await
    .map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("task error: {e}"),
        )
    })?;

    match result {
        Ok(replay) => Ok(Json(replay)),
        Err(e) => Err(lib_err(e)),
    }
}

/// The reconstructed replay-at-slot response: the outcome plus an honest
/// fidelity certificate the UI can show instead of guessing at state drift.
#[derive(Serialize)]
struct ReplayAtSlotResponse {
    result: ReplayResult,
    /// The fidelity label, e.g. `reconstructed@442384762`.
    fidelity: String,
    /// One-line certificate summary.
    certificate: String,
    /// The (anchored) clock the replay ran at.
    clock: String,
    /// Addresses still on current-state data that may differ from the true slot.
    drifted: Vec<String>,
    /// Whether a recorded on-chain outcome exists to verify against.
    verifiable: bool,
    /// How many accounts came from each source (`Recorded`, `MetadataRewind`,
    /// `Unchanged`, `Reconstructed`, `Program`, `CurrentRpc`, ...).
    sources: std::collections::BTreeMap<String, usize>,
    /// Every account with its source, for the UI's per-account view.
    accounts: Vec<svmscope::AccountProvenance>,
    /// The slot this instance's recordings begin at, when it records.
    #[serde(skip_serializing_if = "Option::is_none")]
    recorded_from: Option<u64>,
    /// The slot the world was rebuilt as of.
    slot: u64,
    /// The slot the transaction actually landed in.
    landed_slot: u64,
}

/// `?slot=` for `/replay_at`, plus the usual endpoint overrides.
#[derive(Deserialize)]
struct AtSlotQuery {
    slot: Option<u64>,
    cluster: Option<String>,
    rpc: Option<String>,
    archive: Option<String>,
    relay: Option<String>,
}

/// Rebuild the world as of `slot` (the transaction's own slot when `None`),
/// run the transaction, and describe where every account's state came from.
fn replay_at_response(
    scope: &Scope,
    signature: &str,
    slot: Option<u64>,
) -> Result<ReplayAtSlotResponse, svmscope::Error> {
    let landed_slot = scope
        .landed_slot(signature)?
        .ok_or_else(|| svmscope::Error::TransactionNotFound(signature.to_string()))?;
    let slot = slot.unwrap_or(landed_slot);
    let replay = if slot == landed_slot {
        scope.replay_at_slot(signature)?
    } else {
        scope.replay_at(signature, slot)?
    };
    let cert = replay.certificate();
    let result = replay.run()?.result;
    let mut sources = std::collections::BTreeMap::new();
    for a in &cert.accounts {
        let label = format!("{:?}", a.source);
        let label = label.split([' ', '{']).next().unwrap_or("").to_string();
        *sources.entry(label).or_insert(0) += 1;
    }
    Ok(ReplayAtSlotResponse {
        result,
        fidelity: cert.fidelity.label(),
        certificate: cert.summary(),
        clock: cert.clock.clone(),
        drifted: cert.drifted.clone(),
        verifiable: cert.verifiable,
        sources,
        accounts: cert.accounts.clone(),
        recorded_from: cert.recorded_from,
        slot,
        landed_slot,
    })
}

/// GET /analyze_at/:signature?slot=N — the whole transaction page, rebuilt
/// from a replay against the world as of `slot` (the transaction's own slot
/// when omitted): call tree, balances, token balances, compute and logs from
/// that execution, plus the certificate.
async fn analyze_at_handler(
    Path(signature): Path<String>,
    Query(q): Query<AtSlotQuery>,
) -> Result<Json<std::sync::Arc<serde_json::Value>>, (StatusCode, String)> {
    analyze_at_inner(signature, q, IdlBody::default()).await
}

/// `POST /analyze_at/{signature}` — the same page, with IDLs for programs
/// that never published one on chain.
async fn analyze_at_post_handler(
    Path(signature): Path<String>,
    Query(q): Query<AtSlotQuery>,
    Json(body): Json<IdlBody>,
) -> Result<Json<std::sync::Arc<serde_json::Value>>, (StatusCode, String)> {
    analyze_at_inner(signature, q, body).await
}

async fn analyze_at_inner(
    signature: String,
    q: AtSlotQuery,
    body: IdlBody,
) -> Result<Json<std::sync::Arc<serde_json::Value>>, (StatusCode, String)> {
    if q.slot == Some(0) {
        return Err((StatusCode::BAD_REQUEST, "slot must be positive".to_string()));
    }
    let (url, archive) = endpoints_for(
        q.cluster.as_deref(),
        q.rpc.as_deref(),
        q.archive.as_deref(),
        q.relay.as_deref(),
    )?;
    let slot = q.slot;
    // The fingerprint keeps an answer built with caller IDLs apart from one
    // built without them: same transaction, different decoding.
    let key = format!(
        "analyze_at|{url}|{}|{signature}|{}|{}",
        archive.as_deref().unwrap_or(""),
        slot.map_or("own".to_string(), |s| s.to_string()),
        body.fingerprint()
    );
    if let Some(hit) = at_cache_get(&key) {
        return Ok(Json(hit));
    }
    let _permit = AT_GATE.acquire().await.map_err(|_| {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            "the engine is shutting down".to_string(),
        )
    })?;
    // Someone else may have finished this very replay while we waited.
    if let Some(hit) = at_cache_get(&key) {
        return Ok(Json(hit));
    }
    let out = tokio::task::spawn_blocking(move || {
        let scope = scope_for(url, archive);
        body.apply(&scope);
        if let (Some(slot), Ok(tip)) = (slot, scope.client().get_slot()) {
            if slot > tip {
                return Err(svmscope::Error::Fixture(format!(
                    "slot {slot} is in the future (current slot {tip})"
                )));
            }
        }
        let target = match slot {
            Some(s) => Some(s),
            None => scope.landed_slot(&signature)?,
        };
        if let Some(s) = target {
            check_replay_window(&scope, s)?;
        }
        scope.analyze_at(&signature, slot)
    })
    .await
    .map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("task error: {e}"),
        )
    })?;
    match out {
        Ok(v) => match serde_json::to_value(&v) {
            Ok(value) => Ok(Json(at_cache_put(key, value))),
            Err(e) => Err((StatusCode::INTERNAL_SERVER_ERROR, format!("encode: {e}"))),
        },
        Err(svmscope::Error::Fixture(msg)) if msg.contains("in the future") => {
            Err((StatusCode::BAD_REQUEST, msg))
        }
        Err(e) => Err(lib_err(e)),
    }
}

/// GET /replay_at/:signature?slot=N — replay against the world as of any
/// slot: exact for every account recorded across it, labelled otherwise.
async fn replay_at_handler(
    Path(signature): Path<String>,
    Query(q): Query<AtSlotQuery>,
) -> Result<Json<std::sync::Arc<serde_json::Value>>, (StatusCode, String)> {
    replay_at_inner(signature, q, IdlBody::default()).await
}

/// `POST /replay_at/{signature}` — the same replay, with caller IDLs.
async fn replay_at_post_handler(
    Path(signature): Path<String>,
    Query(q): Query<AtSlotQuery>,
    Json(body): Json<IdlBody>,
) -> Result<Json<std::sync::Arc<serde_json::Value>>, (StatusCode, String)> {
    replay_at_inner(signature, q, body).await
}

async fn replay_at_inner(
    signature: String,
    q: AtSlotQuery,
    body: IdlBody,
) -> Result<Json<std::sync::Arc<serde_json::Value>>, (StatusCode, String)> {
    let Some(slot) = q.slot else {
        return Err((StatusCode::BAD_REQUEST, "slot is required".to_string()));
    };
    if slot == 0 {
        return Err((StatusCode::BAD_REQUEST, "slot must be positive".to_string()));
    }
    let (url, archive) = endpoints_for(
        q.cluster.as_deref(),
        q.rpc.as_deref(),
        q.archive.as_deref(),
        q.relay.as_deref(),
    )?;
    let key = format!(
        "replay_at|{url}|{}|{signature}|{slot}|{}",
        archive.as_deref().unwrap_or(""),
        body.fingerprint()
    );
    if let Some(hit) = at_cache_get(&key) {
        return Ok(Json(hit));
    }
    let _permit = AT_GATE.acquire().await.map_err(|_| {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            "the engine is shutting down".to_string(),
        )
    })?;
    if let Some(hit) = at_cache_get(&key) {
        return Ok(Json(hit));
    }
    let out = tokio::task::spawn_blocking(move || {
        let scope = scope_for(url, archive);
        body.apply(&scope);
        if let Ok(tip) = scope.client().get_slot() {
            if slot > tip {
                return Err(svmscope::Error::Fixture(format!(
                    "slot {slot} is in the future (current slot {tip})"
                )));
            }
        }
        check_replay_window(&scope, slot)?;
        replay_at_response(&scope, &signature, Some(slot))
    })
    .await
    .map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("task error: {e}"),
        )
    })?;
    match out {
        Ok(v) => match serde_json::to_value(&v) {
            Ok(value) => Ok(Json(at_cache_put(key, value))),
            Err(e) => Err((StatusCode::INTERNAL_SERVER_ERROR, format!("encode: {e}"))),
        },
        Err(svmscope::Error::Fixture(msg)) if msg.contains("in the future") => {
            Err((StatusCode::BAD_REQUEST, msg))
        }
        Err(e) => Err(lib_err(e)),
    }
}

/// GET /replay_at_slot/:signature — replay against the transaction's slot at the
/// best fidelity the free data allows, with a per-account drift certificate.
async fn replay_at_slot_handler(
    Path(signature): Path<String>,
    Query(q): Query<ClusterQuery>,
) -> Result<Json<ReplayAtSlotResponse>, (StatusCode, String)> {
    let (url, archive) = endpoints_for(
        q.cluster.as_deref(),
        q.rpc.as_deref(),
        q.archive.as_deref(),
        q.relay.as_deref(),
    )?;
    let out =
        tokio::task::spawn_blocking(move || -> Result<ReplayAtSlotResponse, svmscope::Error> {
            // SVMSCOPE_ARCHIVE_URL (an archival endpoint honoring the `slot`
            // param, e.g. Alchemy's Account Archive) upgrades this replay from
            // Reconstructed to Exact. Unset = free reconstruction, as before.
            let scope = scope_for(url, archive);
            // This route replays at the transaction's own slot, which can be
            // older than the window the other replay routes enforce.
            if let Some(landed) = scope.landed_slot(&signature)? {
                check_replay_window(&scope, landed)?;
            }
            replay_at_response(&scope, &signature, None)
        })
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("task error: {e}"),
            )
        })?;

    match out {
        Ok(v) => Ok(Json(v)),
        Err(e) => Err(lib_err(e)),
    }
}

#[derive(Deserialize)]
struct CounterfactualQuery {
    /// The account whose lamport balance to search.
    account: String,
    /// Build on the replay as of this slot (the transaction's own or any
    /// other inside the window) instead of today's state.
    slot: Option<u64>,
    /// Search range (lamports); defaults 0 .. 0.1 SOL.
    lo: Option<u64>,
    hi: Option<u64>,
    cluster: Option<String>,
    rpc: Option<String>,
    /// Optional archival RPC for exact historical state; vetted like `rpc`.
    archive: Option<String>,
    relay: Option<String>,
}

/// The counterfactual threshold result — the balance at which the outcome flips.
#[derive(Serialize)]
struct CounterfactualResponse {
    account: String,
    lo: u64,
    hi: u64,
    /// The lowest balance in range whose outcome differs from the low bound, or
    /// null if the outcome is the same across the whole range (no flip).
    flips_at: Option<u64>,
    /// Outcome (success) at the low and high bounds.
    low_success: bool,
    high_success: bool,
    /// How many replays the binary search ran.
    evaluations: u32,
}

/// GET /counterfactual/:signature — binary-search an account's lamport balance
/// for the point where the transaction's outcome flips. Free: pure current-state
/// replay, no archive.
async fn counterfactual_handler(
    Path(signature): Path<String>,
    Query(q): Query<CounterfactualQuery>,
) -> Result<Json<CounterfactualResponse>, (StatusCode, String)> {
    let (url, archive) = endpoints_for(
        q.cluster.as_deref(),
        q.rpc.as_deref(),
        q.archive.as_deref(),
        q.relay.as_deref(),
    )?;
    let lo = q.lo.unwrap_or(0);
    let hi = q.hi.unwrap_or(100_000_000);
    if lo > hi {
        return Err((
            StatusCode::BAD_REQUEST,
            format!("lo ({lo}) must not exceed hi ({hi})"),
        ));
    }
    let account = q.account.clone();
    let at_slot = q.slot;

    let out =
        tokio::task::spawn_blocking(move || -> Result<CounterfactualResponse, svmscope::Error> {
            let replay = replay_for(&scope_for(url, archive), &signature, at_slot)?;
            let acct = account.clone();
            let threshold = replay
                .find_threshold(lo, hi, move |v| vec![Mutation::lamports(acct.clone(), v)])?;
            Ok(match threshold {
                Some(t) => CounterfactualResponse {
                    account,
                    lo,
                    hi,
                    flips_at: Some(t.flips_at),
                    low_success: t.low_success,
                    high_success: t.high_success,
                    evaluations: t.evaluations,
                },
                // No crossing: both bounds had the same outcome. Report that
                // outcome rather than inventing a double failure.
                None => {
                    let acct = account.clone();
                    let same = replay
                        .simulate(&[Mutation::lamports(acct, lo)])?
                        .result
                        .success;
                    CounterfactualResponse {
                        account,
                        lo,
                        hi,
                        flips_at: None,
                        low_success: same,
                        high_success: same,
                        evaluations: 3,
                    }
                }
            })
        })
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("task error: {e}"),
            )
        })?;

    match out {
        Ok(v) => Ok(Json(v)),
        Err(e) => Err(lib_err(e)),
    }
}

/// GET /scan/:signature — auto-scan every account field (and SOL balance) for
/// the numeric thresholds that flip the transaction's outcome. Free: local
/// current-state replays, no archive.
async fn scan_handler(
    Path(signature): Path<String>,
    Query(q): Query<ClusterQuery>,
) -> Result<Json<Vec<svmscope::BreakingPoint>>, (StatusCode, String)> {
    let (url, archive) = endpoints_for(
        q.cluster.as_deref(),
        q.rpc.as_deref(),
        q.archive.as_deref(),
        q.relay.as_deref(),
    )?;
    let at_slot = q.slot;
    let out = tokio::task::spawn_blocking(
        move || -> Result<Vec<svmscope::BreakingPoint>, svmscope::Error> {
            let scope = scope_for(url, archive);
            let analysis = scope.analyze(&signature)?;
            let accounts: Vec<String> = analysis
                .accounts
                .iter()
                .map(|a| a.address.clone())
                .collect();
            let replay = replay_for(&scope, &signature, at_slot)?;
            svmscope::scan_breaking_points_on(
                &scope,
                &replay,
                &accounts,
                svmscope::ScanOptions::default(),
            )
        },
    )
    .await
    .map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("task error: {e}"),
        )
    })?;

    match out {
        Ok(v) => Ok(Json(v)),
        Err(e) => Err(lib_err(e)),
    }
}

/// GET /diagnose/:signature — plain-English "why did it fail, and how do I fix
/// it?" over the recorded on-chain outcome. Free.
async fn diagnose_handler(
    Path(signature): Path<String>,
    Query(q): Query<ClusterQuery>,
) -> Result<Json<svmscope::Diagnosis>, (StatusCode, String)> {
    let (url, archive) = endpoints_for(
        q.cluster.as_deref(),
        q.rpc.as_deref(),
        q.archive.as_deref(),
        q.relay.as_deref(),
    )?;
    let out = tokio::task::spawn_blocking(move || scope_for(url, archive).diagnose(&signature))
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("task error: {e}"),
            )
        })?;
    match out {
        Ok(d) => Ok(Json(d)),
        Err(e) => Err(lib_err(e)),
    }
}

/// GET /freeze/:signature — capture a self-contained fixture for offline replay.
async fn freeze_handler(
    Path(signature): Path<String>,
    Query(q): Query<ClusterQuery>,
) -> Result<Json<svmscope::Fixture>, (StatusCode, String)> {
    let (url, archive) = endpoints_for(
        q.cluster.as_deref(),
        q.rpc.as_deref(),
        q.archive.as_deref(),
        q.relay.as_deref(),
    )?;
    let result = tokio::task::spawn_blocking(move || scope_for(url, archive).capture(&signature))
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("task error: {e}"),
            )
        })?;

    match result {
        Ok(fx) => Ok(Json(fx)),
        Err(e) => Err(lib_err(e)),
    }
}

/// GET /rpc_relay (WebSocket) — the page volunteers to make this engine's RPC
/// calls for it.
///
/// Answers with the session id to pass as `relay=` on later requests, then
/// carries JSON-RPC envelopes down and their answers back up. See [`relay`] for
/// why this exists: a validator on the reader's machine is unreachable from a
/// server, and quietly substituting a public cluster is worse than saying so.
async fn rpc_relay_handler(ws: WebSocketUpgrade, Query(q): Query<RelayQuery>) -> Response {
    let label = q.label.unwrap_or_else(|| "browser relay".to_string());
    ws.on_upgrade(move |socket| run_relay(socket, label))
}

#[derive(Deserialize)]
struct RelayQuery {
    /// What the page says it is pointed at, shown in errors. Never dialled.
    label: Option<String>,
}

/// An unguessable session id. The id is the whole authorisation: anyone holding
/// it can have this engine ask that browser to fetch from its own machine, so it
/// must not be derivable from a timestamp or a counter.
fn relay_session_id() -> String {
    let mut bytes = [0u8; 16];
    if getrandom::fill(&mut bytes).is_err() {
        // Refuse to fall back to something predictable.
        return String::new();
    }
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

async fn run_relay(socket: WebSocket, label: String) {
    use futures_util::{SinkExt, StreamExt};

    let id = relay_session_id();
    if id.is_empty() {
        return;
    }
    let Some((_session, mut calls)) = relay::open(id.clone(), label) else {
        let mut socket = socket;
        let _ = socket
            .send(Message::Text(
                json!({ "type": "busy", "error": "too many relays open right now" })
                    .to_string()
                    .into(),
            ))
            .await;
        return;
    };

    let (mut tx, mut rx) = socket.split();
    if tx
        .send(Message::Text(
            json!({ "type": "ready", "session": id }).to_string().into(),
        ))
        .await
        .is_err()
    {
        relay::close(&id);
        return;
    }

    // Down: each call the engine wants made. Up: the page's answers.
    let down_id = id.clone();
    let down = tokio::spawn(async move {
        while let Some(call) = calls.recv().await {
            let frame = json!({ "type": "call", "request": relay::envelope_of(&call) });
            if tx
                .send(Message::Text(frame.to_string().into()))
                .await
                .is_err()
            {
                break;
            }
        }
        let _ = tx.close().await;
        relay::close(&down_id);
    });

    while let Some(Ok(msg)) = rx.next().await {
        match msg {
            Message::Text(text) => {
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) {
                    relay::deliver(&id, &v);
                }
            }
            Message::Close(_) => break,
            _ => {}
        }
    }
    relay::close(&id);
    down.abort();
}

/// GET /api — machine-readable index of the public API, so anything that wants to
/// call svmscope (a dApp, wallet, bot, CI job) can discover the surface in one hit.
async fn api_index() -> Json<serde_json::Value> {
    Json(json!({
        "name": "svmscope",
        "description": "Solana transaction simulation layer — decode, replay, mutate, assert.",
        "version": env!("CARGO_PKG_VERSION"),
        // Lets the UI hide the custom-RPC field on instances that don't allow it.
        "custom_rpc": custom_rpc_allowed(),
        // This engine can borrow a reader's browser to reach endpoints only that
        // reader can see, which is what makes Localnet work on a hosted site.
        "rpc_relay": true,
        "custom_archive": custom_rpc_allowed(),
        // The recorded window's first slot, so the UI can say up front whether
        // a slot can be exact, and the programs whose state comes back from
        // their own event logs at any date.
        "recorded_from": RECORDS.as_ref().and_then(|s| {
            use svmscope::records::StateStore;
            s.covered_from().ok().flatten()
        }),
        "event_log_programs": ["6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P"],
        "replay_window_days": replay_window_days(),
        "slot_at": "/slot_at?time=<unix seconds | ISO 8601> → the nearest slot",
        "archive": "Any route that replays accepts `archive` (query or body): an archival RPC that honours a historical slot, e.g. Alchemy's Account Archive, for exact state at the transaction's slot. Same opt-in and vetting as `rpc`.",
        "endpoints": {
            "GET  /analyze/{signature}":  "Decode a transaction: CPI tree, balance & token changes, compute, and IDL-decoded accounts.",
            "GET  /replay/{signature}":   "Re-execute the transaction locally against reconstructed pre-state.",
            "POST /simulate":             "{ signature, mutations[], time_travel?, features? } — replay with what-if account mutations, an optional clock warp, and optional runtime feature-gate toggles.",
            "POST /simulate_suite":       "{ signature, scenarios[], time_travel?, features? } — run a suite of scenarios with outcome + state assertions, under optional feature-gate toggles.",
            "POST /preflight":            "{ transaction, mutations[] } — simulate an UNSIGNED transaction against current state before sending.",
            "POST /trace":                "{ signature | transaction, mutations[], time_travel?, features? } — step debugger: every instruction and CPI with decoded args, per-step account diffs, and the failing step.",
            "GET  /trace/{signature}":    "The same trace with no mutations, cacheable.",
            "POST /profile":              "{ signature, mutations[]?, time_travel?, features?, symbols[{program, elf_b64}]? } — compute profiler: every BPF instruction attributed to functions, syscalls and call stacks per program frame; symbols name a program's functions from its .debug file.",
            "GET  /profile/{signature}":  "The as-it-happened compute profile, no symbols, cacheable.",
            "GET  /freeze/{signature}":   "Capture a self-contained fixture for deterministic, offline replay.",
            "POST /preflight_report":     "{ transaction, mutations[] } — preflight as an HTML report.",
            "POST /replay_report":        "{ signature, scenarios[] } — a suite run as an HTML report.",
            "GET  /replay_at_slot/{signature}": "Replay against state reconstructed at the transaction's slot, with a per-account fidelity certificate.",
            "GET  /replay_at/{signature}?slot=N": "Replay against the world as of any slot (exact for every account recorded across it, labelled otherwise), with the same certificate.",
            "GET  /analyze_at/{signature}?slot=N": "The whole transaction page rebuilt from a replay as of any slot (own slot when omitted): call tree, balances, token balances, compute, logs, plus the certificate.",
            "GET  /counterfactual/{signature}?account&lo&hi": "Binary-search the lamport balance at which the outcome flips.",
            "GET  /scan/{signature}":     "Which accounts, when drained, change the outcome.",
            "GET  /diagnose/{signature}": "A failure explained: error name, docs, the step and accounts involved.",
            "GET  /account/{address}":    "An account decoded through its program's layout or IDL.",
            "GET  /signatures/{address}": "Recent signatures for an address.",
            "GET  /instructions/{program}": "The instructions a program's on-chain IDL declares.",
            "POST /idl_instructions":     "{ idl } — the same, for an IDL supplied in the request.",
            "POST /decode_account":       "{ owner, data_b64 } — decode raw account bytes.",
            "GET  /stats":                "Usage counters (token-gated)."
        }
    }))
}

/// The client's address, preferring the proxy-forwarded IP since we run behind
/// Render's load balancer (otherwise every request looks like the same peer).
///
/// Use the *rightmost* `X-Forwarded-For` entry: our trusted proxy appends the
/// real client IP on the right, while any entries to the left are supplied by
/// the client itself. Taking the leftmost would let a client send a random
/// `X-Forwarded-For` per request and mint a fresh identity each time, defeating
/// the rate limiter (the only DoS defense on the unauthenticated endpoints).
/// Whether the instance sits behind a proxy that sets `X-Forwarded-For`
/// (`SVMSCOPE_TRUST_PROXY=1`, as on Render). Without it the header is
/// client-controlled and must not become the rate-limit identity.
fn trust_proxy() -> bool {
    static TRUST: std::sync::LazyLock<bool> = std::sync::LazyLock::new(|| {
        std::env::var("SVMSCOPE_TRUST_PROXY")
            .map(|v| matches!(v.trim(), "1" | "true" | "yes"))
            .unwrap_or(false)
    });
    *TRUST
}

fn client_id(req: &Request, peer: Option<SocketAddr>) -> String {
    let forwarded = if trust_proxy() {
        req.headers()
            .get("x-forwarded-for")
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.split(',').next_back())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    } else {
        None
    };
    forwarded
        .or_else(|| peer.map(|p| p.ip().to_string()))
        .unwrap_or_else(|| "unknown".into())
}

/// Reject clients that exceed the per-minute allowance. Simulation is expensive,
/// so one script shouldn't be able to monopolise the instance.
async fn rate_limit(
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    req: Request,
    next: Next,
) -> Response {
    // Only meter the work-doing API; serving the page itself is cheap.
    let path = req.uri().path().to_string();

    if let Some(label) = endpoint_label(&path) {
        let cid = client_id(&req, Some(peer));
        if let Err(retry) = guard::rate_check(&cid) {
            return (
                StatusCode::TOO_MANY_REQUESTS,
                [(header::RETRY_AFTER, retry.to_string())],
                format!("rate limit reached — try again in {retry}s"),
            )
                .into_response();
        }
        // Count real usage (allowed, work-doing requests) so the operator can see
        // whether anyone is using svmscope. Private — read via the /stats token.
        stats::record(label, &cid);
    }
    next.run(req).await
}

/// The usage bucket for a path, or `None` if it isn't a metered, work-doing route.
fn endpoint_label(path: &str) -> Option<&'static str> {
    // `simulate_suite` must be checked before `simulate` (prefix overlap).
    if path.starts_with("/analyze") {
        Some("analyze")
    } else if path.starts_with("/replay") {
        Some("replay")
    } else if path.starts_with("/simulate_suite") {
        Some("simulate_suite")
    } else if path.starts_with("/simulate") {
        Some("simulate")
    } else if path.starts_with("/preflight") {
        Some("preflight")
    } else if path.starts_with("/freeze") {
        Some("freeze")
    } else if path.starts_with("/profile") {
        Some("profile")
    } else if path.starts_with("/trace") {
        Some("trace")
    } else if path.starts_with("/account") {
        Some("account")
    } else if path.starts_with("/signatures") {
        Some("signatures")
    } else if path.starts_with("/scan") {
        Some("scan")
    } else if path.starts_with("/counterfactual") {
        Some("counterfactual")
    } else if path.starts_with("/diagnose") {
        Some("diagnose")
    } else if path.starts_with("/decode_account") {
        Some("decode_account")
    } else if path.starts_with("/instructions") || path.starts_with("/idl_instructions") {
        Some("instructions")
    } else {
        None
    }
}

/// Serve repeat GETs of the same URL from a short-lived cache. Demo traffic means
/// many people opening the *same* link, so this is where most of the savings are.
async fn cache_layer(req: Request, next: Next) -> Response {
    let path = req.uri().path();
    let cacheable = req.method() == axum::http::Method::GET
        && (path.starts_with("/analyze")
            || path.starts_with("/trace")
            || path.starts_with("/profile")
            || path.starts_with("/account")
            || path.starts_with("/signatures")
            || path.starts_with("/replay"));

    if !cacheable {
        return next.run(req).await;
    }

    let key = req.uri().to_string();
    if let Some(body) = guard::cache_get(&key) {
        return (
            StatusCode::OK,
            [
                (header::CONTENT_TYPE, "application/json"),
                (header::HeaderName::from_static("x-cache"), "HIT"),
            ],
            body,
        )
            .into_response();
    }

    let res = next.run(req).await;
    if res.status() != StatusCode::OK {
        return res;
    }

    // Buffer the body so it can be cached and still returned.
    let (mut parts, body) = res.into_parts();
    let bytes = match axum::body::to_bytes(body, 32 * 1024 * 1024).await {
        Ok(b) => b,
        Err(_) => {
            return (StatusCode::INTERNAL_SERVER_ERROR, "response read error").into_response()
        }
    };
    if let Ok(text) = String::from_utf8(bytes.to_vec()) {
        guard::cache_put(key, text);
    }
    parts.headers.insert(
        header::HeaderName::from_static("x-cache"),
        header::HeaderValue::from_static("MISS"),
    );
    Response::from_parts(parts, Body::from(bytes))
}

/// The private stats query: `/stats?token=<secret>`.
#[derive(Deserialize)]
struct StatsQuery {
    token: Option<String>,
}

/// GET /stats — private usage numbers, gated by the `SVMSCOPE_STATS_TOKEN` secret.
///
/// If the token isn't configured, or the caller's `?token=` doesn't match, this
/// returns a plain 404 — so the endpoint is invisible to anyone who doesn't hold
/// the secret, and never exposes usage data publicly.
async fn stats_handler(Query(q): Query<StatsQuery>) -> Response {
    let configured = std::env::var("SVMSCOPE_STATS_TOKEN")
        .ok()
        .filter(|t| !t.is_empty());
    match configured {
        Some(expected) if q.token.as_deref().is_some_and(|t| ct_eq(t, &expected)) => {
            Json(stats::snapshot_json()).into_response()
        }
        _ => (StatusCode::NOT_FOUND, "not found").into_response(),
    }
}

/// Constant-time string equality for the stats token, so a mismatch can't be
/// narrowed by response timing. (Length is not treated as secret.)
fn ct_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

// The free instance has 512 MB. glibc's allocator keeps the fragments of the
// recorder's steady churn (hundreds of account fetches every two seconds)
// and of each replay's large buffers; mimalloc returns them and the process
// stays near its working set.
#[global_allocator]
static ALLOC: mimalloc::MiMalloc = mimalloc::MiMalloc;

#[tokio::main]
async fn main() {
    spawn_recorder();
    // Restore any persisted usage tally before serving.
    stats::load();

    // Permissive CORS so any web app can call the API cross-origin — this is what
    // turns the engine from a local binary into infrastructure others build on.
    let cors = tower_http::cors::CorsLayer::permissive();

    let app = Router::new()
        .route("/", get(index))
        .route("/api", get(api_index))
        .route("/rpc_relay", get(rpc_relay_handler))
        .route(
            "/analyze/{signature}",
            get(analyze_handler).post(analyze_post_handler),
        )
        .route("/simulate", post(simulate_handler))
        .route("/simulate_suite", post(suite_handler))
        .route("/preflight", post(preflight_handler))
        .route("/preflight_report", post(preflight_report_handler))
        .route("/replay_report", post(replay_report_handler))
        .route("/trace", post(trace_handler))
        .route("/trace/{signature}", get(trace_get_handler))
        .route(
            "/profile",
            post(profile_handler).layer(axum::extract::DefaultBodyLimit::max(
                MAX_SYMBOL_BYTES * 2 + 1024 * 1024,
            )),
        )
        .route("/profile/{signature}", get(profile_get_handler))
        .route("/debug/{signature}", get(index))
        .route("/tx/{signature}", get(index))
        .route("/address/{address}", get(index))
        .route("/flame/{signature}", get(index))
        .route("/instructions/{program}", get(instructions_handler))
        .route("/idl_instructions", post(idl_instructions_handler))
        .route("/decode_account", post(decode_account_handler))
        .route("/account/{address}", get(account_handler))
        .route("/signatures/{address}", get(signatures_handler))
        .route("/replay/{signature}", get(replay_handler))
        .route("/slot_at", get(slot_at_handler))
        .route("/replay_at_slot/{signature}", get(replay_at_slot_handler))
        .route(
            "/replay_at/{signature}",
            get(replay_at_handler).post(replay_at_post_handler),
        )
        .route(
            "/analyze_at/{signature}",
            get(analyze_at_handler).post(analyze_at_post_handler),
        )
        .route("/counterfactual/{signature}", get(counterfactual_handler))
        .route("/scan/{signature}", get(scan_handler))
        .route("/diagnose/{signature}", get(diagnose_handler))
        .route("/freeze/{signature}", get(freeze_handler))
        .route("/stats", get(stats_handler))
        // Order matters: rate limit first (cheapest rejection), then serve from
        // cache, then CORS headers on whatever comes back.
        .layer(middleware::from_fn(cache_layer))
        .layer(middleware::from_fn(rate_limit))
        .layer(cors);

    // Host/port from the environment so it runs unchanged locally and on any
    // platform (Fly, Render, Railway, Docker) that injects PORT and expects 0.0.0.0.
    let host = std::env::var("HOST").unwrap_or_else(|_| "0.0.0.0".to_string());
    let port: u16 = std::env::var("PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(3000);
    let addr = format!("{host}:{port}");

    let listener = match tokio::net::TcpListener::bind(&addr).await {
        Ok(l) => l,
        Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {
            eprintln!("svmscope: {addr} is already in use — is a server already running?");
            eprintln!("  (stop it with:  lsof -ti:{port} | xargs kill )");
            std::process::exit(1);
        }
        Err(e) => {
            eprintln!("svmscope: could not bind {addr}: {e}");
            std::process::exit(1);
        }
    };
    println!("svmscope → http://{addr}   (API index: /api)");
    // into_make_service_with_connect_info gives the rate limiter the peer address.
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown_signal())
    .await
    .unwrap();
    // Give the recorder time to push its unpushed tail (a platform typically
    // allows tens of seconds between SIGTERM and SIGKILL).
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(25);
    while !RECORDER_DONE.load(std::sync::atomic::Ordering::SeqCst)
        && std::time::Instant::now() < deadline
    {
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ssrf_guard_blocks_internal_targets() {
        // Cloud metadata, loopback, and private ranges must never be fetched.
        assert!(vet_custom_rpc("http://169.254.169.254/latest/meta-data").is_none());
        assert!(vet_custom_rpc("http://localhost:8899").is_none());
        assert!(vet_custom_rpc("http://127.0.0.1/").is_none());
        assert!(vet_custom_rpc("http://10.0.0.5:8899").is_none());
        assert!(vet_custom_rpc("http://192.168.1.1").is_none());
        assert!(vet_custom_rpc("http://[::1]:8899").is_none());
        assert!(vet_custom_rpc("http://0.0.0.0").is_none());
        // Non-http schemes and junk are rejected outright.
        assert!(vet_custom_rpc("file:///etc/passwd").is_none());
        assert!(vet_custom_rpc("not-a-url").is_none());
    }

    #[test]
    fn ssrf_guard_allows_public_rpc() {
        // A routable public IP literal passes through unchanged — no DNS, so the
        // test is hermetic (a hostname would need a live resolver).
        let ok = vet_custom_rpc("https://8.8.8.8/");
        assert_eq!(ok.as_deref(), Some("https://8.8.8.8/"));
    }

    #[test]
    fn blocked_ip_classifies_ranges() {
        assert!(is_blocked_ip("169.254.169.254".parse().unwrap()));
        assert!(is_blocked_ip("127.0.0.1".parse().unwrap()));
        assert!(is_blocked_ip("100.100.0.1".parse().unwrap())); // CGNAT
        assert!(!is_blocked_ip("8.8.8.8".parse().unwrap()));
        assert!(!is_blocked_ip("1.1.1.1".parse().unwrap()));
    }

    #[test]
    fn caller_rpc_ignored_when_custom_disabled() {
        // Default (no SVMSCOPE_ALLOW_CUSTOM_RPC): a caller-supplied RPC — even a
        // syntactically fine public one — must never be used verbatim. This is the
        // real SSRF backstop on the shared public instance, independent of DNS.
        assert!(!custom_rpc_allowed());
        let out = rpc_for(None, Some("http://8.8.8.8:9999/evil"), None);
        assert!(out.is_err(), "a caller RPC must be refused, not honoured");
        assert_ne!(out.ok(), Some(Rpc::Url("http://8.8.8.8:9999/evil".into())));
    }

    // The bug this replaced: asking for localnet on an engine that cannot serve
    // it used to answer with mainnet, so a local program looked like it was
    // deployed and working when the data came from another chain entirely.
    #[test]
    fn localnet_is_refused_not_quietly_swapped_for_mainnet() {
        assert!(!custom_rpc_allowed());
        for name in ["localnet", "local", "localhost", "l"] {
            let out = rpc_for(Some(name), None, None);
            assert!(out.is_err(), "{name} must be refused");
            assert_ne!(out.ok(), Some(Rpc::Url(rpc_url())));
        }
        // An unknown name is an error too, rather than the default endpoint.
        assert!(rpc_for(Some("mainnnet"), None, None).is_err());
        // The public clusters still resolve.
        assert!(rpc_for(Some("devnet"), None, None).is_ok());
        assert!(rpc_for(None, None, None).is_ok());
        // An id naming no connected page is refused rather than silently
        // falling back to a public cluster.
        assert!(rpc_for(Some("localnet"), None, Some("not-a-session")).is_err());
    }

    #[test]
    fn caller_archive_ignored_when_custom_disabled() {
        // A caller archive is a URL to an arbitrary host: same SSRF backstop as
        // a caller rpc. With custom endpoints disabled it must never be used.
        assert!(!custom_rpc_allowed());
        assert_ne!(
            archive_for(Some("https://8.8.8.8/archive")).as_deref(),
            Some("https://8.8.8.8/archive")
        );
        assert!(
            archive_for(Some("http://169.254.169.254/")).is_none()
                || std::env::var("SVMSCOPE_ARCHIVE_URL").is_ok()
        );
    }

    #[test]
    fn scope_carries_the_archive_it_was_given() {
        let with = scope_for(
            Rpc::Url("https://8.8.8.8/".into()),
            Some("https://8.8.8.8/archive".into()),
        );
        assert_eq!(
            with.archive_url().as_deref(),
            Some("https://8.8.8.8/archive")
        );
        let without = scope_for(Rpc::Url("https://8.8.8.8/".into()), None);
        assert!(without.archive_url().is_none() || std::env::var("SVMSCOPE_ARCHIVE_URL").is_ok());
    }

    #[test]
    fn public_instance_rejects_url_and_localnet_clusters() {
        // The second SSRF vector: `cluster` is also caller-controlled, and
        // resolve_rpc honors URL-shaped and localnet clusters verbatim. On a public
        // instance neither may reach an internal target.
        assert!(!custom_rpc_allowed());
        // Both are refused outright now. The endpoint never appears in the
        // answer, because there is no answer.
        let meta = rpc_for(Some("http://169.254.169.254/latest/meta-data"), None, None);
        assert!(meta.is_err(), "a URL-shaped cluster must be refused");
        assert!(!meta
            .map(|r| r.to_string())
            .unwrap_or_default()
            .contains("169.254"));
        let local = rpc_for(Some("localnet"), None, None);
        assert!(
            local.is_err(),
            "localnet must be refused on a public instance"
        );
        assert!(!local
            .map(|r| r.to_string())
            .unwrap_or_default()
            .contains("127.0.0.1"));
        // A legitimate public cluster still resolves normally.
        assert!(rpc_for(Some("devnet"), None, None)
            .expect("devnet resolves")
            .to_string()
            .starts_with("http"));
    }

    #[test]
    fn public_cluster_allowlist() {
        assert!(public_cluster_ok("mainnet"));
        assert!(public_cluster_ok("devnet"));
        assert!(public_cluster_ok("testnet"));
        assert!(!public_cluster_ok("localnet"));
        assert!(!public_cluster_ok("http://169.254.169.254"));
    }

    #[test]
    fn localnet_alias_is_a_name_never_a_url() {
        assert!(localnet_alias("localnet"));
        assert!(localnet_alias("localhost"));
        // A URL is never a localnet "alias" — so it can't slip through the
        // custom-RPC-enabled branch as a cluster.
        assert!(!localnet_alias("http://127.0.0.1:8899"));
        assert!(!localnet_alias("http://169.254.169.254"));
    }
}
