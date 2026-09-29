use crate::error::{Error, Result};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use std::time::Duration;

const JITO_BASE_URL: &str = "https://bundles.jito.wtf/api/v1";

/// What the Jito explorer knows about one landed bundle.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BundleMeta {
    /// The bundle id.
    pub id: String,
    /// The slot it landed in.
    pub slot: u64,
    /// Its position within the block, when reported.
    pub block_index: Option<u64>,
    /// Its transactions, in execution order.
    pub signatures: Vec<String>,
    /// The accounts that paid the tip.
    pub tippers: Vec<String>,
    /// The tip that actually landed, in lamports.
    pub tip_lamports: u64,
}

#[derive(Deserialize)]
struct BundleOfTransaction {
    bundle_id: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct BundleEntry {
    pub(crate) bundle_id: String,
    pub(crate) slot: u64,
    #[serde(default)]
    pub(crate) block_index: Option<u64>,
    #[serde(default)]
    pub(crate) tx_signatures: Vec<String>,
    #[serde(default)]
    pub(crate) tippers: Vec<String>,
    #[serde(default)]
    pub(crate) landed_tip_lamports: u64,
}

impl From<BundleEntry> for BundleMeta {
    fn from(entry: BundleEntry) -> BundleMeta {
        BundleMeta {
            id: entry.bundle_id,
            slot: entry.slot,
            block_index: entry.block_index,
            signatures: entry.tx_signatures,
            tippers: entry.tippers,
            tip_lamports: entry.landed_tip_lamports,
        }
    }
}

/// The most transactions Jito lands in one bundle.
pub(crate) const MAX_BUNDLE_TXS: usize = 5;

/// A run of consecutive signatures from one bundle, or one signature that
/// was not in any bundle. A list longer than a bundle allows splits into
/// several segments; the engine replays each exactly and carries state
/// between them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Segment {
    /// The bundle these signatures landed in, or `None` when unbundled.
    pub bundle_id: Option<String>,
    /// The signatures, in order.
    pub signatures: Vec<String>,
}

/// Group `lookups` (signature, bundle id if any) into segments: consecutive
/// signatures sharing a bundle id join one segment; a signature with no
/// bundle is a segment of its own. Order is preserved.
pub(crate) fn segment_lookups(lookups: Vec<(String, Option<String>)>) -> Vec<Segment> {
    let mut out: Vec<Segment> = Vec::new();
    for (sig, bundle_id) in lookups {
        match out.last_mut() {
            Some(last) if last.bundle_id.is_some() && last.bundle_id == bundle_id => {
                last.signatures.push(sig)
            }
            _ => out.push(Segment {
                bundle_id,
                signatures: vec![sig],
            }),
        }
    }
    out
}

fn api_error(context: &str, error: impl std::fmt::Display) -> Error {
    Error::MalformedRpcResponse(format!("{}: {}", context, error))
}

pub(crate) struct JitoClient {
    client: reqwest::blocking::Client,
}

impl JitoClient {
    pub(crate) fn new() -> Result<JitoClient> {
        let client = reqwest::blocking::Client::builder()
            .user_agent("svmscope-bundles")
            .timeout(Duration::from_secs(30))
            .build()
            .map_err(|e| api_error("client", e))
            .unwrap();

        let jito_client = JitoClient { client };

        Ok(jito_client)
    }

    fn get<T: DeserializeOwned>(&self, path: &str) -> Result<Option<T>> {
        let url = format!("{JITO_BASE_URL}{path}");
        let resp = self
            .client
            .get(&url)
            .send()
            .map_err(|e| api_error("get", e))?;

        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        };

        if !resp.status().is_success() {
            return Err(api_error("get", resp.status()));
        };

        resp.json::<T>()
            .map(Some)
            .map_err(|e| api_error("get json", e))
    }

    pub(crate) fn bundle_of_signature(&self, signature: &str) -> Result<Option<String>> {
        let list =
            self.get::<Vec<BundleOfTransaction>>(&format!("/bundles/transaction/{signature}"))?;
        Ok(list.and_then(|v| v.into_iter().next()).map(|b| b.bundle_id))
    }

    /// Split an ordered list of signatures into bundle segments by looking
    /// each one up. Six signatures where five landed in one bundle and the
    /// sixth in the next become two segments.
    pub(crate) fn segments(&self, signatures: &[String]) -> Result<Vec<Segment>> {
        let mut lookups = Vec::with_capacity(signatures.len());
        for sig in signatures {
            lookups.push((sig.clone(), self.bundle_of_signature(sig)?));
        }
        Ok(segment_lookups(lookups))
    }

    pub(crate) fn bundle(&self, id: &str) -> Result<Option<BundleMeta>> {
        let list = self.get::<Vec<BundleEntry>>(&format!("/bundles/bundle/{id}"))?;
        Ok(list
            .and_then(|v| v.into_iter().next())
            .map(BundleMeta::from))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SIG: &str =
        "2bbBa5xtDMPTQNxaPFxxZzniB79FzHzjkYu44aho7tiU9WvkJ3gxrVUjzRQM8LUQpphEiM37zCdmFuXUdk3RY2eg";
    const ID: &str = "808e45f8925788196cc0b11d9cefed8ef5c277e6f81ba79afbfc111d93fbfaa5";

    #[test]
    fn parses_a_bundle_entry() {
        let json = r#"[{"bundleId":"808e45f8925788196cc0b11d9cefed8ef5c277e6f81ba79afbfc111d93fbfaa5","slot":451661014,"validator":"C8Bey3LKVJHVqN6xPTeW8WJfUgFQAeGNBpT4Rp99JP1k","tippers":["sp1nLqqjVZ1kTusFjJmHp1xHDpH6M4F7KXCr1iJ3xpX"],"landedTipLamports":1000,"landedCu":594,"blockIndex":233,"timestamp":"2026-09-29T13:53:40+00:00","txSignatures":["2bbBa5xtDMPTQNxaPFxxZzniB79FzHzjkYu44aho7tiU9WvkJ3gxrVUjzRQM8LUQpphEiM37zCdmFuXUdk3RY2eg"]}]"#;
        let entries: Vec<BundleEntry> = serde_json::from_str(json).unwrap();
        let meta = BundleMeta::from(entries.into_iter().next().unwrap());
        assert_eq!(meta.id, ID);
        assert_eq!(meta.slot, 451661014);
        assert_eq!(meta.block_index, Some(233));
        assert_eq!(meta.signatures, vec![SIG.to_string()]);
        assert_eq!(meta.tippers.len(), 1);
        assert_eq!(meta.tip_lamports, 1000);
    }

    #[test]
    fn missing_optional_fields_still_parse() {
        let json = r#"[{"bundleId":"x","slot":1}]"#;
        let entries: Vec<BundleEntry> = serde_json::from_str(json).unwrap();
        let meta = BundleMeta::from(entries.into_iter().next().unwrap());
        assert_eq!(meta.block_index, None);
        assert!(meta.signatures.is_empty());
        assert_eq!(meta.tip_lamports, 0);
    }

    #[test]
    #[ignore = "network"]
    fn signature_to_bundle_and_back() {
        let client = JitoClient::new().unwrap();
        let id = client.bundle_of_signature(SIG).unwrap();
        assert_eq!(id.as_deref(), Some(ID));
        let meta = client.bundle(ID).unwrap().expect("bundle exists");
        assert_eq!(meta.slot, 451661014);
        assert!(meta.signatures.iter().any(|s| s == SIG));
    }

    /// A bundle holds up to five transactions; the explorer must give them
    /// back in execution order, untouched.
    #[test]
    fn keeps_all_five_signatures_in_order() {
        let sigs: Vec<String> = (1..=5).map(|i| format!("sig{i}")).collect();
        let json = format!(
            r#"[{{"bundleId":"five","slot":7,"blockIndex":3,"txSignatures":{}}}]"#,
            serde_json::to_string(&sigs).unwrap()
        );
        let entries: Vec<BundleEntry> = serde_json::from_str(&json).unwrap();
        let meta = BundleMeta::from(entries.into_iter().next().unwrap());
        assert_eq!(meta.signatures.len(), 5);
        assert_eq!(meta.signatures, sigs);
    }

    /// A real two-transaction bundle: every signature in it resolves back to
    /// the same bundle id, and the contents come in the landed order.
    #[test]
    #[ignore = "network"]
    fn multi_transaction_bundle_round_trips() {
        const MULTI_ID: &str = "05a6d9981728cd177f642c8157877bdfa72668bec370bb18a011665cfb4af1e0";
        const FIRST: &str =
            "4A93zFtyM45R14HA1aWEqiMGDMcJgPdUWbzvHoeyGbE9SFeHVV64TMSpaJLyn8DrbGxWehwsoYyZQuLZ61SpBVBu";
        const SECOND: &str =
            "2MmjmVSN337eZygkbhVQd9C9dC5H7U8h7hf6oq3cUBosCSAPy8Z2RQqPNuysiE9aGWfgPFaaFPJ8wniub7D12DAj";
        let client = JitoClient::new().unwrap();
        let meta = client.bundle(MULTI_ID).unwrap().expect("bundle exists");
        assert_eq!(meta.slot, 451678738);
        assert_eq!(meta.signatures, vec![FIRST.to_string(), SECOND.to_string()]);
        assert!(meta.signatures.len() <= 5, "Jito bundles hold at most five");
        for sig in &meta.signatures {
            assert_eq!(
                client.bundle_of_signature(sig).unwrap().as_deref(),
                Some(MULTI_ID)
            );
        }
    }

    fn sigs(n: usize, prefix: &str) -> Vec<String> {
        (1..=n).map(|i| format!("{prefix}{i}")).collect()
    }

    /// Six dependent transactions: Jito lands at most five per bundle, so the
    /// sixth is in a second bundle. The list must split into two segments,
    /// 5 + 1, in order, and never one segment of six.
    #[test]
    fn six_signatures_split_into_two_bundles() {
        let first = sigs(5, "a");
        let sixth = "b1".to_string();
        let mut lookups: Vec<(String, Option<String>)> = first
            .iter()
            .map(|s| (s.clone(), Some("bundle-A".to_string())))
            .collect();
        lookups.push((sixth.clone(), Some("bundle-B".to_string())));
        let segments = segment_lookups(lookups);
        assert_eq!(segments.len(), 2);
        assert_eq!(segments[0].bundle_id.as_deref(), Some("bundle-A"));
        assert_eq!(segments[0].signatures, first);
        assert_eq!(segments[1].bundle_id.as_deref(), Some("bundle-B"));
        assert_eq!(segments[1].signatures, vec![sixth]);
        assert!(segments
            .iter()
            .all(|s| s.signatures.len() <= MAX_BUNDLE_TXS));
    }

    /// A transaction that was not bundled stands alone and does not merge
    /// with neighbours, even when its neighbours share a bundle.
    #[test]
    fn unbundled_signature_is_its_own_segment() {
        let lookups = vec![
            ("a1".to_string(), Some("A".to_string())),
            ("a2".to_string(), Some("A".to_string())),
            ("x".to_string(), None),
            ("y".to_string(), None),
            ("b1".to_string(), Some("B".to_string())),
        ];
        let segments = segment_lookups(lookups);
        let shape: Vec<(Option<&str>, usize)> = segments
            .iter()
            .map(|s| (s.bundle_id.as_deref(), s.signatures.len()))
            .collect();
        assert_eq!(
            shape,
            vec![(Some("A"), 2), (None, 1), (None, 1), (Some("B"), 1)]
        );
    }

    /// Two real bundles: their signatures, concatenated, must come back as
    /// two segments carrying the original bundle ids.
    #[test]
    #[ignore = "network"]
    fn real_signatures_from_two_bundles_split_correctly() {
        let client = JitoClient::new().unwrap();
        let a = client
            .bundle("05a6d9981728cd177f642c8157877bdfa72668bec370bb18a011665cfb4af1e0")
            .unwrap()
            .unwrap();
        let b = client.bundle(ID).unwrap().unwrap();
        let mut all = a.signatures.clone();
        all.extend(b.signatures.clone());
        let segments = client.segments(&all).unwrap();
        assert_eq!(segments.len(), 2);
        assert_eq!(segments[0].bundle_id.as_deref(), Some(a.id.as_str()));
        assert_eq!(segments[0].signatures, a.signatures);
        assert_eq!(segments[1].bundle_id.as_deref(), Some(b.id.as_str()));
        assert_eq!(segments[1].signatures, b.signatures);
    }

    #[test]
    #[ignore = "network"]
    fn a_plain_transaction_is_not_a_bundle() {
        let client = JitoClient::new().unwrap();
        let plain = "5aRbNXeZrTQGVnmMdUvBiRpS5b99KBcXoCaLXacveUz5JwKeY7muxPT5CRCSTrpm61pFLLxLmEuEXGzpascTwEFW";
        assert_eq!(client.bundle_of_signature(plain).unwrap(), None);
    }
}
