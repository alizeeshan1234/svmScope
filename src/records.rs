//! Recorded account state — the free path to exact history for hot accounts.
//!
//! Nothing on chain keeps an account's past bytes, and rebuilding a busy
//! account by replaying its whole write history is not affordable. What is
//! affordable is *remembering* every version of the accounts people ask about,
//! from the moment they first ask: a poller snapshots a watched set at a short
//! interval, stores each version by slot, and from then on "bytes at slot N"
//! is a lookup for anything recorded at or before N, plus at most a few slots
//! of forward replay from the nearest version.
//!
//! The window is 30 days, in two tiers. The last 24 hours keep every change
//! (dense): inside it a version with no newer version at or before the target
//! and polling on both sides of the target is exact by lookup. Days 2 to 30
//! keep one version per 30 seconds (sparse): a version there is a floor, and
//! reconstruction replays at most 30 seconds of writes forward from it.
//! Older than 30 days is dropped.
//!
//! Versions are stored as compressed binary diffs against the previous
//! version, with a full snapshot every [`FULL_EVERY`] versions or whenever a
//! diff would not be smaller than half the account, so a pool change is a few
//! hundred bytes and no lookup replays a long chain.
//!
//! [`StateStore`] is the interface; [`LogStore`] keeps one append-only log
//! per account under a directory and rebuilds its index at startup. Packs
//! ([`LogStore::export_since`], [`LogStore::import_pack`]) move records between
//! stores — the hosted engine's ephemeral disk and the durable 30-day queue.

use {
    crate::{
        error::{Error, Result},
        AccountState,
    },
    base64::Engine,
    flate2::{read::ZlibDecoder, write::ZlibEncoder, Compression},
    solana_client::{rpc_client::RpcClient, rpc_request::RpcRequest},
    std::{
        collections::{BTreeSet, HashMap},
        io::{Read, Write},
        path::PathBuf,
        sync::Mutex,
    },
};

/// One recorded version of an account: its state as of the end of `slot`,
/// or `None` if it did not exist then.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Version {
    /// The slot the version was observed at.
    pub slot: u64,
    /// The account, or `None` for "absent at that slot".
    pub state: Option<AccountState>,
}

/// Where recorded versions live. Implementations must be safe to share across
/// threads: the poller writes while replays read.
pub trait StateStore: Send + Sync {
    /// The newest recorded version of `address` at or before `slot`.
    fn latest_at_or_before(&self, address: &str, slot: u64) -> Result<Option<Version>>;
    /// Record `state` as the version of `address` at `slot`.
    fn record(&self, address: &str, slot: u64, state: Option<&AccountState>) -> Result<()>;
    /// Add `address` to the watched set (idempotent).
    fn watch(&self, address: &str) -> Result<()>;
    /// Every watched address.
    fn watched(&self) -> Result<Vec<String>>;
    /// Note that a polling round observed the watched set at `slot`.
    fn note_round(&self, slot: u64) -> Result<()>;
    /// Whether `slot` lies inside the dense tier of a continuously polled
    /// window: rounds on both sides no further apart than
    /// [`MAX_COVERAGE_GAP`], and the slot no older than [`DENSE_SLOTS`] before
    /// the newest round. Inside such a window an account with no recorded
    /// version between its newest version at or before `slot` and the next
    /// round did not change, so that version is exact at `slot` (a change
    /// that flipped back within one interval is the only blind spot). In the
    /// sparse tier versions are thinned, so nothing is claimed by lookup.
    fn covers(&self, slot: u64) -> Result<bool>;
    /// The first slot of the oldest polled range still held, if any: where
    /// this store's recorded window begins.
    fn covered_from(&self) -> Result<Option<u64>>;
}

#[path = "records_github.rs"]
pub mod github;

/// The widest gap between two consecutive polling rounds that still counts
/// as continuous coverage (about two minutes of slots). A recorder that was
/// down longer than this leaves a hole nothing should be trusted across.
pub const MAX_COVERAGE_GAP: u64 = 300;
/// The dense tier: every change is kept for this many slots behind the
/// newest round (24 hours at 400 ms).
pub const DENSE_SLOTS: u64 = 216_000;
/// The sparse tier keeps one version per this many slots (30 seconds).
pub const SPARSE_BUCKET: u64 = 75;
/// Nothing older than this many slots behind the newest round is kept
/// (30 days).
pub const RETENTION_SLOTS: u64 = 6_480_000;
/// A full snapshot is written at least every this many versions.
pub const FULL_EVERY: usize = 50;
/// The pack entry that carries polled ranges instead of an account's versions.
const COVERAGE_ENTRY: &str = "";

// ---------------------------------------------------------------------------
// Wire format

const KIND_FULL: u8 = 0;
const KIND_DIFF: u8 = 1;
const KIND_ABSENT: u8 = 2;
/// `slot u64 | kind u8 | len u32` precede every payload.
const HEADER_LEN: usize = 13;

/// One record as it sits in a log: `slot`, its kind, and the compressed
/// payload. Diffs reference the previous record in the same log.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Record {
    slot: u64,
    kind: u8,
    payload: Vec<u8>,
}

fn deflate(bytes: &[u8]) -> Vec<u8> {
    let mut enc = ZlibEncoder::new(Vec::new(), Compression::default());
    // Writing to a Vec cannot fail.
    let _ = enc.write_all(bytes);
    enc.finish().unwrap_or_default()
}

fn inflate(bytes: &[u8]) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    ZlibDecoder::new(bytes)
        .read_to_end(&mut out)
        .map_err(|e| Error::Fixture(format!("record store: inflate: {e}")))?;
    Ok(out)
}

fn truncated() -> Error {
    Error::Fixture("record store: truncated record".into())
}

fn put_str(out: &mut Vec<u8>, s: &str) {
    out.extend_from_slice(&(s.len() as u32).to_le_bytes());
    out.extend_from_slice(s.as_bytes());
}

fn get_u32(b: &[u8], at: &mut usize) -> Result<u32> {
    let v = b.get(*at..*at + 4).ok_or_else(truncated)?;
    *at += 4;
    Ok(u32::from_le_bytes(v.try_into().unwrap_or([0; 4])))
}

fn get_u64(b: &[u8], at: &mut usize) -> Result<u64> {
    let v = b.get(*at..*at + 8).ok_or_else(truncated)?;
    *at += 8;
    Ok(u64::from_le_bytes(v.try_into().unwrap_or([0; 8])))
}

fn get_str(b: &[u8], at: &mut usize) -> Result<String> {
    let n = get_u32(b, at)? as usize;
    let s = b.get(*at..*at + n).ok_or_else(truncated)?;
    *at += n;
    String::from_utf8(s.to_vec()).map_err(|_| Error::Fixture("record store: bad utf8".into()))
}

/// Full payload: lamports, owner, data.
fn encode_full(s: &AccountState) -> Vec<u8> {
    let mut raw = Vec::with_capacity(s.data.len() + 48);
    raw.extend_from_slice(&s.lamports.to_le_bytes());
    put_str(&mut raw, &s.owner);
    raw.extend_from_slice(&s.data);
    deflate(&raw)
}

fn decode_full(payload: &[u8]) -> Result<AccountState> {
    let raw = inflate(payload)?;
    let mut at = 0;
    let lamports = get_u64(&raw, &mut at)?;
    let owner = get_str(&raw, &mut at)?;
    Ok(AccountState {
        data: raw[at..].to_vec(),
        lamports,
        owner,
    })
}

/// The byte ranges of `new` that differ from `old` (same length), as
/// (offset, bytes), with gaps under 8 bytes merged so a scattered change is
/// one range.
fn changed_ranges(old: &[u8], new: &[u8]) -> Vec<(usize, Vec<u8>)> {
    let mut ranges: Vec<(usize, usize)> = Vec::new();
    let mut i = 0;
    while i < new.len() {
        if old[i] == new[i] {
            i += 1;
            continue;
        }
        let start = i;
        while i < new.len() && old[i] != new[i] {
            i += 1;
        }
        match ranges.last_mut() {
            Some((_, end)) if start.saturating_sub(*end) < 8 => *end = i,
            _ => ranges.push((start, i)),
        }
    }
    ranges
        .into_iter()
        .map(|(s, e)| (s, new[s..e].to_vec()))
        .collect()
}

/// Diff payload against `base`: lamports, owner, changed ranges. `None` when
/// a diff is not worth it (different length, or not under half the size).
fn encode_diff(base: &AccountState, s: &AccountState) -> Option<Vec<u8>> {
    if base.data.len() != s.data.len() {
        return None;
    }
    let ranges = changed_ranges(&base.data, &s.data);
    let changed: usize = ranges.iter().map(|(_, b)| b.len() + 8).sum();
    if changed * 2 >= s.data.len().max(1) {
        return None;
    }
    let mut raw = Vec::with_capacity(changed + 48);
    raw.extend_from_slice(&s.lamports.to_le_bytes());
    put_str(&mut raw, &s.owner);
    raw.extend_from_slice(&(ranges.len() as u32).to_le_bytes());
    for (offset, bytes) in ranges {
        raw.extend_from_slice(&(offset as u32).to_le_bytes());
        raw.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
        raw.extend_from_slice(&bytes);
    }
    Some(deflate(&raw))
}

fn apply_diff(base: &AccountState, payload: &[u8]) -> Result<AccountState> {
    let raw = inflate(payload)?;
    let mut at = 0;
    let lamports = get_u64(&raw, &mut at)?;
    let owner = get_str(&raw, &mut at)?;
    let n = get_u32(&raw, &mut at)? as usize;
    let mut data = base.data.clone();
    for _ in 0..n {
        let offset = get_u32(&raw, &mut at)? as usize;
        let len = get_u32(&raw, &mut at)? as usize;
        let bytes = raw.get(at..at + len).ok_or_else(truncated)?;
        at += len;
        let dst = data
            .get_mut(offset..offset + len)
            .ok_or_else(|| Error::Fixture("record store: diff out of range".into()))?;
        dst.copy_from_slice(bytes);
    }
    Ok(AccountState {
        data,
        lamports,
        owner,
    })
}

/// Serialise one record: `slot u64 | kind u8 | len u32 | payload`.
fn write_record(out: &mut Vec<u8>, r: &Record) {
    out.extend_from_slice(&r.slot.to_le_bytes());
    out.push(r.kind);
    out.extend_from_slice(&(r.payload.len() as u32).to_le_bytes());
    out.extend_from_slice(&r.payload);
}

fn read_records(bytes: &[u8]) -> Result<Vec<Record>> {
    let mut out = Vec::new();
    for e in index_of(bytes) {
        out.push(Record {
            slot: e.slot,
            kind: e.kind,
            payload: bytes[e.offset..e.offset + e.len].to_vec(),
        });
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Chains of records for one account

/// The decoded versions of one account, in slot order, used to build and
/// rewrite logs. Lookups do not need this; they walk the log.
fn materialize_all(records: &[Record]) -> Result<Vec<Version>> {
    let mut out: Vec<Version> = Vec::with_capacity(records.len());
    let mut current: Option<AccountState> = None;
    for r in records {
        current = match r.kind {
            KIND_FULL => Some(decode_full(&r.payload)?),
            KIND_DIFF => {
                let base = current
                    .as_ref()
                    .ok_or_else(|| Error::Fixture("record store: diff without base".into()))?;
                Some(apply_diff(base, &r.payload)?)
            }
            _ => None,
        };
        out.push(Version {
            slot: r.slot,
            state: current.clone(),
        });
    }
    Ok(out)
}

/// Encode `versions` (slot-ordered) as a fresh chain: fulls where required,
/// diffs elsewhere.
fn encode_chain(versions: &[Version]) -> Vec<Record> {
    let mut out = Vec::with_capacity(versions.len());
    let mut prev: Option<&AccountState> = None;
    let mut since_full = 0usize;
    for v in versions {
        let (kind, payload) = match (&v.state, prev) {
            (None, _) => (KIND_ABSENT, Vec::new()),
            (Some(s), Some(base)) if since_full < FULL_EVERY => match encode_diff(base, s) {
                Some(d) => (KIND_DIFF, d),
                None => (KIND_FULL, encode_full(s)),
            },
            (Some(s), _) => (KIND_FULL, encode_full(s)),
        };
        since_full = if kind == KIND_FULL { 0 } else { since_full + 1 };
        prev = v.state.as_ref();
        out.push(Record {
            slot: v.slot,
            kind,
            payload,
        });
    }
    out
}

/// Which of `versions` (slot-ordered) survive thinning as of `now`: every
/// version inside the dense tier, the newest per [`SPARSE_BUCKET`] in the
/// sparse tier, nothing beyond retention.
fn thinned(versions: &[Version], now: u64) -> Vec<Version> {
    let dense_from = now.saturating_sub(DENSE_SLOTS);
    let keep_from = now.saturating_sub(RETENTION_SLOTS);
    let mut out: Vec<Version> = Vec::new();
    let mut last_bucket: Option<u64> = None;
    for v in versions {
        if v.slot < keep_from {
            continue;
        }
        if v.slot >= dense_from {
            out.push(v.clone());
            continue;
        }
        let bucket = v.slot / SPARSE_BUCKET;
        if last_bucket == Some(bucket) {
            // Newest per bucket: replace the one kept for this bucket.
            if let Some(last) = out.last_mut() {
                *last = v.clone();
            }
        } else {
            out.push(v.clone());
            last_bucket = Some(bucket);
        }
    }
    out
}

// ---------------------------------------------------------------------------
// The log-backed store

/// Index entry for one record in an account's log.
#[derive(Clone, Copy)]
struct Entry {
    slot: u64,
    kind: u8,
    offset: usize,
    len: usize,
}

fn index_of(bytes: &[u8]) -> Vec<Entry> {
    let mut out = Vec::new();
    let mut at = 0;
    while at + HEADER_LEN <= bytes.len() {
        let slot = u64::from_le_bytes(bytes[at..at + 8].try_into().unwrap_or([0; 8]));
        let kind = bytes[at + 8];
        let len = u32::from_le_bytes(bytes[at + 9..at + 13].try_into().unwrap_or([0; 4])) as usize;
        let offset = at + HEADER_LEN;
        if offset + len > bytes.len() {
            break; // a torn tail from a crash mid-append: ignore it
        }
        out.push(Entry {
            slot,
            kind,
            offset,
            len,
        });
        at = offset + len;
    }
    out
}

/// A [`StateStore`] under a directory: `<root>/accounts/<address>.log` per
/// account (append-only records), `<root>/watched.txt`, and
/// `<root>/coverage.txt` (contiguous polled slot ranges).
pub struct LogStore {
    root: PathBuf,
    index: Mutex<HashMap<String, Vec<Entry>>>,
    watched: Mutex<BTreeSet<String>>,
    /// Contiguous polled ranges `(first, last)`, ascending.
    coverage: Mutex<Vec<(u64, u64)>>,
}

fn lock_err<T>(_: T) -> Error {
    Error::Fixture("record store: lock poisoned".into())
}

fn io_err(e: std::io::Error) -> Error {
    Error::Fixture(format!("record store: {e}"))
}

impl LogStore {
    /// Open (creating if needed) a store under `root`, indexing every log.
    pub fn open(root: impl Into<PathBuf>) -> Result<LogStore> {
        let root = root.into();
        std::fs::create_dir_all(root.join("accounts")).map_err(io_err)?;
        let watched = match std::fs::read_to_string(root.join("watched.txt")) {
            Ok(s) => s
                .lines()
                .map(|l| l.trim().to_string())
                .filter(|l| !l.is_empty())
                .collect(),
            Err(_) => BTreeSet::new(),
        };
        let coverage: Vec<(u64, u64)> = match std::fs::read_to_string(root.join("coverage.txt")) {
            Ok(s) => s
                .lines()
                .filter_map(|l| {
                    let (a, b) = l.trim().split_once('-')?;
                    Some((a.parse().ok()?, b.parse().ok()?))
                })
                .collect(),
            Err(_) => Vec::new(),
        };
        let mut index = HashMap::new();
        if let Ok(dir) = std::fs::read_dir(root.join("accounts")) {
            for entry in dir.flatten() {
                let name = entry.file_name();
                let Some(address) = name.to_str().and_then(|n| n.strip_suffix(".log")) else {
                    continue;
                };
                let bytes = std::fs::read(entry.path()).map_err(io_err)?;
                index.insert(address.to_string(), index_of(&bytes));
            }
        }
        Ok(LogStore {
            root,
            index: Mutex::new(index),
            watched: Mutex::new(watched),
            coverage: Mutex::new(coverage),
        })
    }

    fn log_path(&self, address: &str) -> PathBuf {
        self.root.join("accounts").join(format!("{address}.log"))
    }

    fn read_log(&self, address: &str) -> Result<Vec<u8>> {
        match std::fs::read(self.log_path(address)) {
            Ok(b) => Ok(b),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(e) => Err(io_err(e)),
        }
    }

    fn entries(&self, address: &str) -> Result<Vec<Entry>> {
        Ok(self
            .index
            .lock()
            .map_err(lock_err)?
            .get(address)
            .cloned()
            .unwrap_or_default())
    }

    /// The newest version at or before `slot`, materialised by walking back
    /// to the nearest full snapshot and applying diffs forward.
    fn version_at(&self, address: &str, slot: u64) -> Result<Option<Version>> {
        let entries = self.entries(address)?;
        let Some(pos) = entries.iter().rposition(|e| e.slot <= slot) else {
            return Ok(None);
        };
        let target = entries[pos];
        if target.kind == KIND_ABSENT {
            return Ok(Some(Version {
                slot: target.slot,
                state: None,
            }));
        }
        let mut start = pos;
        while entries[start].kind == KIND_DIFF && start > 0 {
            start -= 1;
        }
        let bytes = self.read_log(address)?;
        let mut current: Option<AccountState> = None;
        for e in &entries[start..=pos] {
            let payload = bytes
                .get(e.offset..e.offset + e.len)
                .ok_or_else(|| Error::Fixture("record store: index out of range".into()))?;
            current = match e.kind {
                KIND_FULL => Some(decode_full(payload)?),
                KIND_DIFF => {
                    let base = current
                        .as_ref()
                        .ok_or_else(|| Error::Fixture("record store: diff without base".into()))?;
                    Some(apply_diff(base, payload)?)
                }
                _ => None,
            };
        }
        Ok(Some(Version {
            slot: target.slot,
            state: current,
        }))
    }

    /// Rewrite one account's log with a new slot-ordered chain.
    fn rewrite(&self, address: &str, versions: &[Version]) -> Result<()> {
        let records = encode_chain(versions);
        let mut bytes = Vec::new();
        for r in &records {
            write_record(&mut bytes, r);
        }
        let path = self.log_path(address);
        let tmp = path.with_extension("log.tmp");
        std::fs::write(&tmp, &bytes).map_err(io_err)?;
        std::fs::rename(&tmp, &path).map_err(io_err)?;
        self.index
            .lock()
            .map_err(lock_err)?
            .insert(address.to_string(), index_of(&bytes));
        Ok(())
    }

    /// Apply the two-tier retention as of `now`: every change inside the last
    /// [`DENSE_SLOTS`], one version per [`SPARSE_BUCKET`] out to
    /// [`RETENTION_SLOTS`], nothing older. Returns how many versions were
    /// dropped. Run it about hourly.
    pub fn thin(&self, now: u64) -> Result<usize> {
        let addresses: Vec<String> = self
            .index
            .lock()
            .map_err(lock_err)?
            .keys()
            .cloned()
            .collect();
        let mut dropped = 0;
        for address in addresses {
            let versions = materialize_all(&read_records(&self.read_log(&address)?)?)?;
            let kept = thinned(&versions, now);
            if kept.len() != versions.len() {
                dropped += versions.len() - kept.len();
                self.rewrite(&address, &kept)?;
            }
        }
        let keep_from = now.saturating_sub(RETENTION_SLOTS);
        let mut cov = self.coverage.lock().map_err(lock_err)?;
        cov.retain(|&(_, last)| last >= keep_from);
        self.write_coverage(&cov)?;
        Ok(dropped)
    }

    fn write_coverage(&self, cov: &[(u64, u64)]) -> Result<()> {
        let mut text = String::new();
        for (a, b) in cov {
            text.push_str(&format!("{a}-{b}\n"));
        }
        std::fs::write(self.root.join("coverage.txt"), text).map_err(io_err)
    }

    /// Every version observed after `since`, across all accounts, as one
    /// self-contained pack: what an hourly upload to the durable queue
    /// carries. Each account's versions are re-based so the pack needs
    /// nothing from before `since`. The pack also carries the polled ranges
    /// after `since` (as an entry under the empty address), so a store
    /// restored from packs knows what it covers, and every address in it is
    /// watched again on import.
    pub fn export_since(&self, since: u64) -> Result<Vec<u8>> {
        let addresses: Vec<String> = self
            .index
            .lock()
            .map_err(lock_err)?
            .keys()
            .cloned()
            .collect();
        let mut pack = Vec::new();
        for address in addresses {
            let versions = materialize_all(&read_records(&self.read_log(&address)?)?)?;
            let newer: Vec<Version> = versions.into_iter().filter(|v| v.slot > since).collect();
            if newer.is_empty() {
                continue;
            }
            let mut body = Vec::new();
            for r in &encode_chain(&newer) {
                write_record(&mut body, r);
            }
            put_str(&mut pack, &address);
            pack.extend_from_slice(&(body.len() as u32).to_le_bytes());
            pack.extend_from_slice(&body);
        }
        let mut cov = Vec::new();
        for &(a, b) in self.coverage.lock().map_err(lock_err)?.iter() {
            if b > since {
                cov.extend_from_slice(&a.max(since + 1).to_le_bytes());
                cov.extend_from_slice(&b.to_le_bytes());
            }
        }
        if !cov.is_empty() {
            put_str(&mut pack, COVERAGE_ENTRY);
            pack.extend_from_slice(&(cov.len() as u32).to_le_bytes());
            pack.extend_from_slice(&cov);
        }
        Ok(pack)
    }

    /// Merge polled ranges into this store's coverage, coalescing ranges
    /// that overlap or sit within [`MAX_COVERAGE_GAP`] of each other.
    fn merge_coverage(&self, ranges: &[(u64, u64)]) -> Result<()> {
        let mut cov = self.coverage.lock().map_err(lock_err)?;
        cov.extend_from_slice(ranges);
        cov.sort_unstable();
        let mut merged: Vec<(u64, u64)> = Vec::with_capacity(cov.len());
        for &(a, b) in cov.iter() {
            match merged.last_mut() {
                Some((_, last)) if a <= last.saturating_add(MAX_COVERAGE_GAP) => {
                    *last = (*last).max(b);
                }
                _ => merged.push((a, b)),
            }
        }
        *cov = merged;
        self.write_coverage(&cov)
    }

    /// Merge a pack produced by [`LogStore::export_since`] into this store.
    /// Versions already present (same slot) are kept as they are. Returns how
    /// many versions were added.
    pub fn import_pack(&self, pack: &[u8]) -> Result<usize> {
        let mut at = 0;
        let mut added = 0;
        while at < pack.len() {
            let address = get_str(pack, &mut at)?;
            let len = get_u32(pack, &mut at)? as usize;
            let body = pack.get(at..at + len).ok_or_else(truncated)?;
            at += len;
            if address == COVERAGE_ENTRY {
                let mut ranges = Vec::new();
                let mut p = 0;
                while p + 16 <= body.len() {
                    ranges.push((get_u64(body, &mut p)?, get_u64(body, &mut p)?));
                }
                self.merge_coverage(&ranges)?;
                continue;
            }
            self.watch(&address)?;
            let incoming = materialize_all(&read_records(body)?)?;
            let existing = materialize_all(&read_records(&self.read_log(&address)?)?)?;
            let mut merged: HashMap<u64, Version> =
                existing.into_iter().map(|v| (v.slot, v)).collect();
            for v in incoming {
                if let std::collections::hash_map::Entry::Vacant(e) = merged.entry(v.slot) {
                    e.insert(v);
                    added += 1;
                }
            }
            let mut all: Vec<Version> = merged.into_values().collect();
            all.sort_by_key(|v| v.slot);
            self.rewrite(&address, &all)?;
        }
        Ok(added)
    }

    /// Bytes on disk across every log.
    pub fn size_bytes(&self) -> u64 {
        std::fs::read_dir(self.root.join("accounts"))
            .map(|d| {
                d.flatten()
                    .filter_map(|e| e.metadata().ok())
                    .map(|m| m.len())
                    .sum()
            })
            .unwrap_or(0)
    }
}

impl StateStore for LogStore {
    fn latest_at_or_before(&self, address: &str, slot: u64) -> Result<Option<Version>> {
        self.version_at(address, slot)
    }

    fn record(&self, address: &str, slot: u64, state: Option<&AccountState>) -> Result<()> {
        let entries = self.entries(address)?;
        if entries.last().is_some_and(|e| e.slot >= slot) {
            return Ok(()); // never write out of order
        }
        let since_full = entries
            .iter()
            .rev()
            .take_while(|e| e.kind == KIND_DIFF)
            .count();
        let prev_state = match entries.last() {
            Some(e) if e.kind != KIND_ABSENT => {
                self.version_at(address, u64::MAX)?.and_then(|v| v.state)
            }
            _ => None,
        };
        let (kind, payload) = match (state, prev_state.as_ref()) {
            (None, _) => (KIND_ABSENT, Vec::new()),
            (Some(s), Some(base)) if since_full < FULL_EVERY => match encode_diff(base, s) {
                Some(d) => (KIND_DIFF, d),
                None => (KIND_FULL, encode_full(s)),
            },
            (Some(s), _) => (KIND_FULL, encode_full(s)),
        };
        let record = Record {
            slot,
            kind,
            payload,
        };
        let mut bytes = Vec::new();
        write_record(&mut bytes, &record);
        let path = self.log_path(address);
        let offset = std::fs::metadata(&path)
            .map(|m| m.len() as usize)
            .unwrap_or(0);
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .and_then(|mut f| f.write_all(&bytes))
            .map_err(io_err)?;
        self.index
            .lock()
            .map_err(lock_err)?
            .entry(address.to_string())
            .or_default()
            .push(Entry {
                slot,
                kind,
                offset: offset + HEADER_LEN,
                len: record.payload.len(),
            });
        Ok(())
    }

    fn watch(&self, address: &str) -> Result<()> {
        let mut set = self.watched.lock().map_err(lock_err)?;
        if set.insert(address.to_string()) {
            let mut text = String::new();
            for a in set.iter() {
                text.push_str(a);
                text.push('\n');
            }
            std::fs::write(self.root.join("watched.txt"), text).map_err(io_err)?;
        }
        Ok(())
    }

    fn watched(&self) -> Result<Vec<String>> {
        Ok(self
            .watched
            .lock()
            .map_err(lock_err)?
            .iter()
            .cloned()
            .collect())
    }

    fn note_round(&self, slot: u64) -> Result<()> {
        let mut cov = self.coverage.lock().map_err(lock_err)?;
        match cov.last_mut() {
            Some((_, last)) if *last >= slot => return Ok(()),
            Some((_, last)) if slot - *last <= MAX_COVERAGE_GAP => *last = slot,
            _ => cov.push((slot, slot)),
        }
        self.write_coverage(&cov)
    }

    fn covered_from(&self) -> Result<Option<u64>> {
        Ok(self
            .coverage
            .lock()
            .map_err(lock_err)?
            .first()
            .map(|&(a, _)| a))
    }

    fn covers(&self, slot: u64) -> Result<bool> {
        let cov = self.coverage.lock().map_err(lock_err)?;
        let Some(&(_, newest)) = cov.last() else {
            return Ok(false);
        };
        if slot.saturating_add(DENSE_SLOTS) < newest {
            return Ok(false); // sparse tier: thinned, never claimed by lookup
        }
        Ok(cov.iter().any(|&(a, b)| a <= slot && slot <= b))
    }
}

/// One polling round: fetch every watched account (100 per RPC call) at one
/// context slot and record each one whose state differs from its newest
/// recorded version. Returns how many versions were recorded.
///
/// The interval between rounds bounds how far a reconstruction has to replay
/// forward from a recorded version: at two seconds, at most a few slots.
pub fn poll_once(
    client: &RpcClient,
    store: &dyn StateStore,
    addresses: &[String],
) -> Result<usize> {
    let mut recorded = 0;
    for chunk in addresses.chunks(100) {
        let resp: serde_json::Value = client
            .send(
                RpcRequest::GetMultipleAccounts,
                serde_json::json!([chunk, { "encoding": "base64", "commitment": "confirmed" }]),
            )
            .map_err(Error::rpc)?;
        let slot = resp["context"]["slot"].as_u64().ok_or_else(|| {
            Error::MalformedRpcResponse("getMultipleAccounts: no context slot".into())
        })?;
        let values = resp["value"]
            .as_array()
            .ok_or_else(|| Error::MalformedRpcResponse("getMultipleAccounts: no value".into()))?;
        store.note_round(slot)?;
        for (address, value) in chunk.iter().zip(values) {
            let state = if value.is_null() {
                None
            } else {
                let data = value["data"][0]
                    .as_str()
                    .and_then(|s| base64::engine::general_purpose::STANDARD.decode(s).ok())
                    .unwrap_or_default();
                Some(AccountState {
                    data,
                    lamports: value["lamports"].as_u64().unwrap_or(0),
                    owner: value["owner"].as_str().unwrap_or_default().to_string(),
                })
            };
            let unchanged = store
                .latest_at_or_before(address, u64::MAX)?
                .is_some_and(|v| v.state == state);
            if !unchanged {
                store.record(address, slot, state.as_ref())?;
                recorded += 1;
            }
        }
    }
    Ok(recorded)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("svmscope-records-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    fn state(n: u8) -> AccountState {
        AccountState {
            data: vec![n; 64],
            lamports: n as u64 * 1000,
            owner: "Owner111".into(),
        }
    }

    fn pool_like(seed: u64) -> AccountState {
        // 752 bytes of incompressible filler (a real account is not runs of
        // one byte) with a few "reserve" fields that change per version.
        let mut x: u64 = 0x9E37_79B9_7F4A_7C15;
        let mut data: Vec<u8> = (0..752)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                (x & 0xff) as u8
            })
            .collect();
        data[64..72].copy_from_slice(&seed.to_le_bytes());
        data[400..408].copy_from_slice(&(seed * 3).to_le_bytes());
        AccountState {
            data,
            lamports: 2_039_280,
            owner: "PoolProgram".into(),
        }
    }

    #[test]
    fn full_and_diff_payloads_round_trip() {
        let a = pool_like(1);
        let b = pool_like(2);
        assert_eq!(decode_full(&encode_full(&a)).unwrap(), a);
        let d = encode_diff(&a, &b).expect("small change diffs");
        assert!(
            d.len() < encode_full(&b).len() / 2,
            "a diff is much smaller than a full"
        );
        assert_eq!(apply_diff(&a, &d).unwrap(), b);
        // A wholesale change is not diffed.
        let mut c = b.clone();
        c.data.iter_mut().for_each(|x| *x ^= 0xff);
        assert!(encode_diff(&b, &c).is_none());
        // A length change is not diffed.
        let mut e = b.clone();
        e.data.push(1);
        assert!(encode_diff(&b, &e).is_none());
    }

    #[test]
    fn versions_round_trip_and_lookup_picks_the_newest_at_or_before() {
        let store = LogStore::open(temp_root("lookup")).unwrap();
        store.record("Acc", 100, Some(&pool_like(1))).unwrap();
        store.record("Acc", 200, Some(&pool_like(2))).unwrap();
        store.record("Acc", 250, Some(&pool_like(3))).unwrap();
        store.record("Acc", 300, None).unwrap();
        store.record("Acc", 400, Some(&state(4))).unwrap();
        assert!(store.latest_at_or_before("Acc", 99).unwrap().is_none());
        let v = store.latest_at_or_before("Acc", 150).unwrap().unwrap();
        assert_eq!((v.slot, v.state), (100, Some(pool_like(1))));
        let v = store.latest_at_or_before("Acc", 260).unwrap().unwrap();
        assert_eq!(
            (v.slot, v.state),
            (250, Some(pool_like(3))),
            "two diffs applied on a full"
        );
        let v = store.latest_at_or_before("Acc", 350).unwrap().unwrap();
        assert_eq!((v.slot, v.state), (300, None));
        let v = store.latest_at_or_before("Acc", 10_000).unwrap().unwrap();
        assert_eq!(
            (v.slot, v.state),
            (400, Some(state(4))),
            "a full after an absent"
        );
        assert!(store
            .latest_at_or_before("Other", 10_000)
            .unwrap()
            .is_none());
        // Out-of-order writes are refused silently.
        store.record("Acc", 50, Some(&state(9))).unwrap();
        assert_eq!(store.latest_at_or_before("Acc", 60).unwrap(), None);
    }

    #[test]
    fn index_survives_reopen_and_ignores_a_torn_tail() {
        let root = temp_root("reopen");
        {
            let store = LogStore::open(&root).unwrap();
            for i in 1..=60u64 {
                store.record("Acc", i * 10, Some(&pool_like(i))).unwrap();
            }
        }
        // Append garbage as if a crash tore the last record.
        let path = root.join("accounts").join("Acc.log");
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        f.write_all(&[1, 2, 3, 4, 5, 6, 7]).unwrap();
        let store = LogStore::open(&root).unwrap();
        let v = store.latest_at_or_before("Acc", 600).unwrap().unwrap();
        assert_eq!((v.slot, v.state), (600, Some(pool_like(60))));
        let v = store.latest_at_or_before("Acc", 505).unwrap().unwrap();
        assert_eq!(v.state, Some(pool_like(50)));
    }

    #[test]
    fn thinning_keeps_dense_recent_sparse_old_and_drops_ancient() {
        let now = 10_000_000u64;
        let dense_from = now - DENSE_SLOTS;
        let mut versions = vec![Version {
            slot: 1000, // beyond retention
            state: Some(state(1)),
        }];
        // Sparse tier: 10 versions inside one 30 s bucket, then one in the next.
        let old = dense_from - 10_000;
        let bucket_start = (old / SPARSE_BUCKET) * SPARSE_BUCKET;
        for i in 0..10u64 {
            versions.push(Version {
                slot: bucket_start + i,
                state: Some(state(i as u8)),
            });
        }
        versions.push(Version {
            slot: bucket_start + SPARSE_BUCKET + 1,
            state: Some(state(42)),
        });
        // Dense tier: all kept.
        for i in 0..5u64 {
            versions.push(Version {
                slot: dense_from + 100 + i,
                state: Some(state(i as u8)),
            });
        }
        let kept = thinned(&versions, now);
        let slots: Vec<u64> = kept.iter().map(|v| v.slot).collect();
        assert_eq!(
            slots,
            vec![
                bucket_start + 9,
                bucket_start + SPARSE_BUCKET + 1,
                dense_from + 100,
                dense_from + 101,
                dense_from + 102,
                dense_from + 103,
                dense_from + 104
            ]
        );
        assert_eq!(kept[0].state, Some(state(9)), "newest per bucket wins");
    }

    #[test]
    fn thin_rewrites_logs_and_lookups_still_work() {
        let store = LogStore::open(temp_root("thin")).unwrap();
        let now = 10_000_000u64;
        let old = now - DENSE_SLOTS - 5_000;
        let bucket_start = (old / SPARSE_BUCKET) * SPARSE_BUCKET;
        for i in 0..20u64 {
            store
                .record("Acc", bucket_start + i, Some(&pool_like(i)))
                .unwrap();
        }
        store.record("Acc", now - 10, Some(&pool_like(99))).unwrap();
        let dropped = store.thin(now).unwrap();
        assert_eq!(dropped, 19, "one sparse bucket keeps one of twenty");
        let v = store
            .latest_at_or_before("Acc", bucket_start + 100)
            .unwrap()
            .unwrap();
        assert_eq!((v.slot, v.state), (bucket_start + 19, Some(pool_like(19))));
        let v = store.latest_at_or_before("Acc", now).unwrap().unwrap();
        assert_eq!(v.state, Some(pool_like(99)));
    }

    #[test]
    fn coverage_is_dense_tier_only_and_needs_both_sides() {
        let store = LogStore::open(temp_root("coverage")).unwrap();
        assert!(!store.covers(100).unwrap());
        store.note_round(100).unwrap();
        assert!(store.covers(100).unwrap());
        assert!(!store.covers(150).unwrap(), "nothing after 150 yet");
        store.note_round(200).unwrap();
        assert!(store.covers(150).unwrap());
        store.note_round(200 + MAX_COVERAGE_GAP + 1).unwrap();
        assert!(
            !store.covers(300).unwrap(),
            "the recorder was down across 300"
        );
        assert!(!store.covers(50).unwrap(), "before recording began");
        // The sparse tier is never covered by lookup.
        let far = 200 + MAX_COVERAGE_GAP + 1 + DENSE_SLOTS + 10;
        store.note_round(far).unwrap();
        assert!(
            !store.covers(150).unwrap(),
            "150 has fallen out of the dense tier"
        );
    }

    #[test]
    fn packs_export_and_import_between_stores() {
        let a = LogStore::open(temp_root("pack-a")).unwrap();
        let b = LogStore::open(temp_root("pack-b")).unwrap();
        for i in 1..=10u64 {
            a.record("Acc", i * 10, Some(&pool_like(i))).unwrap();
            a.record("Other", i * 10, Some(&state(i as u8))).unwrap();
        }
        let pack = a.export_since(50).unwrap();
        let added = b.import_pack(&pack).unwrap();
        assert_eq!(added, 10, "five newer versions of each account");
        assert!(b.latest_at_or_before("Acc", 50).unwrap().is_none());
        let v = b.latest_at_or_before("Acc", 85).unwrap().unwrap();
        assert_eq!((v.slot, v.state), (80, Some(pool_like(8))));
        // Importing again adds nothing.
        assert_eq!(b.import_pack(&pack).unwrap(), 0);
    }

    #[test]
    fn packs_carry_coverage_and_the_watched_set() {
        let a = LogStore::open(temp_root("pack-cov-a")).unwrap();
        let b = LogStore::open(temp_root("pack-cov-b")).unwrap();
        a.watch("Acc").unwrap();
        for slot in [10u64, 20, 30, 40, 1000, 1010] {
            a.record("Acc", slot, Some(&state(slot as u8))).unwrap();
            a.note_round(slot).unwrap();
        }
        // Two polled ranges in `a`: 10-40 and 1000-1010 (a gap of 960 > MAX_COVERAGE_GAP).
        assert!(a.covers(25).unwrap() && !a.covers(500).unwrap());
        let pack = a.export_since(15).unwrap();
        assert_eq!(b.import_pack(&pack).unwrap(), 5);
        assert_eq!(b.watched().unwrap(), vec!["Acc".to_string()]);
        // Coverage after `since` is restored, clipped at since + 1.
        assert_eq!(b.covered_from().unwrap(), Some(16));
        assert!(b.covers(25).unwrap());
        assert!(!b.covers(500).unwrap());
        assert!(b.covers(1005).unwrap());
        // Importing the same pack again changes nothing.
        b.import_pack(&pack).unwrap();
        assert_eq!(b.covered_from().unwrap(), Some(16));
        // A later pack that continues the last range extends it.
        for slot in [1020u64, 1030] {
            a.record("Acc", slot, Some(&state(slot as u8))).unwrap();
            a.note_round(slot).unwrap();
        }
        b.import_pack(&a.export_since(1010).unwrap()).unwrap();
        assert!(b.covers(1025).unwrap());
    }

    #[test]
    fn watched_set_persists_across_opens() {
        let root = temp_root("watched");
        {
            let store = LogStore::open(&root).unwrap();
            store.watch("B").unwrap();
            store.watch("A").unwrap();
            store.watch("B").unwrap();
        }
        let store = LogStore::open(&root).unwrap();
        assert_eq!(
            store.watched().unwrap(),
            vec!["A".to_string(), "B".to_string()]
        );
    }
}
