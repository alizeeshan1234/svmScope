//! svmscope web server — serves the frontend and a JSON analysis endpoint.
//!
//! Run with `cargo run --bin server`, then open http://127.0.0.1:3000.

mod guard;
mod stats;

use axum::{
    body::Body,
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
        _ => None,
    }
}

/// Per-request cluster selection: `?cluster=devnet` (or mainnet/testnet/localnet)
/// or `?rpc=<url>`, so one instance serves every cluster.
#[derive(Deserialize)]
struct ClusterQuery {
    cluster: Option<String>,
    rpc: Option<String>,
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
    // host[:port] is everything up to the first '/', '?' or '#'.
    let hostport = rest.split(['/', '?', '#']).next().unwrap_or("");
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
fn rpc_for(cluster: Option<&str>, rpc: Option<&str>) -> String {
    // Only trust a caller-supplied RPC when the operator has opted in.
    let allow = custom_rpc_allowed();
    // Only ever act on named clusters: public presets always, plus localnet when
    // the operator enabled custom RPC. A URL-shaped cluster is never honored — even
    // by a self-hoster — so `cluster` can't be an SSRF vector; custom endpoints go
    // through the vetted `rpc` field instead.
    let cluster = cluster.filter(|c| public_cluster_ok(c) || (allow && localnet_alias(c)));
    if allow {
        if let Some(u) = rpc {
            if let Some(safe) = vet_custom_rpc(u) {
                return safe;
            }
        }
    }
    if let Some(u) = cluster_env_rpc(cluster) {
        return u;
    }
    // Never let a caller `rpc` reach resolve_rpc's verbatim-URL branch unless it's
    // both allowed and vetted. `cluster` is already sanitized above.
    let safe_rpc = rpc.filter(|u| allow && vet_custom_rpc(u).is_some());
    // `cluster` is pre-sanitized above, so an unknown name can't reach here;
    // fall back to the default endpoint defensively anyway.
    svmscope::resolve_rpc_url(cluster, safe_rpc, &rpc_url()).unwrap_or_else(|_| rpc_url())
}

/// POST body for /simulate.
#[derive(Deserialize)]
struct SimRequest {
    signature: String,
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

/// GET /analyze/:signature — decode + replay a transaction, return JSON.
async fn analyze_handler(
    Path(signature): Path<String>,
    Query(q): Query<ClusterQuery>,
) -> Result<Json<Analysis>, (StatusCode, String)> {
    let url = rpc_for(q.cluster.as_deref(), q.rpc.as_deref());
    // `analyze` does blocking I/O (RPC) and heavy CPU work (replay), so run it on
    // the blocking thread pool instead of stalling the async runtime.
    let result = tokio::task::spawn_blocking(move || Scope::new(url).analyze(&signature))
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

    let url = rpc_for(req.cluster.as_deref(), req.rpc.as_deref());
    let result = tokio::task::spawn_blocking(move || -> Result<ReplayResult, svmscope::Error> {
        let mut replay = Scope::new(url).replay(&req.signature)?;
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
    let url = rpc_for(req.cluster.as_deref(), req.rpc.as_deref());
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
            let mut replay = Scope::new(url).replay(&signature)?;
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

    let url = rpc_for(req.cluster.as_deref(), req.rpc.as_deref());
    let result = tokio::task::spawn_blocking(move || -> Result<ReplayResult, svmscope::Error> {
        let replay = Scope::new(url).preflight(&req.transaction)?;
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
    let url = rpc_for(q.cluster.as_deref(), q.rpc.as_deref());
    let result = tokio::task::spawn_blocking(move || Scope::new(url).account(&address))
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
    let url = rpc_for(q.cluster.as_deref(), q.rpc.as_deref());
    let result = tokio::task::spawn_blocking(move || Scope::new(url).signatures(&address, 25))
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
    let url = rpc_for(req.cluster.as_deref(), req.rpc.as_deref());
    let tt = req.time_travel.clone();
    let result = tokio::task::spawn_blocking(
        move || -> Result<svmscope::SimulationReport, svmscope::Error> {
            let mut replay = Scope::new(url).preflight(&req.transaction)?;
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
    let url = rpc_for(req.cluster.as_deref(), req.rpc.as_deref());
    let tt = req.time_travel.clone();
    let result = tokio::task::spawn_blocking(
        move || -> Result<svmscope::SimulationReport, svmscope::Error> {
            let mut replay = Scope::new(url).replay(&req.signature)?;
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
    let url = rpc_for(req.cluster.as_deref(), req.rpc.as_deref());
    let idl = (!req.idl.is_null()).then_some(req.idl);

    let result =
        tokio::task::spawn_blocking(move || Scope::new(url).decode_account(&address, idl.as_ref()))
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
    let url = rpc_for(q.cluster.as_deref(), q.rpc.as_deref());
    let result =
        tokio::task::spawn_blocking(move || Scope::new(url).program_instructions(&program))
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
    let url = rpc_for(q.cluster.as_deref(), q.rpc.as_deref());
    let result = tokio::task::spawn_blocking(move || -> Result<ReplayResult, svmscope::Error> {
        Ok(Scope::new(url).replay(&signature)?.run()?.result)
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
}

/// GET /replay_at_slot/:signature — replay against the transaction's slot at the
/// best fidelity the free data allows, with a per-account drift certificate.
async fn replay_at_slot_handler(
    Path(signature): Path<String>,
    Query(q): Query<ClusterQuery>,
) -> Result<Json<ReplayAtSlotResponse>, (StatusCode, String)> {
    let url = rpc_for(q.cluster.as_deref(), q.rpc.as_deref());
    let out = tokio::task::spawn_blocking(
        move || -> Result<ReplayAtSlotResponse, svmscope::Error> {
            let replay = Scope::new(url).replay_at_slot(&signature)?;
            let cert = replay.certificate();
            let result = replay.run()?.result;
            Ok(ReplayAtSlotResponse {
                result,
                fidelity: cert.fidelity.label(),
                certificate: cert.summary(),
                clock: cert.clock.clone(),
                drifted: cert.drifted.clone(),
                verifiable: cert.verifiable,
            })
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

/// GET /freeze/:signature — capture a self-contained fixture for offline replay.
async fn freeze_handler(
    Path(signature): Path<String>,
    Query(q): Query<ClusterQuery>,
) -> Result<Json<svmscope::Fixture>, (StatusCode, String)> {
    let url = rpc_for(q.cluster.as_deref(), q.rpc.as_deref());
    let result = tokio::task::spawn_blocking(move || Scope::new(url).capture(&signature))
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

/// GET /api — machine-readable index of the public API, so anything that wants to
/// call svmscope (a dApp, wallet, bot, CI job) can discover the surface in one hit.
async fn api_index() -> Json<serde_json::Value> {
    Json(json!({
        "name": "svmscope",
        "description": "Solana transaction simulation layer — decode, replay, mutate, assert.",
        "version": env!("CARGO_PKG_VERSION"),
        // Lets the UI hide the custom-RPC field on instances that don't allow it.
        "custom_rpc": custom_rpc_allowed(),
        "endpoints": {
            "GET  /analyze/{signature}":  "Decode a transaction: CPI tree, balance & token changes, compute, and IDL-decoded accounts.",
            "GET  /replay/{signature}":   "Re-execute the transaction locally against reconstructed pre-state.",
            "POST /simulate":             "{ signature, mutations[], time_travel?, features? } — replay with what-if account mutations, an optional clock warp, and optional runtime feature-gate toggles.",
            "POST /simulate_suite":       "{ signature, scenarios[], time_travel?, features? } — run a suite of scenarios with outcome + state assertions, under optional feature-gate toggles.",
            "POST /preflight":            "{ transaction, mutations[] } — simulate an UNSIGNED transaction against current state before sending.",
            "GET  /freeze/{signature}":   "Capture a self-contained fixture for deterministic, offline replay."
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
fn client_id(req: &Request, peer: Option<SocketAddr>) -> String {
    req.headers()
        .get("x-forwarded-for")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.split(',').next_back())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
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
    } else if path.starts_with("/account") {
        Some("account")
    } else if path.starts_with("/signatures") {
        Some("signatures")
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

#[tokio::main]
async fn main() {
    // Restore any persisted usage tally before serving.
    stats::load();

    // Permissive CORS so any web app can call the API cross-origin — this is what
    // turns the engine from a local binary into infrastructure others build on.
    let cors = tower_http::cors::CorsLayer::permissive();

    let app = Router::new()
        .route("/", get(index))
        .route("/api", get(api_index))
        .route("/analyze/{signature}", get(analyze_handler))
        .route("/simulate", post(simulate_handler))
        .route("/simulate_suite", post(suite_handler))
        .route("/preflight", post(preflight_handler))
        .route("/preflight_report", post(preflight_report_handler))
        .route("/replay_report", post(replay_report_handler))
        .route("/instructions/{program}", get(instructions_handler))
        .route("/idl_instructions", post(idl_instructions_handler))
        .route("/decode_account", post(decode_account_handler))
        .route("/account/{address}", get(account_handler))
        .route("/signatures/{address}", get(signatures_handler))
        .route("/replay/{signature}", get(replay_handler))
        .route("/replay_at_slot/{signature}", get(replay_at_slot_handler))
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
    .await
    .unwrap();
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
        let out = rpc_for(None, Some("http://8.8.8.8:9999/evil"));
        assert_ne!(out, "http://8.8.8.8:9999/evil");
    }

    #[test]
    fn public_instance_rejects_url_and_localnet_clusters() {
        // The second SSRF vector: `cluster` is also caller-controlled, and
        // resolve_rpc honors URL-shaped and localnet clusters verbatim. On a public
        // instance neither may reach an internal target.
        assert!(!custom_rpc_allowed());
        let meta = rpc_for(Some("http://169.254.169.254/latest/meta-data"), None);
        assert!(!meta.contains("169.254"), "url cluster leaked: {meta}");
        let local = rpc_for(Some("localnet"), None);
        assert!(!local.contains("127.0.0.1"), "localnet leaked: {local}");
        // A legitimate public cluster still resolves normally.
        assert!(rpc_for(Some("devnet"), None).starts_with("http"));
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
