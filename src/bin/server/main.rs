//! svmscope web server — serves the frontend and a JSON analysis endpoint.
//!
//! Run with `cargo run --bin server`, then open http://127.0.0.1:3000.

mod guard;

use axum::{
    body::Body,
    extract::{ConnectInfo, Path, Query, Request},
    http::{header, StatusCode},
    middleware::{self, Next},
    response::{Html, IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use std::net::SocketAddr;
use serde::Deserialize;
use serde_json::json;
use solana_client::rpc_client::RpcClient;
use svmscope::api::{MutationInput, SuiteRequest};
use svmscope::replay::{Mutation, ReplayResult, ScenarioOutcome};
use svmscope::Analysis;

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

/// Resolve a per-request RPC. Precedence: explicit ?rpc= > per-cluster env var >
/// cluster's public endpoint > the generic env default.
fn rpc_for(cluster: Option<&str>, rpc: Option<&str>) -> String {
    if let Some(u) = rpc {
        if u.starts_with("http") {
            return u.to_string();
        }
    }
    if let Some(u) = cluster_env_rpc(cluster) {
        return u;
    }
    svmscope::resolve_rpc(cluster, rpc, &rpc_url())
}

/// POST body for /simulate.
#[derive(Deserialize)]
struct SimRequest {
    signature: String,
    mutations: Vec<MutationInput>,
    /// Optional clock warp — test time-gated logic without waiting.
    #[serde(default)]
    time_travel: svmscope::replay::TimeTravel,
    #[serde(default)]
    cluster: Option<String>,
    #[serde(default)]
    rpc: Option<String>,
}

/// Serve the static frontend page.
async fn index() -> Html<&'static str> {
    Html(include_str!("../../../static/index.html"))
}

/// GET /analyze/:signature — decode + replay a transaction, return JSON.
async fn analyze_handler(
    Path(signature): Path<String>,
    Query(q): Query<ClusterQuery>,
) -> Result<Json<Analysis>, (StatusCode, String)> {
    let url = rpc_for(q.cluster.as_deref(), q.rpc.as_deref());
    // `analyze` does blocking I/O (RPC) and heavy CPU work (replay), so run it on
    // the blocking thread pool instead of stalling the async runtime.
    let result = tokio::task::spawn_blocking(move || {
        let client = RpcClient::new(url);
        svmscope::analyze(&client, &signature)
    })
    .await
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("task error: {e}")))?;

    match result {
        Ok(analysis) => Ok(Json(analysis)),
        Err(msg) => Err((StatusCode::BAD_REQUEST, msg)),
    }
}

/// POST /simulate — apply what-if mutations and return the mutated replay result.
async fn simulate_handler(
    Json(req): Json<SimRequest>,
) -> Result<Json<ReplayResult>, (StatusCode, String)> {
    let mutations: Vec<Mutation> = req
        .mutations
        .into_iter()
        .map(MutationInput::into_mutation)
        .collect::<Result<_, _>>()
        .map_err(|e| (StatusCode::BAD_REQUEST, e))?;

    let url = rpc_for(req.cluster.as_deref(), req.rpc.as_deref());
    let result = tokio::task::spawn_blocking(move || {
        let client = RpcClient::new(url);
        svmscope::simulate(&client, &req.signature, &mutations, req.time_travel)
    })
    .await
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("task error: {e}")))?;

    match result {
        Ok(replay) => Ok(Json(replay)),
        Err(msg) => Err((StatusCode::BAD_REQUEST, msg)),
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
            "fixture suites run locally: `svmscope test suite.json`. The API needs a `signature`.".to_string(),
        ));
    }
    let signature = req
        .signature
        .clone()
        .ok_or((StatusCode::BAD_REQUEST, "signature is required".to_string()))?;
    let url = rpc_for(req.cluster.as_deref(), req.rpc.as_deref());
    let scenarios = req
        .scenarios
        .into_iter()
        .map(|s| s.into_spec())
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| (StatusCode::BAD_REQUEST, e))?;

    let result = tokio::task::spawn_blocking(move || {
        let client = RpcClient::new(url);
        svmscope::simulate_suite(&client, &signature, scenarios, req.time_travel)
    })
    .await
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("task error: {e}")))?;

    match result {
        Ok(outcomes) => Ok(Json(outcomes)),
        Err(msg) => Err((StatusCode::BAD_REQUEST, msg)),
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
    time_travel: svmscope::replay::TimeTravel,
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
    let mutations: Vec<Mutation> = req
        .mutations
        .into_iter()
        .map(MutationInput::into_mutation)
        .collect::<Result<_, _>>()
        .map_err(|e| (StatusCode::BAD_REQUEST, e))?;

    let url = rpc_for(req.cluster.as_deref(), req.rpc.as_deref());
    let result = tokio::task::spawn_blocking(move || {
        let client = RpcClient::new(url);
        svmscope::simulate_preflight(&client, &req.transaction, &mutations)
    })
    .await
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("task error: {e}")))?;

    match result {
        Ok(r) => Ok(Json(r)),
        Err(msg) => Err((StatusCode::BAD_REQUEST, msg)),
    }
}

/// GET /account/:address — explorer-style overview of an account or program.
async fn account_handler(
    Path(address): Path<String>,
    Query(q): Query<ClusterQuery>,
) -> Result<Json<svmscope::AccountOverview>, (StatusCode, String)> {
    let url = rpc_for(q.cluster.as_deref(), q.rpc.as_deref());
    let result = tokio::task::spawn_blocking(move || {
        let client = RpcClient::new(url);
        svmscope::account_overview(&client, &address)
    })
    .await
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("task error: {e}")))?;

    match result {
        Ok(ov) => Ok(Json(ov)),
        Err(msg) => Err((StatusCode::BAD_REQUEST, msg)),
    }
}

/// GET /signatures/:address — recent transactions for an account/program (explorer-style).
async fn signatures_handler(
    Path(address): Path<String>,
    Query(q): Query<ClusterQuery>,
) -> Result<Json<Vec<svmscope::SigInfo>>, (StatusCode, String)> {
    let url = rpc_for(q.cluster.as_deref(), q.rpc.as_deref());
    let result = tokio::task::spawn_blocking(move || {
        let client = RpcClient::new(url);
        svmscope::recent_signatures(&client, &address, 25)
    })
    .await
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("task error: {e}")))?;

    match result {
        Ok(sigs) => Ok(Json(sigs)),
        Err(msg) => Err((StatusCode::BAD_REQUEST, msg)),
    }
}

/// POST /preflight_report — simulate an unsigned tx and return the full developer
/// report: outcome, human-readable failure reason, and the account diff.
async fn preflight_report_handler(
    Json(req): Json<PreflightRequest>,
) -> Result<Json<svmscope::SimulationReport>, (StatusCode, String)> {
    let mutations: Vec<Mutation> = req
        .mutations
        .into_iter()
        .map(MutationInput::into_mutation)
        .collect::<Result<_, _>>()
        .map_err(|e| (StatusCode::BAD_REQUEST, e))?;

    let url = rpc_for(req.cluster.as_deref(), req.rpc.as_deref());
    let tt = req.time_travel.clone();
    let result = tokio::task::spawn_blocking(move || {
        let client = RpcClient::new(url);
        svmscope::preflight_report(&client, &req.transaction, &mutations, tt)
    })
    .await
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("task error: {e}")))?;

    result.map(Json).map_err(|m| (StatusCode::BAD_REQUEST, m))
}

/// POST /replay_report — replay a landed tx (optionally mutated) with explanation
/// and account diff.
async fn replay_report_handler(
    Json(req): Json<SimRequest>,
) -> Result<Json<svmscope::SimulationReport>, (StatusCode, String)> {
    let mutations: Vec<Mutation> = req
        .mutations
        .into_iter()
        .map(MutationInput::into_mutation)
        .collect::<Result<_, _>>()
        .map_err(|e| (StatusCode::BAD_REQUEST, e))?;

    let url = rpc_for(req.cluster.as_deref(), req.rpc.as_deref());
    let tt = req.time_travel.clone();
    let result = tokio::task::spawn_blocking(move || {
        let client = RpcClient::new(url);
        svmscope::replay_report(&client, &req.signature, &mutations, tt)
    })
    .await
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("task error: {e}")))?;

    result.map(Json).map_err(|m| (StatusCode::BAD_REQUEST, m))
}

/// POST body for IDL-assisted decoding / instruction listing.
#[derive(Deserialize)]
struct IdlRequest {
    /// Account address (for /decode_account) or program id (for /idl_instructions).
    #[serde(default)]
    address: Option<String>,
    /// The IDL JSON, e.g. the contents of target/idl/<program>.json.
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
) -> Result<Json<svmscope::decode::AccountInfo>, (StatusCode, String)> {
    let address = req
        .address
        .clone()
        .ok_or((StatusCode::BAD_REQUEST, "address is required".to_string()))?;
    let url = rpc_for(req.cluster.as_deref(), req.rpc.as_deref());
    let idl = (!req.idl.is_null()).then_some(req.idl);

    let result = tokio::task::spawn_blocking(move || {
        let client = RpcClient::new(url);
        svmscope::decode_account_with(&client, &address, idl.as_ref())
    })
    .await
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("task error: {e}")))?;

    result.map(Json).map_err(|m| (StatusCode::BAD_REQUEST, m))
}

/// POST /idl_instructions — instructions from a supplied IDL (no on-chain publish needed).
async fn idl_instructions_handler(
    Json(req): Json<IdlRequest>,
) -> Json<Vec<svmscope::idl::IdlInstruction>> {
    Json(svmscope::instructions_from_idl(&req.idl))
}

/// GET /instructions/:program — the instructions a program exposes (from its IDL),
/// for the transaction builder.
async fn instructions_handler(
    Path(program): Path<String>,
    Query(q): Query<ClusterQuery>,
) -> Result<Json<Vec<svmscope::idl::IdlInstruction>>, (StatusCode, String)> {
    let url = rpc_for(q.cluster.as_deref(), q.rpc.as_deref());
    let result = tokio::task::spawn_blocking(move || {
        let client = RpcClient::new(url);
        svmscope::program_instructions(&client, &program)
    })
    .await
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("task error: {e}")))?;

    result.map(Json).map_err(|m| (StatusCode::BAD_REQUEST, m))
}

/// GET /replay/:signature — run the local replay on demand (analyze skips it).
async fn replay_handler(
    Path(signature): Path<String>,
    Query(q): Query<ClusterQuery>,
) -> Result<Json<ReplayResult>, (StatusCode, String)> {
    let url = rpc_for(q.cluster.as_deref(), q.rpc.as_deref());
    let result = tokio::task::spawn_blocking(move || {
        let client = RpcClient::new(url);
        svmscope::run_replay(&client, &signature)
    })
    .await
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("task error: {e}")))?;

    match result {
        Ok(replay) => Ok(Json(replay)),
        Err(msg) => Err((StatusCode::BAD_REQUEST, msg)),
    }
}

/// GET /freeze/:signature — capture a self-contained fixture for offline replay.
async fn freeze_handler(
    Path(signature): Path<String>,
    Query(q): Query<ClusterQuery>,
) -> Result<Json<svmscope::fixture::Fixture>, (StatusCode, String)> {
    let url = rpc_for(q.cluster.as_deref(), q.rpc.as_deref());
    let result = tokio::task::spawn_blocking(move || {
        let client = RpcClient::new(url);
        svmscope::capture_fixture(&client, &signature)
    })
    .await
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("task error: {e}")))?;

    match result {
        Ok(fx) => Ok(Json(fx)),
        Err(msg) => Err((StatusCode::BAD_REQUEST, msg)),
    }
}

/// GET /api — machine-readable index of the public API, so anything that wants to
/// call svmscope (a dApp, wallet, bot, CI job) can discover the surface in one hit.
async fn api_index() -> Json<serde_json::Value> {
    Json(json!({
        "name": "svmscope",
        "description": "Solana transaction simulation layer — decode, replay, mutate, assert.",
        "version": env!("CARGO_PKG_VERSION"),
        "endpoints": {
            "GET  /analyze/{signature}":  "Decode a transaction: CPI tree, balance & token changes, compute, and IDL-decoded accounts.",
            "GET  /replay/{signature}":   "Re-execute the transaction locally against reconstructed pre-state.",
            "POST /simulate":             "{ signature, mutations[], time_travel? } — replay with what-if account mutations, optionally with the clock warped.",
            "POST /simulate_suite":       "{ signature, scenarios[], time_travel? } — run a suite of scenarios with outcome + state assertions.",
            "POST /preflight":            "{ transaction, mutations[] } — simulate an UNSIGNED transaction against current state before sending.",
            "GET  /freeze/{signature}":   "Capture a self-contained fixture for deterministic, offline replay."
        }
    }))
}

/// The client's address, preferring the proxy-forwarded IP since we run behind
/// Render's load balancer (otherwise every request looks like the same peer).
fn client_id(req: &Request, peer: Option<SocketAddr>) -> String {
    req.headers()
        .get("x-forwarded-for")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.split(',').next())
        .map(|s| s.trim().to_string())
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
    let path = req.uri().path();
    let metered = path.starts_with("/analyze")
        || path.starts_with("/replay")
        || path.starts_with("/simulate")
        || path.starts_with("/preflight")
        || path.starts_with("/freeze")
        || path.starts_with("/account")
        || path.starts_with("/signatures");

    if metered {
        if let Err(retry) = guard::rate_check(&client_id(&req, Some(peer))) {
            return (
                StatusCode::TOO_MANY_REQUESTS,
                [(header::RETRY_AFTER, retry.to_string())],
                format!("rate limit reached — try again in {retry}s"),
            )
                .into_response();
        }
    }
    next.run(req).await
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
        Err(_) => return (StatusCode::INTERNAL_SERVER_ERROR, "response read error").into_response(),
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

#[tokio::main]
async fn main() {
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
        .route("/freeze/{signature}", get(freeze_handler))
        // Order matters: rate limit first (cheapest rejection), then serve from
        // cache, then CORS headers on whatever comes back.
        .layer(middleware::from_fn(cache_layer))
        .layer(middleware::from_fn(rate_limit))
        .layer(cors);

    // Host/port from the environment so it runs unchanged locally and on any
    // platform (Fly, Render, Railway, Docker) that injects PORT and expects 0.0.0.0.
    let host = std::env::var("HOST").unwrap_or_else(|_| "0.0.0.0".to_string());
    let port: u16 = std::env::var("PORT").ok().and_then(|p| p.parse().ok()).unwrap_or(3000);
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
    axum::serve(listener, app.into_make_service_with_connect_info::<SocketAddr>())
        .await
        .unwrap();
}
