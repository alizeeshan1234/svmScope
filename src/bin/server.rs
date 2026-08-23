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
use solana_client::rpc_client::RpcClient;
use svmscope::replay::{Mutation, ReplayResult};
use svmscope::Analysis;

const RPC_URL: &str = "https://api.mainnet-beta.solana.com";

/// POST body for /simulate.
#[derive(Deserialize)]
struct SimRequest {
    signature: String,
    mutations: Vec<MutationInput>,
}

/// One what-if mutation from the UI. `kind` picks the variant:
/// `{"kind":"lamports","address":..,"lamports":..}` or
/// `{"kind":"data","address":..,"offset":..,"bytes_hex":".."}` (patch at offset).
#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
enum MutationInput {
    Lamports {
        address: String,
        lamports: u64,
    },
    Data {
        address: String,
        offset: usize,
        bytes_hex: String,
    },
}

fn hex_decode(s: &str) -> Result<Vec<u8>, String> {
    let s = s.trim().trim_start_matches("0x").replace([' ', '_'], "");
    if s.is_empty() || s.len() % 2 != 0 {
        return Err("hex bytes must be a non-empty, even-length hex string".into());
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).map_err(|_| format!("invalid hex: {s}")))
        .collect()
}

impl MutationInput {
    fn into_mutation(self) -> Result<Mutation, String> {
        Ok(match self {
            MutationInput::Lamports { address, lamports } => Mutation::Lamports {
                address,
                value: lamports,
            },
            MutationInput::Data {
                address,
                offset,
                bytes_hex,
            } => Mutation::DataPatch {
                address,
                offset,
                bytes: hex_decode(&bytes_hex)?,
            },
        })
    }
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

#[tokio::main]
async fn main() {
    let app = Router::new()
        .route("/", get(index))
        .route("/analyze/{signature}", get(analyze_handler))
        .route("/simulate", post(simulate_handler));

    let listener = tokio::net::TcpListener::bind("127.0.0.1:3000")
        .await
        .unwrap();
    println!("svmscope server → http://127.0.0.1:3000");
    axum::serve(listener, app).await.unwrap();
}
