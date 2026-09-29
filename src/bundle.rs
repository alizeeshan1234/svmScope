//! Bundle replay: an ordered set of dependent transactions replayed in
//! sequence on one SVM, proven against the chain step by step, then mutable
//! at any step. See docs/BUNDLE_PLAN.md.

use crate::analyze::AccountDiff;
use crate::error::{Error, Result};
use crate::jito::{BundleMeta, JitoClient, Segment};
use crate::replay::{to_replay_result, Mutation, Prepared, ReplayContext, ReplayResult};
use crate::scope::{diffs_of, OnchainRecord, Scope};
use crate::utils::resolve_account_keys;
use serde::{Deserialize, Serialize};
use solana_account::Account;
use solana_address::Address;
use std::collections::HashMap;
use std::str::FromStr;

/// The most steps a bundle will replay. A sequence is a bundle or a hand-made
/// list, not a block: past this, one request turns into minutes of CPU.
pub const MAX_STEPS: usize = 16;

/// Mutations to apply, per step index. A step with no entry runs untouched.
pub type StepMutations = Vec<(usize, Vec<Mutation>)>;

/// What the caller hands in. Every variant resolves to an ordered list of
/// signatures; a Jito bundle id and a signature inside a bundle also carry
/// the bundle's metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum BundleInput {
    /// A Jito bundle by id, as the explorer names it.
    BundleId {
        /// The bundle id, as the explorer names it.
        id: String,
    },
    /// Any transaction that landed inside a Jito bundle.
    Signature {
        /// Any signature that landed inside the bundle.
        signature: String,
    },
    /// An explicit ordered list, bundled or not.
    Signatures {
        /// The transactions, in the order they should replay.
        signatures: Vec<String>,
    },
}

/// How the steps relate in time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    /// One slot, back to back: replaying in order on one SVM is exact.
    Consecutive,
    /// Different slots: written accounts carry forward, everything else is
    /// rebuilt at each step's own slot, and disagreements are flagged.
    CrossSlot,
}

/// How a later step names an account an earlier step changed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    /// The later step can change it again.
    Writable,
    /// The later step only reads it.
    Readonly,
}

/// An account one step wrote and a later step names: the wire the cascade
/// actually travels along.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Edge {
    /// The step that changed the account.
    pub from_step: usize,
    /// The later step that names it.
    pub to_step: usize,
    /// The account, as base58.
    pub account: String,
    /// Whether the later step may write it too.
    pub role: Role,
}

/// A carried account whose bytes disagree with the world rebuilt at the later
/// step's own slot. Cross-slot only: between two slots, anything may have
/// touched the account, so the two answers are both defensible and the
/// disagreement is the finding.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Diverged {
    /// The account, as base58.
    pub account: String,
    /// The carried balance differs from the rebuilt one.
    pub lamports_differ: bool,
    /// The carried data differs from the rebuilt data.
    pub data_differs: bool,
}

/// What one step did when the sequence ran.
#[derive(Debug, Clone, Serialize)]
pub struct StepReport {
    /// Position in the sequence.
    pub index: usize,
    /// The transaction's signature.
    pub signature: String,
    /// What the replay produced.
    pub result: ReplayResult,
    /// Accounts the replay changed, decoded where an IDL was available.
    pub diff: Vec<AccountDiff>,
    /// Whether the replay reached the same outcome as the chain.
    pub matches_chain: bool,
    /// Whether the chain recorded a success.
    pub chain_success: bool,
    /// The chain's error, as the RPC encoded it.
    pub chain_error: Option<String>,
    /// Compute units the chain recorded, when it reported them.
    pub chain_compute: Option<u64>,
    /// Accounts this step inherited from earlier steps rather than from its
    /// own world.
    pub carried: Vec<String>,
    /// Carried accounts that disagree with this step's rebuilt world.
    pub diverged: Vec<Diverged>,
}

/// The whole sequence, run.
#[derive(Debug, Clone, Serialize)]
pub struct BundleReport {
    /// How the steps relate in time.
    pub mode: Mode,
    /// The Jito bundle's metadata, when the input named one.
    pub meta: Option<BundleMeta>,
    /// Whether every step matched the chain.
    pub exact: bool,
    /// The first step that did not match, if any.
    pub first_drift: Option<usize>,
    /// One report per step, in order.
    pub steps: Vec<StepReport>,
    /// Which step carried what to which later step.
    pub edges: Vec<Edge>,
}

/// One transaction of the sequence.
#[derive(Debug, Clone)]
pub struct BundleStep {
    /// Position in the sequence.
    pub index: usize,
    /// The transaction's signature.
    pub signature: String,
    /// The slot it landed in.
    pub slot: u64,
    /// Every account the transaction names, lookup tables resolved.
    pub keys: Vec<String>,
    /// What the chain recorded for it.
    pub record: OnchainRecord,
    /// The Jito bundle it landed in, if any.
    pub bundle_id: Option<String>,
    tx: serde_json::Value,
}

/// The sequence, with its worlds built and ready to run.
pub struct Bundle {
    /// How the steps relate in time.
    pub mode: Mode,
    /// The Jito bundle's metadata, when the input named one.
    pub meta: Option<BundleMeta>,
    /// The transactions, in replay order.
    pub steps: Vec<BundleStep>,
    /// The bundle runs the sequence splits into.
    pub segments: Vec<Segment>,
    /// The merged pre-bundle world step 0 starts from.
    pub(crate) world: ReplayContext,
    /// One world per step at its own slot; cross-slot mode rebuilds from
    /// these, and per-step traces borrow them.
    pub(crate) worlds: Vec<ReplayContext>,
}

impl Scope {
    /// Resolve the input to an ordered signature list, fetch each step's
    /// record, build one world per step at its own slot, and merge them
    /// first-wins into the pre-bundle world step 0 starts from.
    pub fn bundle(&self, input: BundleInput) -> Result<Bundle> {
        let client = JitoClient::new()?;

        let (signatures, meta): (Vec<String>, Option<BundleMeta>) = match input {
            BundleInput::BundleId { id } => {
                let meta = client
                    .bundle(&id)?
                    .ok_or_else(|| Error::TransactionNotFound(id.clone()))?;
                (meta.signatures.clone(), Some(meta))
            }
            BundleInput::Signature { signature } => {
                let id = client.bundle_of_signature(&signature)?.ok_or_else(|| {
                    Error::InvalidSpec(format!(
                        "{signature} is not in a Jito bundle; use the signatures input"
                    ))
                })?;
                let meta = client
                    .bundle(&id)?
                    .ok_or_else(|| Error::TransactionNotFound(id.clone()))?;
                (meta.signatures.clone(), Some(meta))
            }
            BundleInput::Signatures { signatures } => {
                let mut unique: Vec<String> = Vec::with_capacity(signatures.len());
                for sig in signatures {
                    if !unique.contains(&sig) {
                        unique.push(sig);
                    }
                }
                if unique.is_empty() {
                    return Err(Error::InvalidSpec(
                        "a bundle needs at least one signature".into(),
                    ));
                }
                (unique, None)
            }
        };

        let segments = client.segments(&signatures)?;

        let mut steps = Vec::with_capacity(signatures.len());
        for (index, input_sig) in signatures.iter().enumerate() {
            let signature = self.resolve_signature(input_sig)?;
            let tx = self.transaction_json(&signature)?;
            let slot = tx["slot"].as_u64().ok_or_else(|| {
                Error::InvalidSpec(format!("{signature} has no slot in its record"))
            })?;
            let keys = resolve_account_keys(&tx);
            let record = OnchainRecord::from_tx_json(&tx);
            let bundle_id = segments
                .iter()
                .find(|s| s.signatures.iter().any(|s| s == input_sig))
                .and_then(|s| s.bundle_id.clone());
            steps.push(BundleStep {
                index,
                signature,
                slot,
                keys,
                record,
                bundle_id,
                tx,
            });
        }

        let first_slot = steps[0].slot;
        let mode = if steps.iter().all(|s| s.slot == first_slot) {
            Mode::Consecutive
        } else {
            Mode::CrossSlot
        };

        let mut worlds = Vec::with_capacity(steps.len());
        for step in &steps {
            worlds.push(self.replay_at(&step.signature, step.slot)?.ctx);
        }

        let mut world = worlds[0].clone();
        for w in &worlds[1..] {
            world.absorb(w);
        }

        Ok(Bundle {
            mode,
            meta,
            steps,
            segments,
            world,
            worlds,
        })
    }
}

/// A carried account against the world rebuilt at the later step's own slot:
/// a difference in either balance or bytes is a divergence, and which one it
/// was decides whether the reader is looking at a transfer or a write.
fn diverged_of(account: &Address, carried: &Account, rebuilt: &Account) -> Option<Diverged> {
    let lamports_differ = carried.lamports != rebuilt.lamports;
    let data_differs = carried.data != rebuilt.data;
    (lamports_differ || data_differs).then(|| Diverged {
        account: account.to_string(),
        lamports_differ,
        data_differs,
    })
}

/// Every account a step changed that a later step also names: the cascade,
/// as a list of wires. Role comes from the later step's own message header,
/// so it says whether that step may change the account again or only read it.
fn edges_of(steps: &[BundleStep], reports: &[StepReport]) -> Vec<Edge> {
    let mut edges = Vec::new();
    for report in reports {
        for changed in &report.diff {
            for later in &steps[report.index + 1..] {
                if !later.keys.contains(&changed.address) {
                    continue;
                }
                let role = if Scope::tx_writes(&later.tx, &changed.address) {
                    Role::Writable
                } else {
                    Role::Readonly
                };
                edges.push(Edge {
                    from_step: report.index,
                    to_step: later.index,
                    account: changed.address.clone(),
                    role,
                });
            }
        }
    }
    edges
}

impl Bundle {
    /// Replay every step in order on one SVM, each starting from what the
    /// steps before it left behind, and compare each against the chain.
    ///
    /// `mutations` attaches to a step by index; a step with no entry runs
    /// exactly as it landed. A mutation applies to the *cascaded* world, so
    /// editing a balance at step 0 is visible to every later step.
    pub fn run(&self, mutations: &StepMutations) -> Result<BundleReport> {
        if self.steps.len() > MAX_STEPS {
            return Err(Error::InvalidSpec(format!(
                "{} steps; a bundle replays up to {MAX_STEPS}",
                self.steps.len()
            )));
        }

        let muts_for = |index: usize| -> &[Mutation] {
            mutations
                .iter()
                .find(|(i, _)| *i == index)
                .map(|(_, m)| m.as_slice())
                .unwrap_or_default()
        };

        // The world the current step runs in: step 0's is the merged
        // pre-bundle state, and every later one is the previous step's
        // post-state carried forward.
        let mut ctx = self.world.clone();
        // Everything any earlier step wrote, newest value winning. This is
        // the cascade itself, and cross-slot mode overwrites only these.
        let mut carried: HashMap<Address, Account> = HashMap::new();
        let mut steps: Vec<StepReport> = Vec::with_capacity(self.steps.len());
        // Divergences are found while cascading *into* a step, so they wait
        // here until that step's report is built.
        let mut pending_diverged: Vec<Diverged> = Vec::new();

        for (k, step) in self.steps.iter().enumerate() {
            let Prepared { mut svm, tx, .. } = ctx.prepare(muts_for(k), false)?;
            // After `prepare`, not before: the world the transaction actually
            // starts from includes this step's mutations, and measuring from
            // the unmutated load would report the edit as the step's effect.
            let pre = ctx.loaded_state_of(&svm);
            let result = to_replay_result(svm.send_transaction(tx));
            let post = ctx.loaded_state_of(&svm);

            let diff = diffs_of(
                &|address| pre.get(address).cloned(),
                &post,
                &step.keys,
                ctx.idl_map(),
            );

            let carried_here: Vec<String> = step
                .keys
                .iter()
                .filter(|key| Address::from_str(key).is_ok_and(|a| carried.contains_key(&a)))
                .cloned()
                .collect();

            // Outcome only. LiteSVM formats its errors as text and the RPC
            // returns JSON, so identical failures compare unequal as strings;
            // both are kept in the report for the reader to compare.
            let matches_chain = result.success == step.record.success;

            steps.push(StepReport {
                index: k,
                signature: step.signature.clone(),
                result,
                diff,
                matches_chain,
                chain_success: step.record.success,
                chain_error: step.record.error.clone(),
                chain_compute: step.record.compute_units,
                carried: carried_here,
                diverged: std::mem::take(&mut pending_diverged),
            });

            // Everything this step wrote joins the cascade.
            for (address, account) in &post {
                carried.insert(*address, account.clone());
            }

            if k + 1 >= self.steps.len() {
                break;
            }
            let mut next = match self.mode {
                // One slot: the previous step's post-state *is* the next
                // step's pre-state, so carry the whole world forward.
                Mode::Consecutive => {
                    let mut next = ctx.with_transaction(self.worlds[k + 1].transaction().clone());
                    for (address, account) in &post {
                        next.set_loaded_data(*address, Some(account.clone()));
                    }
                    next
                }
                // Different slots: the next step's own world is the better
                // answer for everything except what earlier steps wrote.
                Mode::CrossSlot => {
                    let rebuilt = &self.worlds[k + 1];
                    let mut next = rebuilt.clone();
                    for (address, account) in &carried {
                        if let Some(existing) = rebuilt.loaded_data_of(address) {
                            pending_diverged.extend(diverged_of(address, account, &existing));
                        }
                        next.set_loaded_data(*address, Some(account.clone()));
                    }
                    next
                }
            };

            // Accounts a step creates (a fresh token account, a new position)
            // are in no world's loaded set, so a later step that names one
            // would load nothing. Take them from the SVM that just made them.
            for later in &self.steps[k + 1..] {
                for key in &later.keys {
                    let Ok(address) = Address::from_str(key) else {
                        continue;
                    };
                    if next.has_loaded(&address) {
                        continue;
                    }
                    if let Some(account) = svm.get_account(&address) {
                        next.set_loaded_data(address, Some(account));
                    }
                }
            }
            ctx = next;
        }

        let exact = steps.iter().all(|s| s.matches_chain);
        let first_drift = steps.iter().position(|s| !s.matches_chain);

        let edges = edges_of(&self.steps, &steps);

        Ok(BundleReport {
            mode: self.mode,
            meta: self.meta.clone(),
            exact,
            first_drift,
            steps,
            edges,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    const MULTI_ID: &str = "05a6d9981728cd177f642c8157877bdfa72668bec370bb18a011665cfb4af1e0";
    const BIG_ID: &str = "71e899723de972d9e5b286ed1e1a6b77a8f60d6029324124c98eeca0fe100912";
    const BIG_SLOT: u64 = 451_735_996;

    fn scope() -> Scope {
        let rpc = std::env::var("RPC")
            .unwrap_or_else(|_| "https://api.mainnet-beta.solana.com".to_string());
        Scope::new(&rpc)
    }

    /// The proof, on a full five-transaction bundle that really does cascade:
    /// every step replays to the chain's own outcome *and* its exact compute
    /// units, each step after the first inherits state from the ones before,
    /// and killing the first step's fee payer changes every later step.
    ///
    /// Replaying one slot back to back is exact in a way a single historical
    /// replay is not: there is no state to rewind between steps, so compute
    /// units match to the unit rather than approximately.
    #[test]
    #[ignore = "network"]
    fn runs_the_five_transaction_bundle_and_cascades_a_mutation() {
        let bundle = scope()
            .bundle(BundleInput::BundleId {
                id: BIG_ID.to_string(),
            })
            .unwrap();
        assert_eq!(bundle.steps.len(), 5, "Jito's maximum bundle size");
        assert_eq!(bundle.mode, Mode::Consecutive);
        assert!(bundle.steps.iter().all(|s| s.slot == BIG_SLOT));
        assert_eq!(bundle.segments.len(), 1);
        assert_eq!(bundle.segments[0].bundle_id.as_deref(), Some(BIG_ID));

        let baseline = bundle.run(&Vec::new()).unwrap();
        assert!(
            baseline.exact,
            "every step must match the chain; first drift at {:?}",
            baseline.first_drift
        );
        assert_eq!(baseline.first_drift, None);
        assert_eq!(baseline.steps.len(), 5);
        for step in &baseline.steps {
            assert!(step.matches_chain, "step {} drifted", step.index);
            assert!(step.result.success && step.chain_success);
            assert_eq!(
                Some(step.result.compute_units),
                step.chain_compute,
                "step {} burned different compute than the chain",
                step.index
            );
            assert!(!step.diff.is_empty(), "step {} changed nothing", step.index);
            assert!(
                step.diverged.is_empty(),
                "one slot cannot diverge: step {}",
                step.index
            );
        }

        // The cascade itself: step 0 starts from the merged world and inherits
        // nothing, and every later step runs on what its predecessors left.
        assert!(baseline.steps[0].carried.is_empty());
        for step in &baseline.steps[1..] {
            assert!(
                !step.carried.is_empty(),
                "step {} inherited nothing from the steps before it",
                step.index
            );
        }

        // Edges only ever point forward, and this bundle's five transactions
        // really do hand accounts along.
        assert!(!baseline.edges.is_empty(), "a cascading bundle has edges");
        assert!(baseline.edges.iter().all(|e| e.from_step < e.to_step));
        assert!(
            baseline.edges.iter().any(|e| e.to_step == 4),
            "the last step should inherit from an earlier one"
        );

        // Take the first step's fee payer away. The step can no longer pay, and
        // because the sequence runs on one SVM every later step sees a world
        // the first step never wrote to.
        let payer = bundle.steps[0].keys[0].clone();
        let mutated = bundle
            .run(&vec![(
                0,
                vec![Mutation::Lamports {
                    address: payer,
                    value: 0,
                }],
            )])
            .unwrap();
        assert!(
            !mutated.steps[0].result.success,
            "a fee payer with no lamports cannot land"
        );
        assert_eq!(mutated.first_drift, Some(0));
        assert!(!mutated.exact);
        for (before, after) in baseline.steps.iter().zip(&mutated.steps).skip(1) {
            assert!(
                after.result.success,
                "step {} should still land on its own",
                after.index
            );
            assert_ne!(
                serde_json::to_string(&before.diff).unwrap(),
                serde_json::to_string(&after.diff).unwrap(),
                "step {} did not feel the change at step 0",
                after.index
            );
        }
    }

    /// A step whose message puts `address` among the writable accounts, or
    /// among the readonly ones when `writable` is false.
    fn synthetic_step(index: usize, keys: &[&str], writable: bool) -> BundleStep {
        // One signer (the fee payer, always writable) plus the keys; marking
        // them readonly-unsigned is what makes `tx_writes` say readonly.
        let mut all: Vec<String> = vec!["Fee1111111111111111111111111111111111111111".to_string()];
        all.extend(keys.iter().map(|k| k.to_string()));
        let readonly_unsigned = if writable { 0 } else { keys.len() };
        let tx = serde_json::json!({
            "slot": 1,
            "meta": { "err": null, "fee": 5000 },
            "transaction": { "message": {
                "accountKeys": all,
                "header": {
                    "numRequiredSignatures": 1,
                    "numReadonlySignedAccounts": 0,
                    "numReadonlyUnsignedAccounts": readonly_unsigned,
                },
            }},
        });
        BundleStep {
            index,
            signature: format!("sig{index}"),
            slot: 1,
            keys: keys.iter().map(|k| k.to_string()).collect(),
            record: OnchainRecord::from_tx_json(&tx),
            bundle_id: None,
            tx,
        }
    }

    fn synthetic_report(index: usize, changed: &[&str]) -> StepReport {
        StepReport {
            index,
            signature: format!("sig{index}"),
            result: ReplayResult {
                success: true,
                error: None,
                error_name: None,
                logs: Vec::new(),
                compute_units: 0,
            },
            diff: changed
                .iter()
                .map(|a| AccountDiff {
                    address: a.to_string(),
                    owner: "11111111111111111111111111111111".to_string(),
                    lamports_before: 1,
                    lamports_after: 2,
                    fields: Vec::new(),
                    raw_data_changed: false,
                })
                .collect(),
            matches_chain: true,
            chain_success: true,
            chain_error: None,
            chain_compute: None,
            carried: Vec::new(),
            diverged: Vec::new(),
        }
    }

    /// An account step 0 writes and step 2 names is an edge from 0 to 2, and
    /// the role is the later step's, not the earlier one's. A step that never
    /// names the account gets no edge, and nothing points backwards.
    #[test]
    fn edges_follow_written_accounts_forward_only() {
        const POOL: &str = "Poo11111111111111111111111111111111111111111";
        let steps = vec![
            synthetic_step(0, &[POOL], true),
            synthetic_step(1, &["Other111111111111111111111111111111111111111"], true),
            synthetic_step(2, &[POOL], false),
        ];
        let reports = vec![
            synthetic_report(0, &[POOL]),
            synthetic_report(1, &[]),
            synthetic_report(2, &[POOL]),
        ];
        let edges = edges_of(&steps, &reports);
        assert_eq!(edges.len(), 1, "only 0 → 2 carries the pool");
        assert_eq!(edges[0].from_step, 0);
        assert_eq!(edges[0].to_step, 2);
        assert_eq!(edges[0].account, POOL);
        // Step 2 only reads the pool, even though step 0 wrote it.
        assert_eq!(edges[0].role, Role::Readonly);
    }

    /// The same account written by two steps and named by a third is two
    /// edges: the cascade table must show both hops, not just the last.
    #[test]
    fn two_writers_of_one_account_give_two_edges() {
        const POOL: &str = "Poo11111111111111111111111111111111111111111";
        let steps = vec![
            synthetic_step(0, &[POOL], true),
            synthetic_step(1, &[POOL], true),
            synthetic_step(2, &[POOL], true),
        ];
        let reports = vec![
            synthetic_report(0, &[POOL]),
            synthetic_report(1, &[POOL]),
            synthetic_report(2, &[POOL]),
        ];
        let edges = edges_of(&steps, &reports);
        let hops: Vec<(usize, usize)> = edges.iter().map(|e| (e.from_step, e.to_step)).collect();
        assert_eq!(hops, vec![(0, 1), (0, 2), (1, 2)]);
        assert!(edges.iter().all(|e| e.role == Role::Writable));
    }

    fn account(lamports: u64, data: &[u8]) -> Account {
        Account {
            lamports,
            data: data.to_vec(),
            owner: Address::from_str("11111111111111111111111111111111").unwrap(),
            executable: false,
            rent_epoch: 0,
        }
    }

    /// Divergence says which half disagreed: a balance that moved without a
    /// write reads differently from bytes that changed under the same balance.
    #[test]
    fn divergence_names_balance_and_bytes_separately() {
        let address = Address::from_str("So11111111111111111111111111111111111111112").unwrap();
        assert_eq!(
            diverged_of(&address, &account(5, b"same"), &account(5, b"same")),
            None,
            "identical accounts have not diverged"
        );
        let balance = diverged_of(&address, &account(5, b"same"), &account(9, b"same")).unwrap();
        assert!(balance.lamports_differ && !balance.data_differs);
        let bytes = diverged_of(&address, &account(5, b"one"), &account(5, b"two")).unwrap();
        assert!(!bytes.lamports_differ && bytes.data_differs);
        let both = diverged_of(&address, &account(5, b"one"), &account(9, b"two")).unwrap();
        assert!(both.lamports_differ && both.data_differs);
        assert_eq!(both.account, address.to_string());
    }

    /// The pinned two-transaction bundle: two consecutive steps in one slot,
    /// one segment, and every account of the second step is already in the
    /// merged world the first step starts from.
    #[test]
    #[ignore = "network"]
    fn builds_the_pinned_two_transaction_bundle() {
        let bundle = scope()
            .bundle(BundleInput::BundleId {
                id: MULTI_ID.to_string(),
            })
            .unwrap();
        assert_eq!(bundle.steps.len(), 2);
        assert_eq!(bundle.mode, Mode::Consecutive);
        assert!(bundle.steps.iter().all(|s| s.slot == 451678738));
        assert_eq!(bundle.segments.len(), 1);
        assert_eq!(bundle.segments[0].bundle_id.as_deref(), Some(MULTI_ID));
        assert_eq!(bundle.meta.as_ref().map(|m| m.id.as_str()), Some(MULTI_ID));
        assert_eq!(bundle.worlds.len(), 2);
        // Builtins and accounts that do not exist are never loaded, so the
        // check is against what step 1's own world holds: none of it may be
        // lost in the merge.
        let mut checked = 0;
        for key in &bundle.steps[1].keys {
            let address = solana_address::Address::from_str(key).unwrap();
            if bundle.worlds[1].has_loaded(&address) {
                assert!(
                    bundle.world.has_loaded(&address),
                    "{key} missing from merged world"
                );
                checked += 1;
            }
        }
        assert!(checked > 0, "step 1 loaded nothing");

        // The merge must be inert for the first step: absorbing step 1's world
        // adds accounts step 0 never names, so step 0 has to replay exactly as
        // it does from its own world alone. Both runs come from the *same*
        // fetch on purpose. Building a second world over the network would
        // compare two different worlds: the free tier reads current accounts
        // and rewinds them, so a world built minutes later has moved on, and
        // the compute units drift in either direction for reasons that have
        // nothing to do with the merge.
        let report = bundle.run(&Vec::new()).unwrap();
        let Prepared { mut svm, tx, .. } = bundle.worlds[0].prepare(&[], false).unwrap();
        let alone = crate::replay::to_replay_result(svm.send_transaction(tx));
        assert_eq!(
            report.steps[0].result.success, alone.success,
            "step 0 in the bundle disagreed with step 0 from its own world"
        );
        assert_eq!(
            report.steps[0].result.compute_units, alone.compute_units,
            "the merge moved step 0's compute"
        );
        assert_eq!(report.steps[0].result.error, alone.error);
    }
}
