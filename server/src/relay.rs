//! RPC relayed through the reader's browser.
//!
//! A validator on someone's laptop is at `127.0.0.1`, and this engine runs on a
//! server, where that address means the server's own container. No configuration
//! fixes that: the two machines are simply different. Before this, picking
//! Localnet on the hosted site quietly returned mainnet, which looks exactly like
//! success and is how you end up reading another chain's data.
//!
//! The browser, though, *is* on the reader's machine. So the engine stops trying
//! to reach the validator and asks the page to do it. The page holds a WebSocket
//! open; every RPC call the engine would have made travels down it as an ordinary
//! JSON-RPC request, the page `fetch`es its own validator, and the answer comes
//! back up. To the rest of the engine this is just another [`RpcSender`], so
//! replay, decoding and the certificate work unchanged.
//!
//! This also keeps the engine off the network rather than putting it on: it never
//! connects anywhere, it only asks. There is no URL here for an attacker to aim,
//! so relayed localnet needs none of the SSRF vetting that a caller-supplied
//! `rpc=` does — and that vetting is untouched.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde_json::{json, Value};
use solana_rpc_client::rpc_client::RpcClient;
use solana_rpc_client::rpc_sender::{RpcSender, RpcTransportStats};
use solana_rpc_client_api::client_error::{Error as ClientError, Result as ClientResult};
use solana_rpc_client_api::request::{RpcError, RpcRequest};
use tokio::sync::{mpsc, oneshot};

/// How long the engine waits for the browser to answer one call. A local
/// validator replies in milliseconds; this is the ceiling for a page that has
/// been backgrounded or a laptop that went to sleep mid-replay.
const CALL_TIMEOUT: Duration = Duration::from_secs(30);

/// Sessions the process will hold at once. Each is one open WebSocket and a
/// little bookkeeping, but the count is driven by whoever connects, so it is
/// bounded rather than trusted.
const MAX_SESSIONS: usize = 64;

/// A session with nothing in flight and no request this long is dropped, so a
/// tab closed without a clean disconnect does not hold its slot forever.
const IDLE_TIMEOUT: Duration = Duration::from_secs(30 * 60);

/// One request travelling from the engine down to a browser.
pub struct Call {
    /// The full JSON-RPC envelope, exactly as the HTTP transport would have
    /// posted it. The page forwards it to its validator verbatim.
    envelope: Value,
}

/// One connected page, and the calls it currently owes answers to.
pub struct Session {
    to_browser: mpsc::UnboundedSender<Call>,
    pending: Mutex<HashMap<u64, oneshot::Sender<Value>>>,
    next_id: AtomicU64,
    last_used: Mutex<Instant>,
    /// What the page said it is talking to, for diagnostics only. The engine
    /// never connects to it.
    label: String,
}

impl Session {
    /// Hand one answer back to whoever is waiting for it. Unknown ids are
    /// ignored: a late reply to a call that already timed out is not an error.
    fn resolve(&self, id: u64, payload: Value) {
        let waiting = self.pending.lock().ok().and_then(|mut p| p.remove(&id));
        if let Some(tx) = waiting {
            let _ = tx.send(payload);
        }
    }

    /// Fail every outstanding call. Called when the page goes away, so a replay
    /// mid-flight ends with a clear error instead of waiting out the timeout.
    fn fail_all(&self) {
        let Ok(mut pending) = self.pending.lock() else {
            return;
        };
        for (_, tx) in pending.drain() {
            let _ = tx.send(json!({ "transport_error": "the page relaying RPC disconnected" }));
        }
    }

    fn idle_for(&self) -> Duration {
        self.last_used
            .lock()
            .map(|t| t.elapsed())
            .unwrap_or_default()
    }
}

type Registry = HashMap<String, Arc<Session>>;
static SESSIONS: LazyLock<Mutex<Registry>> = LazyLock::new(|| Mutex::new(HashMap::new()));

/// Register a newly connected page. `None` when the process is already holding
/// as many sessions as it will.
pub fn open(id: String, label: String) -> Option<(Arc<Session>, mpsc::UnboundedReceiver<Call>)> {
    let (tx, rx) = mpsc::unbounded_channel();
    let session = Arc::new(Session {
        to_browser: tx,
        pending: Mutex::new(HashMap::new()),
        next_id: AtomicU64::new(1),
        last_used: Mutex::new(Instant::now()),
        label,
    });
    let mut sessions = SESSIONS.lock().ok()?;
    sessions.retain(|_, s| s.idle_for() < IDLE_TIMEOUT);
    if sessions.len() >= MAX_SESSIONS {
        return None;
    }
    sessions.insert(id, Arc::clone(&session));
    Some((session, rx))
}

/// Drop a session and fail anything it still owed.
pub fn close(id: &str) {
    let removed = SESSIONS.lock().ok().and_then(|mut s| s.remove(id));
    if let Some(session) = removed {
        session.fail_all();
    }
}

fn get(id: &str) -> Option<Arc<Session>> {
    SESSIONS.lock().ok()?.get(id).cloned()
}

/// Whether this id names a page that is still connected.
pub fn is_open(id: &str) -> bool {
    get(id).is_some()
}

/// Route one answer from a page back to the call waiting on it.
pub fn deliver(id: &str, reply: &Value) {
    let Some(session) = get(id) else { return };
    let Some(call_id) = reply.get("id").and_then(Value::as_u64) else {
        return;
    };
    session.resolve(call_id, reply.clone());
}

/// Pull one call off the queue to send to the page.
pub fn envelope_of(call: &Call) -> &Value {
    &call.envelope
}

/// An [`RpcClient`] whose transport is a connected browser, or `None` if that
/// page has since disconnected.
pub fn client_for(id: &str) -> Option<RpcClient> {
    let session = get(id)?;
    Some(RpcClient::new_sender(
        RelaySender { session },
        Default::default(),
    ))
}

/// The transport itself: instead of posting to a URL, it queues the request for
/// a browser and waits for that browser to answer.
struct RelaySender {
    session: Arc<Session>,
}

#[async_trait]
impl RpcSender for RelaySender {
    async fn send(&self, request: RpcRequest, params: Value) -> ClientResult<Value> {
        let id = self.session.next_id.fetch_add(1, Ordering::Relaxed);
        let envelope = request.build_request_json(id, params);

        let (tx, rx) = oneshot::channel();
        {
            let mut pending = self.session.pending.lock().map_err(|_| {
                ClientError::from(RpcError::RpcRequestError(
                    "the RPC relay is unavailable".to_string(),
                ))
            })?;
            pending.insert(id, tx);
        }
        if let Ok(mut t) = self.session.last_used.lock() {
            *t = Instant::now();
        }

        if self.session.to_browser.send(Call { envelope }).is_err() {
            self.session.pending.lock().ok().map(|mut p| p.remove(&id));
            return Err(
                RpcError::RpcRequestError("the page relaying RPC has closed".to_string()).into(),
            );
        }

        let reply = match tokio::time::timeout(CALL_TIMEOUT, rx).await {
            Ok(Ok(v)) => v,
            // Dropped sender: the session was torn down under us.
            Ok(Err(_)) => {
                return Err(RpcError::RpcRequestError(
                    "the page relaying RPC disconnected".to_string(),
                )
                .into())
            }
            Err(_) => {
                self.session.pending.lock().ok().map(|mut p| p.remove(&id));
                return Err(RpcError::RpcRequestError(format!(
                    "the browser did not answer {request} within {}s — is the validator running?",
                    CALL_TIMEOUT.as_secs()
                ))
                .into());
            }
        };

        interpret(reply)
    }

    fn get_transport_stats(&self) -> RpcTransportStats {
        RpcTransportStats::default()
    }

    fn url(&self) -> String {
        self.session.label.clone()
    }
}

/// Turn one browser reply into what the RPC client expects.
///
/// The page reports its own failures (the validator refused the connection, say)
/// under `transport_error`; anything else is the validator's own JSON-RPC
/// response, whose `error` and `result` are read the same way the HTTP transport
/// reads them. The JSON-RPC error code is kept in the message because callers
/// match on codes such as -32015 to tell "this node cannot do that" apart from
/// "that does not exist".
fn interpret(reply: Value) -> ClientResult<Value> {
    if let Some(msg) = reply.get("transport_error").and_then(Value::as_str) {
        return Err(RpcError::RpcRequestError(msg.to_string()).into());
    }
    let body = reply.get("body").unwrap_or(&reply);
    if !body.is_object() {
        return Err(RpcError::RpcRequestError(format!(
            "RPC response is not a JSON object: {body}"
        ))
        .into());
    }
    if body["error"].is_object() {
        let code = body["error"]["code"].as_i64().unwrap_or(0);
        let message = body["error"]["message"].as_str().unwrap_or("RPC error");
        return Err(RpcError::RpcRequestError(format!("{code}: {message}")).into());
    }
    Ok(body["result"].clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_validator_error_keeps_its_code() {
        let reply = json!({
            "id": 1,
            "body": { "jsonrpc": "2.0", "id": 1,
                      "error": { "code": -32015, "message": "Transaction version (1)" } }
        });
        let err = interpret(reply).unwrap_err().to_string();
        assert!(err.contains("-32015"), "code must survive: {err}");
    }

    #[test]
    fn a_result_comes_back_unwrapped() {
        let reply =
            json!({ "id": 1, "body": { "jsonrpc": "2.0", "id": 1, "result": { "ok": 7 } } });
        assert_eq!(interpret(reply).unwrap(), json!({ "ok": 7 }));
    }

    // The page could not reach its own validator: that is not a JSON-RPC error
    // response, it is the relay saying the call never happened.
    #[test]
    fn a_page_side_failure_is_an_error_not_a_null_result() {
        let reply = json!({ "id": 1, "transport_error": "Failed to fetch" });
        let err = interpret(reply).unwrap_err().to_string();
        assert!(err.contains("Failed to fetch"), "{err}");
    }

    // A validator that answers `null` (no such account) must not look like a
    // transport failure — it is a perfectly good answer.
    #[test]
    fn a_null_result_is_still_a_result() {
        let reply = json!({ "id": 1, "body": { "jsonrpc": "2.0", "id": 1, "result": null } });
        assert!(interpret(reply).unwrap().is_null());
    }

    #[test]
    fn sessions_are_bounded_and_close_cleanly() {
        let (session, _rx) = open("relay-test-1".into(), "localnet".into()).expect("opens");
        assert!(is_open("relay-test-1"));
        assert!(session.idle_for() < Duration::from_secs(5));
        close("relay-test-1");
        assert!(!is_open("relay-test-1"));
    }
}
