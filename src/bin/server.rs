//! svmscope web server — serves the frontend and a JSON analysis endpoint.
//!
//! Run with `cargo run --bin server`, then open http://127.0.0.1:3000.

use axum::{
    extract::Path,
    http::StatusCode,
    response::Html,
    routing::{get, post},
    Json, Router,
};
use serde::Deserialize;
use serde_json::json;
use solana_client::rpc_client::RpcClient;
use svmscope::api::{MutationInput, SuiteRequest};
use svmscope::replay::{Mutation, ReplayResult, ScenarioOutcome};
use svmscope::Analysis;

const RPC_URL: &str = "https://api.mainnet-beta.solana.com";

/// POST body for /simulate.
#[derive(Deserialize)]
struct SimRequest {
    signature: String,
    mutations: Vec<MutationInput>,
}

/// Serve the static frontend page.
async fn index() -> Html<&'static str> {
    Html(include_str!("../../static/index.html"))
}

/// GET /analyze/:signature — decode + replay a transaction, return JSON.
async fn analyze_handler(
    Path(signature): Path<String>,
) -> Result<Json<Analysis>, (StatusCode, String)> {
    // `analyze` does blocking I/O (RPC) and heavy CPU work (replay), so run it on
    // the blocking thread pool instead of stalling the async runtime.
    let result = tokio::task::spawn_blocking(move || {
        let client = RpcClient::new(RPC_URL.to_string());
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

    let result = tokio::task::spawn_blocking(move || {
        let client = RpcClient::new(RPC_URL.to_string());
        svmscope::simulate(&client, &req.signature, &mutations)
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
    let signature = req
        .signature
        .clone()
        .ok_or((StatusCode::BAD_REQUEST, "signature is required".to_string()))?;
    let scenarios = req
        .scenarios
        .into_iter()
        .map(|s| s.into_spec())
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| (StatusCode::BAD_REQUEST, e))?;

    let result = tokio::task::spawn_blocking(move || {
        let client = RpcClient::new(RPC_URL.to_string());
        svmscope::simulate_suite(&client, &signature, scenarios)
    })
    .await
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("task error: {e}")))?;

    match result {
        Ok(outcomes) => Ok(Json(outcomes)),
        Err(msg) => Err((StatusCode::BAD_REQUEST, msg)),
    }
}

/// GET /replay/:signature — run the local replay on demand (analyze skips it).
async fn replay_handler(
    Path(signature): Path<String>,
) -> Result<Json<ReplayResult>, (StatusCode, String)> {
    let result = tokio::task::spawn_blocking(move || {
        let client = RpcClient::new(RPC_URL.to_string());
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
) -> Result<Json<svmscope::fixture::Fixture>, (StatusCode, String)> {
    let result = tokio::task::spawn_blocking(move || {
        let client = RpcClient::new(RPC_URL.to_string());
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
            "POST /simulate":             "{ signature, mutations[] } — replay with what-if account mutations.",
            "POST /simulate_suite":       "{ signature, scenarios[] } — run a suite of scenarios with outcome + state assertions.",
            "GET  /freeze/{signature}":   "Capture a self-contained fixture for deterministic, offline replay."
        }
    }))
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
        .route("/replay/{signature}", get(replay_handler))
        .route("/freeze/{signature}", get(freeze_handler))
        .layer(cors);

    let addr = "127.0.0.1:3000";
    let listener = match tokio::net::TcpListener::bind(addr).await {
        Ok(l) => l,
        Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {
            eprintln!("svmscope: port 3000 is already in use — is a server already running?");
            eprintln!("  (stop it with:  lsof -ti:3000 | xargs kill )");
            std::process::exit(1);
        }
        Err(e) => {
            eprintln!("svmscope: could not bind {addr}: {e}");
            std::process::exit(1);
        }
    };
    println!("svmscope → http://{addr}   (API index: http://{addr}/api)");
    axum::serve(listener, app).await.unwrap();
}
