//! Dependency watch, server side: a thread that reads the on-chain registry,
//! keeps the current binary of every watched dependency, and, when one is
//! redeployed, replays each dependent protocol's recent transactions against
//! the new binary and delivers the report to the protocol's alert URL.
//!
//! Configuration (all optional):
//! - `SVMSCOPE_DEPWATCH=0` disables the watcher.
//! - `SVMSCOPE_DEPWATCH_REGISTRY`: the registry program (default: the devnet one).
//! - `SVMSCOPE_DEPWATCH_RPC`: the cluster the registry lives on (default: the
//!   public devnet endpoint).
//! - `SVMSCOPE_DEPWATCH_CHECK_RPC`: where checks run for programs that live
//!   there (default: `SVMSCOPE_RPC_URL`, else public mainnet). A protocol is
//!   checked on whichever of the two clusters its registered authority holds
//!   the program's upgrade authority; one that holds it nowhere is ignored.
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
    /// The cluster the check ran on.
    pub cluster: String,
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
    /// Where checks run for programs that live there.
    pub check_rpc: String,
    pub reporter: Option<String>,
    pub interval_secs: u64,
    pub last_poll_at: Option<i64>,
    pub last_error: Option<String>,
    pub protocols: usize,
    pub dependencies: usize,
    /// `cluster:program` to the deploy slot the watcher holds a binary for.
    pub watched: HashMap<String, u64>,
    /// Protocols the watcher could not verify on any cluster.
    pub unverified: Vec<String>,
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

/// The cluster checks run on for programs that live there: mainnet by
/// default, through the engine's own RPC. The registry may sit on another
/// cluster; a protocol is checked wherever its registered authority really
/// holds the program.
pub fn check_rpc() -> String {
    std::env::var("SVMSCOPE_DEPWATCH_CHECK_RPC")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .or_else(|| {
            std::env::var("SVMSCOPE_RPC_URL")
                .ok()
                .filter(|v| !v.trim().is_empty())
        })
        .unwrap_or_else(|| "https://api.mainnet-beta.solana.com".to_string())
}

/// An RPC URL with its query string and userinfo removed: keys ride in
/// both, and the status endpoint is public.
pub fn redact_url(url: &str) -> String {
    let no_query = url.split(['?', '#']).next().unwrap_or(url);
    match no_query.split_once("://") {
        Some((scheme, rest)) => {
            let host = rest.split('/').next().unwrap_or(rest);
            let host = host.rsplit('@').next().unwrap_or(host);
            format!("{scheme}://{host}")
        }
        None => no_query.to_string(),
    }
}

/// A short name for a cluster URL, for keys and the page.
pub fn cluster_label(url: &str) -> &'static str {
    let u = url.to_ascii_lowercase();
    if u.contains("devnet") {
        "devnet"
    } else if u.contains("testnet") {
        "testnet"
    } else if u.contains("localhost") || u.contains("127.0.0.1") {
        "localnet"
    } else {
        "mainnet"
    }
}

/// The RPC for a cluster name the page may send.
pub fn rpc_for_label(label: &str) -> Option<String> {
    let check = check_rpc();
    let registry = registry_rpc();
    if label == cluster_label(&check) {
        Some(check)
    } else if label == cluster_label(&registry) {
        Some(registry)
    } else {
        None
    }
}

/// A scope for checks on `url`: the account-changes stream and the record
/// store are attached for the mainnet endpoint, so an earlier binary of a
/// dependency can be fetched; other clusters get a plain scope.
pub fn check_scope(url: String) -> Scope {
    let mainnet = cluster_label(&url) == "mainnet";
    let scope = Scope::new(url);
    if !mainnet {
        return scope;
    }
    let scope = match svmscope::history::HistoryStream::from_env() {
        Some(stream) => scope.with_history_stream(stream),
        None => scope,
    };
    match crate::RECORDS.as_ref() {
        Some(store) => {
            let dynamic: Arc<dyn svmscope::records::StateStore> =
                Arc::<svmscope::records::LogStore>::clone(store);
            scope.with_records(dynamic)
        }
        None => scope,
    }
}

/// The clusters a protocol may live on, check cluster first, without
/// repeating a URL.
fn candidate_clusters() -> Vec<(String, &'static str)> {
    let mut out: Vec<(String, &'static str)> = Vec::new();
    for url in [check_rpc(), registry_rpc()] {
        if !out.iter().any(|(u, _)| *u == url) {
            let label = cluster_label(&url);
            out.push((url, label));
        }
    }
    out
}

/// Mark every protocol with the cluster on which its registered authority
/// holds the program's upgrade authority, if any. A protocol nobody can
/// vouch for is left `verified: false` and gets no checks and no alerts.
pub fn verify_protocols(reg: &mut Registry) {
    let clusters: Vec<(Scope, &'static str)> = candidate_clusters()
        .into_iter()
        .map(|(url, label)| (Scope::new(url), label))
        .collect();
    for p in reg.protocols.iter_mut() {
        p.verified = Some(false);
        p.cluster = None;
        for (scope, label) in &clusters {
            match scope.holds_upgrade_authority(&p.program_id, &p.authority) {
                Ok(true) => {
                    p.verified = Some(true);
                    p.cluster = Some((*label).to_string());
                    break;
                }
                Ok(false) => {}
                Err(e) => eprintln!(
                    "depwatch: could not verify {} on {label}: {e}",
                    p.program_id
                ),
            }
        }
    }
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

fn held_key(cluster: &str, program: &str) -> String {
    format!("{cluster}:{program}")
}

/// The previous binary the watcher holds for `program` on `cluster`, if any.
pub fn previous_binary(cluster: &str, program: &str) -> Option<(u64, Arc<Vec<u8>>)> {
    BINARIES
        .lock()
        .ok()?
        .get(&held_key(cluster, program))
        .cloned()
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
        s.rpc = redact_url(&rpc);
        s.check_rpc = redact_url(&check_rpc());
        s.reporter = reporter.as_ref().map(|r| r.address());
        s.interval_secs = interval;
    }
    std::thread::Builder::new()
        .name("svmscope-depwatch".into())
        .spawn(move || {
            let registry_scope = Scope::new(rpc);
            loop {
                if let Err(e) = poll_once(&registry_scope, &registry, reporter.as_ref()) {
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

/// One pass: read the registry, place every protocol on the cluster where
/// its authority holds the program, snapshot new dependencies there, and
/// check the ones whose deploy slot moved.
fn poll_once(
    registry_scope: &Scope,
    registry: &str,
    reporter: Option<&Reporter>,
) -> Result<(), String> {
    let mut reg = registry_scope
        .registry(registry)
        .map_err(|e| e.to_string())?;
    verify_protocols(&mut reg);
    if let Ok(mut s) = STATUS.lock() {
        s.last_poll_at = Some(now());
        s.last_error = None;
        s.protocols = reg.protocols.len();
        s.dependencies = reg.dependencies.len();
        s.unverified = reg
            .protocols
            .iter()
            .filter(|p| p.verified != Some(true))
            .map(|p| p.program_id.clone())
            .collect();
    }

    // Dependencies are watched per cluster: a protocol on mainnet needs the
    // mainnet bytes of its dependencies, one on devnet the devnet bytes.
    for (url, label) in candidate_clusters() {
        let scope = check_scope(url);
        let mut programs: Vec<String> = reg
            .dependencies
            .iter()
            .filter(|d| {
                reg.protocols
                    .iter()
                    .any(|p| p.address == d.protocol && p.cluster.as_deref() == Some(label))
            })
            .map(|d| d.program_id.clone())
            .collect();
        programs.sort();
        programs.dedup();

        for program in programs {
            let Some(deploy) = scope.deploy_info(&program).map_err(|e| e.to_string())? else {
                // Not upgradeable: nothing can change, nothing to hold.
                continue;
            };
            match previous_binary(label, &program) {
                None => snapshot(&scope, label, &program, deploy.last_deploy_slot)?,
                Some((slot, old)) if slot != deploy.last_deploy_slot => {
                    eprintln!(
                        "depwatch: {program} redeployed on {label} at slot {} (held {slot}); checking dependents",
                        deploy.last_deploy_slot
                    );
                    run_checks(&scope, label, &reg, &program, old, reporter);
                    snapshot(&scope, label, &program, deploy.last_deploy_slot)?;
                }
                Some(_) => {}
            }
        }
    }
    Ok(())
}

fn snapshot(scope: &Scope, cluster: &str, program: &str, slot: u64) -> Result<(), String> {
    let elf = scope.program_elf(program).map_err(|e| e.to_string())?;
    let key = held_key(cluster, program);
    if let Ok(mut b) = BINARIES.lock() {
        b.insert(key.clone(), (slot, Arc::new(elf)));
    }
    if let Ok(mut s) = STATUS.lock() {
        s.watched.insert(key, slot);
    }
    Ok(())
}

/// Check every verified protocol on `cluster` that depends on `program`
/// with alerts on, and deliver each report.
fn run_checks(
    scope: &Scope,
    cluster: &str,
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
        let Some(protocol) = reg.protocols.iter().find(|p| {
            p.address == dep.protocol
                && p.verified == Some(true)
                && p.cluster.as_deref() == Some(cluster)
        }) else {
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
            "depwatch: {} vs {} on {cluster}: {} -> {} ({delivery})",
            protocol.program_id, program, payload.verdict, protocol.alert_url
        );
        store_report(StoredReport {
            id,
            protocol: protocol.address.clone(),
            cluster: cluster.to_string(),
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

/// GET /registry — every protocol and dependency in the registry, each
/// protocol marked with whether and where the engine could verify it.
pub async fn registry_handler(
    Query(q): Query<RegistryQuery>,
) -> Result<Json<Registry>, (StatusCode, String)> {
    let registry = q.registry.unwrap_or_else(registry_program);
    let rpc = registry_rpc();
    tokio::task::spawn_blocking(move || {
        let mut reg = Scope::new(rpc).registry(&registry)?;
        verify_protocols(&mut reg);
        Ok::<_, svmscope::Error>(reg)
    })
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
    /// `previous` (the watcher's held binary), `before_deploy` (the binary
    /// before the current deploy, from the stream), `at_slot`, or `current`.
    /// Default: `previous` when held, else `before_deploy` when the stream
    /// can supply it, else `current`.
    pub baseline: Option<String>,
    /// `mainnet` or `devnet` (whatever the two configured clusters are);
    /// default is the check cluster.
    pub cluster: Option<String>,
}

/// GET /dependency_check/{program}?dependency=…&cluster=… — run a check now.
pub async fn check_handler(
    Path(program): Path<String>,
    Query(q): Query<CheckQuery>,
) -> Result<Json<DependencyReport>, (StatusCode, String)> {
    let limit = q.limit.unwrap_or(20).clamp(1, max_corpus());
    let rpc = match q.cluster.as_deref() {
        None => check_rpc(),
        Some(label) => rpc_for_label(label).ok_or_else(|| {
            (
                StatusCode::BAD_REQUEST,
                format!(
                    "cluster must be {} or {}",
                    cluster_label(&check_rpc()),
                    cluster_label(&registry_rpc())
                ),
            )
        })?,
    };
    let label = cluster_label(&rpc);
    let held = previous_binary(label, &q.dependency);
    let baseline = match q.baseline.as_deref() {
        Some("at_slot") => Baseline::AtSlot,
        Some("current") => Baseline::Current,
        Some("before_deploy") => Baseline::BeforeDeploy,
        Some("previous") => match held {
            Some((_, elf)) => Baseline::Previous((*elf).clone()),
            None => {
                return Err((
                    StatusCode::CONFLICT,
                    format!(
                        "the watcher holds no previous binary for {} on {label}",
                        q.dependency
                    ),
                ))
            }
        },
        _ => match held {
            Some((_, elf)) => Baseline::Previous((*elf).clone()),
            None => Baseline::BeforeDeploy,
        },
    };
    let explicit = q.baseline.is_some();
    let dependency = q.dependency.clone();
    tokio::task::spawn_blocking(move || {
        let scope = check_scope(rpc);
        let first = scope.dependency_check(&program, &dependency, limit, baseline.clone());
        // Without an explicit choice, a missing earlier binary is not an
        // error: fall back to the current one and let the report say so.
        match first {
            Err(svmscope::Error::InvalidSpec(_))
                if !explicit && matches!(baseline, Baseline::BeforeDeploy) =>
            {
                scope.dependency_check(&program, &dependency, limit, Baseline::Current)
            }
            other => other,
        }
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
                        "cluster": r.cluster,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redaction_drops_keys_and_paths() {
        assert_eq!(
            redact_url("https://mainnet.helius-rpc.com/?api-key=SECRET"),
            "https://mainnet.helius-rpc.com"
        );
        assert_eq!(
            redact_url("https://user:pw@rpc.example.com/v1/SECRET#frag"),
            "https://rpc.example.com"
        );
        assert_eq!(redact_url("http://127.0.0.1:8899"), "http://127.0.0.1:8899");
        assert_eq!(cluster_label("https://api.devnet.solana.com"), "devnet");
        assert_eq!(
            cluster_label("https://mainnet.helius-rpc.com/?api-key=x"),
            "mainnet"
        );
    }
}
