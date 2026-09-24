//! Dependency watch, server side: a thread that reads the on-chain registry,
//! keeps the current binary of every watched dependency, and, when one is
//! redeployed, replays each dependent protocol's recent transactions against
//! the new binary and delivers the report to the protocol's alert URL.
//!
//! Configuration (all optional):
//! - `SVMSCOPE_DEPWATCH=0` disables the watcher.
//! - `SVMSCOPE_DEPWATCH_REGISTRY`: the registry program (default: the devnet one).
//! - `SVMSCOPE_DEPWATCH_RPC`: the cluster the registry and its protocols live on
//!   (default: the public devnet endpoint).
//! - `SVMSCOPE_DEPWATCH_INTERVAL_SECS`: poll period (default 120).
//! - `SVMSCOPE_DEPWATCH_MAX_CORPUS`: cap on transactions per check (default 50).
//! - `SVMSCOPE_REPORTER_KEYPAIR`: a keypair (JSON array, file path or base58)
//!   that signs every alert; without it alerts go out unsigned.
//! - `SVMSCOPE_PUBLIC_URL`: this engine's public base URL, used for report links.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use axum::{
    extract::{Path, Query},
    http::{HeaderMap, StatusCode},
    Json,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use svmscope::{
    dependency_watch::{deliver_alert, DEVNET_REGISTRY},
    AlertPayload, Baseline, DependencyReport, Registry, Reporter, Scope,
};

/// A report the watcher produced, kept in memory for the UI.
#[derive(Clone, Serialize)]
pub struct StoredReport {
    pub id: u64,
    pub protocol: String,
    pub alert_url: String,
    /// HTTP status the alert URL answered with, or the delivery error.
    pub delivery: String,
    pub verdict: String,
    pub report: DependencyReport,
}

/// Something POSTed to `/alerts/test`, kept so a demo can show delivery.
#[derive(Clone, Serialize)]
pub struct ReceivedAlert {
    pub received_at: i64,
    pub reporter: Option<String>,
    pub signature: Option<String>,
    pub body: serde_json::Value,
}

const MAX_STORED: usize = 50;

static REPORTS: std::sync::LazyLock<Mutex<Vec<StoredReport>>> =
    std::sync::LazyLock::new(|| Mutex::new(Vec::new()));
static RECEIVED: std::sync::LazyLock<Mutex<Vec<ReceivedAlert>>> =
    std::sync::LazyLock::new(|| Mutex::new(Vec::new()));
/// The binary of every watched dependency as last seen: program id to
/// (deploy slot, bytes). This is what makes a check differential: when the
/// deploy slot moves, the bytes here are the "before".
type HeldBinaries = HashMap<String, (u64, Arc<Vec<u8>>)>;
static BINARIES: std::sync::LazyLock<Mutex<HeldBinaries>> =
    std::sync::LazyLock::new(|| Mutex::new(HashMap::new()));
/// What the watcher is doing, for `/dependency_watch`.
static STATUS: std::sync::LazyLock<Mutex<WatchStatus>> =
    std::sync::LazyLock::new(|| Mutex::new(WatchStatus::default()));

#[derive(Clone, Default, Serialize)]
pub struct WatchStatus {
    pub enabled: bool,
    pub registry: String,
    pub rpc: String,
    pub reporter: Option<String>,
    pub interval_secs: u64,
    pub last_poll_at: Option<i64>,
    pub last_error: Option<String>,
    pub protocols: usize,
    pub dependencies: usize,
    /// Dependency program id to the deploy slot the watcher holds a binary for.
    pub watched: HashMap<String, u64>,
    pub reports: usize,
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn env_or(name: &str, default: &str) -> String {
    std::env::var(name)
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| default.to_string())
}

pub fn registry_program() -> String {
    env_or("SVMSCOPE_DEPWATCH_REGISTRY", DEVNET_REGISTRY)
}

pub fn registry_rpc() -> String {
    env_or("SVMSCOPE_DEPWATCH_RPC", "https://api.devnet.solana.com")
}

fn max_corpus() -> usize {
    env_or("SVMSCOPE_DEPWATCH_MAX_CORPUS", "50")
        .parse()
        .unwrap_or(50)
}

fn public_url() -> Option<String> {
    std::env::var("SVMSCOPE_PUBLIC_URL")
        .ok()
        .map(|u| u.trim_end_matches('/').to_string())
        .filter(|u| !u.is_empty())
}

fn reporter() -> Option<Reporter> {
    std::env::var("SVMSCOPE_REPORTER_KEYPAIR")
        .ok()
        .and_then(|v| Reporter::from_env_value(&v))
}

/// The previous binary the watcher holds for `program`, if any.
pub fn previous_binary(program: &str) -> Option<(u64, Arc<Vec<u8>>)> {
    BINARIES.lock().ok()?.get(program).cloned()
}

fn store_report(r: StoredReport) {
    if let Ok(mut v) = REPORTS.lock() {
        v.insert(0, r);
        v.truncate(MAX_STORED);
    }
}

/// Start the watcher thread. Returns quietly when disabled.
pub fn spawn() {
    if env_or("SVMSCOPE_DEPWATCH", "1") == "0" {
        return;
    }
    let registry = registry_program();
    let rpc = registry_rpc();
    let interval: u64 = env_or("SVMSCOPE_DEPWATCH_INTERVAL_SECS", "120")
        .parse()
        .unwrap_or(120);
    let reporter = reporter();
    if let Ok(mut s) = STATUS.lock() {
        s.enabled = true;
        s.registry = registry.clone();
        s.rpc = rpc.clone();
        s.reporter = reporter.as_ref().map(|r| r.address());
        s.interval_secs = interval;
    }
    std::thread::Builder::new()
        .name("svmscope-depwatch".into())
        .spawn(move || {
            let scope = Scope::new(rpc);
            loop {
                if let Err(e) = poll_once(&scope, &registry, reporter.as_ref()) {
                    eprintln!("depwatch: {e}");
                    if let Ok(mut s) = STATUS.lock() {
                        s.last_error = Some(e);
                    }
                }
                std::thread::sleep(std::time::Duration::from_secs(interval.max(10)));
            }
        })
        .expect("spawn depwatch thread");
}

/// One pass: read the registry, snapshot new dependencies, check the ones
/// whose deploy slot moved.
fn poll_once(scope: &Scope, registry: &str, reporter: Option<&Reporter>) -> Result<(), String> {
    let reg = scope.registry(registry).map_err(|e| e.to_string())?;
    if let Ok(mut s) = STATUS.lock() {
        s.last_poll_at = Some(now());
        s.last_error = None;
        s.protocols = reg.protocols.len();
        s.dependencies = reg.dependencies.len();
    }
    let mut programs: Vec<String> = reg
        .dependencies
        .iter()
        .map(|d| d.program_id.clone())
        .collect();
    programs.sort();
    programs.dedup();

    for program in programs {
        let Some(deploy) = scope.deploy_info(&program).map_err(|e| e.to_string())? else {
            // Not upgradeable: nothing can change, nothing to hold.
            continue;
        };
        let held = previous_binary(&program);
        match held {
            None => snapshot(scope, &program, deploy.last_deploy_slot)?,
            Some((slot, old)) if slot != deploy.last_deploy_slot => {
                eprintln!(
                    "depwatch: {program} redeployed at slot {} (held {slot}); checking dependents",
                    deploy.last_deploy_slot
                );
                run_checks(scope, &reg, &program, old, reporter);
                snapshot(scope, &program, deploy.last_deploy_slot)?;
            }
            Some(_) => {}
        }
    }
    Ok(())
}

fn snapshot(scope: &Scope, program: &str, slot: u64) -> Result<(), String> {
    let elf = scope.program_elf(program).map_err(|e| e.to_string())?;
    if let Ok(mut b) = BINARIES.lock() {
        b.insert(program.to_string(), (slot, Arc::new(elf)));
    }
    if let Ok(mut s) = STATUS.lock() {
        s.watched.insert(program.to_string(), slot);
    }
    Ok(())
}

/// Check every protocol that depends on `program` with alerts on, and
/// deliver each report.
fn run_checks(
    scope: &Scope,
    reg: &Registry,
    program: &str,
    previous: Arc<Vec<u8>>,
    reporter: Option<&Reporter>,
) {
    for dep in reg
        .dependencies
        .iter()
        .filter(|d| d.program_id == program && d.alerts_enabled)
    {
        let Some(protocol) = reg.protocols.iter().find(|p| p.address == dep.protocol) else {
            continue;
        };
        let limit = (protocol.corpus_size as usize).clamp(1, max_corpus());
        let report = match scope.dependency_check(
            &protocol.program_id,
            program,
            limit,
            Baseline::Previous((*previous).clone()),
        ) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("depwatch: check for {} failed: {e}", protocol.program_id);
                continue;
            }
        };
        let id = now() as u64 * 1000 + (REPORTS.lock().map(|v| v.len()).unwrap_or(0) as u64);
        let payload = AlertPayload {
            kind: "dependency_upgraded",
            protocol: protocol.address.clone(),
            verdict: report.verdict(),
            report: report.clone(),
            report_url: public_url().map(|u| format!("{u}/dependency_reports/{id}")),
        };
        let delivery = if protocol.alert_url.starts_with("http") {
            match deliver_alert(&protocol.alert_url, &payload, reporter) {
                Ok(status) => format!("HTTP {status}"),
                Err(e) => e.to_string(),
            }
        } else {
            "no alert url".to_string()
        };
        eprintln!(
            "depwatch: {} vs {}: {} -> {} ({delivery})",
            protocol.program_id, program, payload.verdict, protocol.alert_url
        );
        store_report(StoredReport {
            id,
            protocol: protocol.address.clone(),
            alert_url: protocol.alert_url.clone(),
            delivery,
            verdict: payload.verdict,
            report,
        });
    }
}

// ---- routes ---------------------------------------------------------------

#[derive(Deserialize)]
pub struct RegistryQuery {
    pub registry: Option<String>,
}

/// GET /registry — every protocol and dependency in the registry.
pub async fn registry_handler(
    Query(q): Query<RegistryQuery>,
) -> Result<Json<Registry>, (StatusCode, String)> {
    let registry = q.registry.unwrap_or_else(registry_program);
    let rpc = registry_rpc();
    tokio::task::spawn_blocking(move || Scope::new(rpc).registry(&registry))
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("task error: {e}"),
            )
        })?
        .map(Json)
        .map_err(|e| (StatusCode::BAD_GATEWAY, e.to_string()))
}

#[derive(Deserialize)]
pub struct CheckQuery {
    pub dependency: String,
    pub limit: Option<usize>,
    /// `previous` (default when the watcher holds one), `at_slot`, or `current`.
    pub baseline: Option<String>,
}

/// GET /dependency_check/{program}?dependency=… — run a check now against
/// the registry's cluster.
pub async fn check_handler(
    Path(program): Path<String>,
    Query(q): Query<CheckQuery>,
) -> Result<Json<DependencyReport>, (StatusCode, String)> {
    let limit = q.limit.unwrap_or(20).clamp(1, max_corpus());
    let held = previous_binary(&q.dependency);
    let baseline = match q.baseline.as_deref() {
        Some("at_slot") => Baseline::AtSlot,
        Some("current") => Baseline::Current,
        Some("previous") => match held {
            Some((_, elf)) => Baseline::Previous((*elf).clone()),
            None => {
                return Err((
                    StatusCode::CONFLICT,
                    format!("the watcher holds no previous binary for {}", q.dependency),
                ))
            }
        },
        _ => match held {
            Some((_, elf)) => Baseline::Previous((*elf).clone()),
            None => Baseline::Current,
        },
    };
    let rpc = registry_rpc();
    let dependency = q.dependency.clone();
    tokio::task::spawn_blocking(move || {
        Scope::new(rpc).dependency_check(&program, &dependency, limit, baseline)
    })
    .await
    .map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("task error: {e}"),
        )
    })?
    .map(Json)
    .map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))
}

/// GET /dependency_watch — the watcher's status.
pub async fn status_handler() -> Json<WatchStatus> {
    let mut s = STATUS.lock().map(|s| s.clone()).unwrap_or_default();
    s.reports = REPORTS.lock().map(|v| v.len()).unwrap_or(0);
    Json(s)
}

/// GET /dependency_reports — reports the watcher produced, newest first,
/// without their transaction lists.
pub async fn reports_handler() -> Json<serde_json::Value> {
    let list: Vec<serde_json::Value> = REPORTS
        .lock()
        .map(|v| {
            v.iter()
                .map(|r| {
                    json!({
                        "id": r.id,
                        "protocol": r.protocol,
                        "program_id": r.report.program_id,
                        "dependency": r.report.dependency,
                        "generated_at": r.report.generated_at,
                        "verdict": r.verdict,
                        "summary": r.report.summary,
                        "delivery": r.delivery,
                        "alert_url": r.alert_url,
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    Json(json!(list))
}

/// GET /dependency_reports/{id} — one full report.
pub async fn report_handler(Path(id): Path<u64>) -> Result<Json<StoredReport>, StatusCode> {
    REPORTS
        .lock()
        .ok()
        .and_then(|v| v.iter().find(|r| r.id == id).cloned())
        .map(Json)
        .ok_or(StatusCode::NOT_FOUND)
}

/// POST /alerts/test — a sink any protocol can point its alert URL at while
/// trying the feature; keeps the last few payloads with their signature.
pub async fn alert_sink_handler(
    headers: HeaderMap,
    Json(body): Json<serde_json::Value>,
) -> Json<serde_json::Value> {
    let header = |name: &str| {
        headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string())
    };
    let received = ReceivedAlert {
        received_at: now(),
        reporter: header("x-svmscope-reporter"),
        signature: header("x-svmscope-signature"),
        body,
    };
    if let Ok(mut v) = RECEIVED.lock() {
        v.insert(0, received);
        v.truncate(20);
    }
    Json(json!({ "ok": true }))
}

/// GET /alerts/test — what the sink has received.
pub async fn alert_sink_list() -> Json<Vec<ReceivedAlert>> {
    Json(RECEIVED.lock().map(|v| v.clone()).unwrap_or_default())
}
