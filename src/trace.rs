//! Step-by-step execution trace of a transaction — the debugger's data model.
//!
//! A [`Trace`] is the transaction unrolled into [`Step`]s in pre-order: each
//! top-level instruction followed by the CPIs it made, nested by `depth`. Every
//! step carries its decoded instruction, the accounts it touched, the compute it
//! consumed, its slice of the logs, and (for top-level steps) the exact
//! before → after diff of every account it changed. The failing step, when there
//! is one, is pinpointed with its error decoded.
//!
//! Top-level state is obtained by replaying instruction prefixes (`0..=k`) and
//! diffing; CPI rows carry structure, logs and compute but not state until the
//! runtime hook lands (`state_known == false`).

use serde::Serialize;

use crate::analyze::{AccountDiff, Explanation};
use crate::cpi_tree::{IxAccount, IxArg};
use crate::replay::ReplayResult;

/// The whole transaction, unrolled.
#[derive(Debug, Clone, Serialize)]
pub struct Trace {
    /// The traced transaction's signature (empty for pre-flight transactions).
    pub signature: String,
    /// Every instruction and CPI, pre-order.
    pub steps: Vec<Step>,
    /// The full-transaction outcome (the last prefix, i.e. the whole thing).
    pub result: ReplayResult,
    /// The failure in plain language, when the transaction failed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub explain: Option<Explanation>,
    /// Index into `steps` of the innermost step that failed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failed_step: Option<usize>,
    /// Where the clock was warped to, when time travel was requested.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub clock: Option<String>,
    /// How faithful the starting state is to the transaction's slot.
    pub fidelity: String,
    /// What actually happened on-chain, when known. A replay that disagrees
    /// with this is state drift (see `fidelity`), not a bug in the program.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub onchain_success: Option<bool>,
}

/// One instruction or CPI.
#[derive(Debug, Clone, Serialize)]
pub struct Step {
    /// Position in the tree: "2" is the third top-level instruction, "2.0" its
    /// first CPI, "2.0.1" that CPI's second nested call.
    pub path: String,
    /// 1 = top-level, 2 = CPI, 3 = nested CPI, …
    pub depth: u8,
    /// The top-level instruction this step belongs to.
    pub index: usize,
    /// The invoked program's address.
    pub program: String,
    /// Decoded instruction name where an IDL or native layout allows it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Decoded arguments.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<IxArg>,
    /// The accounts passed to the instruction, named where known.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub accounts: Vec<IxAccount>,
    /// Compute units this invocation consumed (inclusive of its own CPIs), when
    /// the program logs it. Builtins don't.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cu_consumed: Option<u64>,
    /// `[start, end)` into `Trace::result.logs` — the lines this step produced,
    /// including its CPIs' lines.
    pub logs: (usize, usize),
    /// Accounts this step changed, before → after. Top-level steps only.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub diffs: Vec<AccountDiff>,
    /// Whether `diffs` is authoritative. False for CPI rows (structure known,
    /// state not yet captured) and for steps after a failure.
    pub state_known: bool,
    /// Whether this invocation completed without error.
    pub success: bool,
    /// The error, when this is the step that raised it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<StepError>,
    /// Data the program returned (`set_return_data`), when this step's program
    /// set it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub return_data: Option<ReturnData>,
    /// Anchor events this invocation emitted (`Program data:` lines decoded
    /// against the program's IDL).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub events: Vec<DecodedEvent>,
    /// The accounts this instruction was given, decoded as they stand after
    /// the step (for CPIs: after the enclosing top-level step).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub state: Vec<StepAccountState>,
    /// True when this prefix failed but the whole transaction did not: a
    /// mid-transaction state (e.g. below rent) that a later instruction fixes.
    /// Not a real failure.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub prefix_artifact: bool,
}

/// Return data a program set during a step.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ReturnData {
    /// The program that set it.
    pub program: String,
    /// The bytes, base64.
    pub data_base64: String,
}

/// One Anchor event, decoded.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DecodedEvent {
    /// The event's IDL name.
    pub name: String,
    /// Its fields, decoded up to the first variable-length one.
    pub fields: Vec<crate::decode::Field>,
}

/// One account as it stands after a step.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct StepAccountState {
    /// The account's address.
    pub address: String,
    /// The IDL role name the instruction gave it, where known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    /// The owning program.
    pub owner: String,
    /// Lamports after the step.
    pub lamports: u64,
    /// Data length in bytes.
    pub data_len: usize,
    /// Decoded type label ("SPL Token Account", an IDL account name), when the
    /// layout is known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub type_name: Option<String>,
    /// Decoded fields, when the layout is known. Each carries its offset, type
    /// and whether it is safe to edit in place.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub fields: Vec<crate::decode::Field>,
    /// Whether this step changed the account.
    pub changed: bool,
    /// Whether the account exists at all after the step.
    pub exists: bool,
}

/// A failure attributed to a step.
#[derive(Debug, Clone, Serialize)]
pub struct StepError {
    /// The raw runtime error, e.g. `InstructionError(2, Custom(6001))`.
    pub raw: String,
    /// Decoded via the program's IDL / known runtime errors, when possible.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub explain: Option<Explanation>,
}

/// One step's outcome, compact enough to compare across two traces.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct StepSummary {
    /// The invoked program.
    pub program: String,
    /// Decoded instruction name, where known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Whether the invocation succeeded.
    pub success: bool,
    /// Compute consumed, where logged.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cu_consumed: Option<u64>,
    /// Raw error, when this step raised one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// `(address, field, before, after)` for every changed field.
    pub changes: Vec<(String, String, String, String)>,
}

/// What changed between two traces of the same transaction (typically before
/// and after a mutation).
#[derive(Debug, Clone, Serialize)]
pub struct TraceDiff {
    /// Whether the transaction's overall success flipped.
    pub outcome_changed: bool,
    /// Steps whose outcome, error, compute or state changes differ.
    pub steps: Vec<StepDiff>,
}

/// One step's before/after across two traces. `None` on a side means the step
/// did not execute in that trace (an earlier step failed).
#[derive(Debug, Clone, Serialize)]
pub struct StepDiff {
    /// The step's position, shared by both traces.
    pub path: String,
    /// The step in the first trace.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub before: Option<StepSummary>,
    /// The step in the second trace.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub after: Option<StepSummary>,
}

impl Step {
    fn summary(&self) -> StepSummary {
        let mut changes: Vec<(String, String, String, String)> = self
            .diffs
            .iter()
            .flat_map(|d| {
                let addr = d.address.clone();
                let mut v: Vec<_> = d
                    .fields
                    .iter()
                    .map(|f| {
                        (
                            addr.clone(),
                            f.name.clone(),
                            f.before.clone(),
                            f.after.clone(),
                        )
                    })
                    .collect();
                if d.lamports_before != d.lamports_after {
                    v.push((
                        addr.clone(),
                        "lamports".into(),
                        d.lamports_before.to_string(),
                        d.lamports_after.to_string(),
                    ));
                }
                if d.raw_data_changed && d.fields.is_empty() {
                    v.push((addr, "data".into(), "…".into(), "changed".into()));
                }
                v
            })
            .collect();
        changes.sort();
        StepSummary {
            program: self.program.clone(),
            name: self.name.clone(),
            success: self.success,
            cu_consumed: self.cu_consumed,
            error: self.error.as_ref().map(|e| e.raw.clone()),
            changes,
        }
    }
}

impl Trace {
    /// True when the replay's outcome differs from the recorded on-chain
    /// outcome: the starting state has drifted from the transaction's slot.
    pub fn drifted(&self) -> bool {
        matches!(self.onchain_success, Some(o) if o != self.result.success)
    }

    /// The step that raised the transaction's error, if any.
    pub fn failed(&self) -> Option<&Step> {
        self.failed_step.and_then(|i| self.steps.get(i))
    }

    /// Top-level steps only.
    pub fn top_level(&self) -> impl Iterator<Item = &Step> {
        self.steps.iter().filter(|s| s.depth == 1)
    }

    /// Compare with another trace of the same transaction — "what did my
    /// change do?" Steps are matched by `path`.
    pub fn diff(&self, other: &Trace) -> TraceDiff {
        let mut paths: Vec<&str> = self
            .steps
            .iter()
            .chain(other.steps.iter())
            .map(|s| s.path.as_str())
            .collect();
        paths.sort_by_key(|p| path_key(p));
        paths.dedup();
        let find = |t: &Trace, p: &str| t.steps.iter().find(|s| s.path == p).map(Step::summary);
        let steps = paths
            .into_iter()
            .filter_map(|p| {
                let (before, after) = (find(self, p), find(other, p));
                (before != after).then(|| StepDiff {
                    path: p.to_string(),
                    before,
                    after,
                })
            })
            .collect();
        TraceDiff {
            outcome_changed: self.result.success != other.result.success,
            steps,
        }
    }
}

fn path_key(p: &str) -> Vec<usize> {
    p.split('.').filter_map(|s| s.parse().ok()).collect()
}

// ---------------------------------------------------------------------------
// Log attribution: which lines belong to which step.
// ---------------------------------------------------------------------------

/// What the runtime says about one invocation, recovered from its log lines.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct LogSpan {
    pub program: String,
    pub depth: u8,
    pub start: usize,
    pub end: usize,
    pub cu_consumed: Option<u64>,
    pub success: bool,
    /// The `Program X failed: …` tail, when it failed.
    pub failure: Option<String>,
}

/// Walk `logs[from..]` and recover every invocation in pre-order. The runtime
/// emits exactly one `Program <id> invoke [d]` per invocation and closes it with
/// `Program <id> success` or `Program <id> failed: …`, so a stack reproduces the
/// tree. Indices are absolute into `logs`.
pub(crate) fn spans_from_logs(logs: &[String], from: usize) -> Vec<LogSpan> {
    let mut spans: Vec<LogSpan> = Vec::new();
    let mut stack: Vec<usize> = Vec::new(); // indexes into `spans`
    for (i, line) in logs.iter().enumerate().skip(from) {
        let Some(rest) = line.strip_prefix("Program ") else {
            continue;
        };
        if let Some(pos) = rest.find(" invoke [") {
            let program = rest[..pos].to_string();
            let depth = rest[pos + 9..]
                .trim_end_matches(']')
                .parse::<u8>()
                .unwrap_or(stack.len() as u8 + 1);
            spans.push(LogSpan {
                program,
                depth,
                start: i,
                end: i + 1,
                ..Default::default()
            });
            stack.push(spans.len() - 1);
        } else if let Some(pos) = rest.find(" consumed ") {
            let program = &rest[..pos];
            let cu = rest[pos + 10..]
                .split(' ')
                .next()
                .and_then(|n| n.parse::<u64>().ok());
            if let Some(&top) = stack.iter().rev().find(|&&s| spans[s].program == program) {
                spans[top].cu_consumed = cu;
            }
        } else if let Some(program) = rest.strip_suffix(" success") {
            if let Some(idx) = pop_matching(&mut stack, &spans, program) {
                spans[idx].success = true;
                spans[idx].end = i + 1;
            }
        } else if let Some(pos) = rest.find(" failed: ") {
            let program = &rest[..pos];
            if let Some(idx) = pop_matching(&mut stack, &spans, program) {
                spans[idx].success = false;
                spans[idx].failure = Some(rest[pos + 9..].to_string());
                spans[idx].end = i + 1;
            }
        }
    }
    // Anything still open (truncated logs) ends at the last line.
    for idx in stack {
        spans[idx].end = logs.len();
    }
    spans
}

fn pop_matching(stack: &mut Vec<usize>, spans: &[LogSpan], program: &str) -> Option<usize> {
    let pos = stack.iter().rposition(|&s| spans[s].program == program)?;
    let idx = stack[pos];
    stack.truncate(pos);
    Some(idx)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn spans_reconstruct_nested_tree() {
        let logs = lines(&[
            "Program A invoke [1]",
            "Program log: hi",
            "Program B invoke [2]",
            "Program B consumed 100 of 200 compute units",
            "Program B success",
            "Program A consumed 500 of 1000 compute units",
            "Program A success",
            "Program C invoke [1]",
            "Program C failed: custom program error: 0x1771",
        ]);
        let s = spans_from_logs(&logs, 0);
        assert_eq!(s.len(), 3);
        assert_eq!(
            (s[0].program.as_str(), s[0].depth, s[0].start, s[0].end),
            ("A", 1, 0, 7)
        );
        assert_eq!(s[0].cu_consumed, Some(500));
        assert!(s[0].success);
        assert_eq!(
            (s[1].program.as_str(), s[1].depth, s[1].start, s[1].end),
            ("B", 2, 2, 5)
        );
        assert_eq!(s[1].cu_consumed, Some(100));
        assert_eq!((s[2].program.as_str(), s[2].depth), ("C", 1));
        assert!(!s[2].success);
        assert_eq!(
            s[2].failure.as_deref(),
            Some("custom program error: 0x1771")
        );
    }

    #[test]
    fn spans_start_from_offset_and_survive_truncation() {
        let logs = lines(&[
            "Program A invoke [1]",
            "Program A success",
            "Program B invoke [1]",
            "Program log: …",
        ]);
        let s = spans_from_logs(&logs, 2);
        assert_eq!(s.len(), 1);
        assert_eq!((s[0].program.as_str(), s[0].start, s[0].end), ("B", 2, 4));
        assert!(!s[0].success);
    }

    #[test]
    fn path_ordering_is_numeric() {
        let mut v = vec!["10", "2", "2.1", "2.0", "0"];
        v.sort_by_key(|p| path_key(p));
        assert_eq!(v, vec!["0", "2", "2.0", "2.1", "10"]);
    }
}
