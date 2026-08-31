//! Historical state reconstruction — rebuild an account's state at a past slot
//! by replaying its own write history forward, instead of buying it from a paid
//! archive. This is the free, self-owned path to "replay at any slot".
//!
//! The raw ledger (which transactions touched an account, and their contents)
//! comes from a [`LedgerSource`]: a standard Solana RPC for recent history, or
//! **Old Faithful** — the free, open, Solana-Foundation-funded archive of the
//! whole chain — for deep history. Both expose the same `getSignaturesForAddress`
//! / `getTransaction` methods, so the engine is source-agnostic; only the reach
//! differs. That is what lets this be built and tested against an ordinary RPC
//! today, and pointed at Old Faithful for full history later.

use crate::error::{Error, Result};
use serde_json::{json, Value};
use solana_client::rpc_client::RpcClient;
use solana_client::rpc_request::RpcRequest;

/// One transaction that touched an account, and where it landed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WriteRef {
    /// The transaction signature.
    pub signature: String,
    /// The slot it landed in.
    pub slot: u64,
    /// Whether it failed (a failed transaction changes no account data, only fees).
    pub failed: bool,
}

/// A source of raw ledger data for reconstruction. A standard RPC implements it
/// for whatever history it retains; an Old Faithful node implements it for all
/// history. The reconstruction engine is written against this trait, not against
/// any one provider.
pub trait LedgerSource {
    /// Signatures that touched `address`, newest-first, paging back from the
    /// `before` signature when given. At most `limit` returned.
    fn signatures_for_address(
        &self,
        address: &str,
        before: Option<&str>,
        limit: usize,
    ) -> Result<Vec<WriteRef>>;

    /// The full `getTransaction` JSON for a signature.
    fn transaction(&self, signature: &str) -> Result<Value>;
}

/// A [`LedgerSource`] backed by a standard Solana JSON-RPC endpoint. Its reach is
/// whatever history that endpoint keeps; swap in an Old Faithful source for the
/// full chain without changing the reconstruction engine.
pub struct RpcLedger {
    client: RpcClient,
}

impl RpcLedger {
    /// A ledger source over the given RPC endpoint.
    pub fn new(url: impl Into<String>) -> RpcLedger {
        RpcLedger {
            client: RpcClient::new(url.into()),
        }
    }
}

impl LedgerSource for RpcLedger {
    fn signatures_for_address(
        &self,
        address: &str,
        before: Option<&str>,
        limit: usize,
    ) -> Result<Vec<WriteRef>> {
        let mut opts = json!({ "limit": limit });
        if let Some(b) = before {
            opts["before"] = json!(b);
        }
        let resp: Value = self
            .client
            .send(RpcRequest::GetSignaturesForAddress, json!([address, opts]))
            .map_err(Error::rpc)?;
        let arr = resp.as_array().ok_or_else(|| {
            Error::MalformedRpcResponse("getSignaturesForAddress: not an array".into())
        })?;
        Ok(arr
            .iter()
            .filter_map(|s| {
                Some(WriteRef {
                    signature: s["signature"].as_str()?.to_string(),
                    slot: s["slot"].as_u64()?,
                    failed: !s["err"].is_null(),
                })
            })
            .collect())
    }

    fn transaction(&self, signature: &str) -> Result<Value> {
        let resp: Value = self
            .client
            .send(
                RpcRequest::GetTransaction,
                json!([
                    signature,
                    { "encoding": "json", "commitment": "confirmed", "maxSupportedTransactionVersion": 0 }
                ]),
            )
            .map_err(Error::rpc)?;
        if resp.is_null() {
            return Err(Error::TransactionNotFound(signature.to_string()));
        }
        Ok(resp)
    }
}

/// The successful transactions that wrote `address` strictly before `slot`,
/// **oldest-first** — the ordered write history to replay when reconstructing the
/// account's state going into `slot`. Pages back through the ledger source until
/// it gathers `max` writes, exhausts history, or hits `max_pages` (a guard so a
/// very active account can't page unboundedly on a first pass).
pub fn write_history(
    src: &dyn LedgerSource,
    address: &str,
    before_slot: u64,
    max: usize,
    max_pages: usize,
) -> Result<Vec<WriteRef>> {
    let page = 1000usize.min(max.max(1));
    let mut out: Vec<WriteRef> = Vec::new();
    let mut before: Option<String> = None;

    for _ in 0..max_pages {
        let batch = src.signatures_for_address(address, before.as_deref(), page)?;
        let Some(last) = batch.last() else {
            break; // history exhausted
        };
        before = Some(last.signature.clone());
        for w in &batch {
            if w.slot < before_slot && !w.failed {
                out.push(w.clone());
                if out.len() >= max {
                    break;
                }
            }
        }
        if out.len() >= max {
            break;
        }
    }

    // The source returns newest-first; the replay wants oldest-first.
    out.reverse();
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A canned ledger for testing the walker without a network — `sigs` are
    /// newest-first, as a real `getSignaturesForAddress` returns them.
    struct MockLedger {
        sigs: Vec<WriteRef>,
    }

    impl LedgerSource for MockLedger {
        fn signatures_for_address(
            &self,
            _address: &str,
            before: Option<&str>,
            limit: usize,
        ) -> Result<Vec<WriteRef>> {
            let start = match before {
                None => 0,
                Some(b) => self
                    .sigs
                    .iter()
                    .position(|w| w.signature == b)
                    .map(|i| i + 1)
                    .unwrap_or(self.sigs.len()),
            };
            Ok(self.sigs[start..].iter().take(limit).cloned().collect())
        }
        fn transaction(&self, _signature: &str) -> Result<Value> {
            Ok(json!({}))
        }
    }

    fn w(sig: &str, slot: u64, failed: bool) -> WriteRef {
        WriteRef {
            signature: sig.to_string(),
            slot,
            failed,
        }
    }

    #[test]
    fn write_history_keeps_pre_slot_successes_oldest_first() {
        // newest-first, spanning the target slot 100, with a failed tx mixed in.
        let ledger = MockLedger {
            sigs: vec![
                w("s120", 120, false), // after the slot → excluded
                w("s090", 90, false),
                w("s080", 80, true), // failed → excluded
                w("s070", 70, false),
                w("s060", 60, false),
            ],
        };
        let hist = write_history(&ledger, "Acc", 100, 100, 10).unwrap();
        let sigs: Vec<&str> = hist.iter().map(|w| w.signature.as_str()).collect();
        // oldest-first, only successful writes strictly before slot 100.
        assert_eq!(sigs, vec!["s060", "s070", "s090"]);
    }

    #[test]
    fn write_history_respects_the_max_cap() {
        let ledger = MockLedger {
            sigs: (0..50)
                .map(|i| w(&format!("s{i:02}"), 50 - i, false))
                .collect(),
        };
        let hist = write_history(&ledger, "Acc", 1000, 5, 10).unwrap();
        assert_eq!(hist.len(), 5);
        // still oldest-first
        assert!(hist[0].slot < hist[hist.len() - 1].slot);
    }
}
