//! Lift: rebuild a landed transaction with composition moved from programs
//! into transaction construction, and measure what that costs and where it
//! is impossible.
//!
//! A router such as Jupiter takes one instruction and calls the venues from
//! inside its program. The lifted transaction drops the router instruction
//! and puts the venue calls it made, exactly as it made them, at the top
//! level, so the client composes them instead. Both versions are simulated
//! on the same state. The report says whether the user ends up with the
//! same balances, how much compute the on-chain composition cost, whether
//! the lifted transaction would even fit in a packet, and which inner calls
//! cannot be lifted because they needed a signature only the router's
//! program-derived address could give.

use std::collections::{HashMap, HashSet};
use std::str::FromStr;

use base64::Engine;
use serde::{Deserialize, Serialize};
use solana_address::Address;
use solana_message::{
    compiled_instruction::CompiledInstruction,
    v0::{Message as V0Message, MessageAddressTableLookup},
    MessageHeader, VersionedMessage,
};

use crate::analyze::AccountDiff;
use crate::error::{Error, Result};
use crate::scope::Scope;

/// Solana's transaction size limit, in bytes.
pub const PACKET_LIMIT: usize = 1232;

const SPL_TOKEN: &str = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA";
const TOKEN_2022: &str = "TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb";

/// Programs whose inner calls are plumbing, not composition: their CPIs are
/// signed by their own program-derived addresses by design (an associated
/// token account is created by the ATA program signing for it), so lifting
/// them proves nothing. They are left in place unless named explicitly.
const HELPERS: &[&str] = &[
    "ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL",
    "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA",
    "TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb",
    "11111111111111111111111111111111",
    "ComputeBudget111111111111111111111111111111",
    "MemoSq4gqABAXKb96qnH8TysNcWxMyWCqXgDLGmfcHr",
];

/// One top-level instruction that made inner calls, and what happened to it.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RouterLift {
    /// Its position in the original transaction.
    pub index: usize,
    /// The router program.
    pub program: String,
    /// How many direct inner calls it made; each becomes a top-level
    /// instruction in the lifted transaction.
    pub inner_calls: usize,
    /// The programs of those calls, in order.
    pub callees: Vec<String>,
}

/// An inner call the lifted transaction could not execute, and why.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Blocked {
    /// Position of the failing instruction in the lifted transaction.
    pub lifted_index: usize,
    /// The program it calls.
    pub program: String,
    /// The router it was lifted out of.
    pub router: String,
    /// What the simulation said.
    pub reason: String,
}

/// One simulated run, original or lifted, reduced to what the comparison needs.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LiftRun {
    /// Whether it succeeded.
    pub success: bool,
    /// The failure, when it did not.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Compute units consumed.
    pub compute_units: u64,
    /// Instructions at the top level.
    pub instructions: usize,
    /// Serialized size in bytes, signatures included.
    pub size_bytes: usize,
    /// Token balance change of every token account the transaction touched:
    /// address to signed change in base units.
    pub token_deltas: HashMap<String, i128>,
    /// The fee payer's lamport change.
    pub payer_lamport_delta: i64,
}

/// The whole comparison.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LiftReport {
    /// The landed transaction.
    pub signature: String,
    /// Its fee payer, whose balances decide equivalence.
    pub payer: String,
    /// Top-level instructions that made inner calls.
    pub routers: Vec<RouterLift>,
    /// The original, replayed on the state it ran in.
    pub original: LiftRun,
    /// The lifted transaction, simulated on the same state.
    pub lifted: LiftRun,
    /// Inner calls the lifted transaction could not execute.
    pub blocked: Vec<Blocked>,
    /// Inner calls a router made to itself, dropped from the lift: Anchor
    /// programs emit events by invoking themselves with a program-derived
    /// signer, which is logging, not composition.
    pub dropped_self_calls: usize,
    /// Every instruction of the lifted transaction, in order.
    pub lifted_instructions: Vec<LiftedInstruction>,
    /// The last log lines of the lifted run when it failed, for the why.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub lifted_logs: Vec<String>,
    /// Both succeeded and every token balance the transaction touched moved
    /// by the same amount in both; false when neither moved any.
    pub equivalent: bool,
    /// `original.compute_units - lifted.compute_units` when both succeeded:
    /// what on-chain composition cost.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub compute_saved: Option<i64>,
    /// Whether the lifted transaction is within the packet limit.
    pub fits_packet: bool,
    /// The lifted transaction, unsigned, base64, for anyone to inspect.
    pub lifted_tx_b64: String,
    /// One line.
    pub verdict: String,
}

/// One instruction of the lifted transaction, as the report shows it.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LiftedInstruction {
    /// The program it calls.
    pub program: String,
    /// The router it was lifted out of, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from_router: Option<String>,
    /// Accounts as (address, signer, writable).
    pub accounts: Vec<(String, bool, bool)>,
    /// Instruction data length in bytes.
    pub data_len: usize,
}

/// A decoded instruction with full account metas, before compilation.
struct Lifted {
    program: String,
    accounts: Vec<(String, bool, bool)>, // address, signer, writable
    data: Vec<u8>,
    router: Option<String>,
}

fn b58(s: &str) -> Result<Vec<u8>> {
    bs58::decode(s)
        .into_vec()
        .map_err(|e| Error::MalformedRpcResponse(format!("bad base58: {e}")))
}

/// Token balance deltas of every token account the transaction changed,
/// from the replay's account diffs: token accounts decode into named fields,
/// and `amount` is the balance. All of them, not only the payer's: a lifted
/// transaction is equivalent only if every venue, fee and user account
/// moved by the same amount, and the router's own fee accounts are the first
/// place a difference would show.
fn token_deltas(diffs: &[AccountDiff]) -> HashMap<String, i128> {
    let mut out = HashMap::new();
    for d in diffs {
        if d.owner != SPL_TOKEN && d.owner != TOKEN_2022 {
            continue;
        }
        if let Some(f) = d.fields.iter().find(|f| f.name == "amount") {
            let parse = |s: &str| s.replace(['_', ','], "").parse::<i128>().ok();
            if let (Some(b), Some(a)) = (parse(&f.before), parse(&f.after)) {
                out.insert(d.address.clone(), a - b);
            }
        }
    }
    out
}

fn run_of(replayed: &crate::Replayed, payer: &str, instructions: usize, size: usize) -> LiftRun {
    let payer_lamport_delta = replayed
        .diffs
        .iter()
        .find(|d| d.address == payer)
        .map(|d| d.lamports_after as i64 - d.lamports_before as i64)
        .unwrap_or(0);
    LiftRun {
        success: replayed.result.success,
        error: replayed.result.error.clone(),
        compute_units: replayed.result.compute_units,
        instructions,
        size_bytes: size,
        token_deltas: token_deltas(&replayed.diffs),
        payer_lamport_delta,
    }
}

impl Scope {
    /// Lift a landed transaction: see the module documentation. `router`
    /// names the one program to lift out; without it every top-level
    /// instruction that made inner calls is lifted, except the helpers whose
    /// inner calls are plumbing rather than composition.
    pub fn lift(&self, signature: &str, router: Option<&str>) -> Result<LiftReport> {
        self.lift_with(signature, router, false)
    }

    /// [`Scope::lift`] with `exact`: replay at the transaction's own slot, so
    /// the original executes as it did on chain instead of on reconstructed
    /// state that may have drifted. Slower, and needs the history stream.
    pub fn lift_with(
        &self,
        signature: &str,
        router: Option<&str>,
        exact: bool,
    ) -> Result<LiftReport> {
        let signature = self.resolve_signature(signature)?;
        let tx_json = self.transaction_json(&signature)?;
        let original = crate::replay::fetch_transaction(self.client(), &signature)?;

        let message = &tx_json["transaction"]["message"];
        let meta = &tx_json["meta"];
        let static_keys: Vec<String> = message["accountKeys"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|k| k.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();
        let loaded_writable: Vec<String> = meta["loadedAddresses"]["writable"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|k| k.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();
        let loaded_readonly: Vec<String> = meta["loadedAddresses"]["readonly"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|k| k.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();
        let mut keys = static_keys.clone();
        keys.extend(loaded_writable.iter().cloned());
        keys.extend(loaded_readonly.iter().cloned());
        let payer = static_keys
            .first()
            .cloned()
            .ok_or_else(|| Error::MalformedRpcResponse("transaction has no accounts".into()))?;

        // Signers and writable accounts of the original: the privileges a
        // lifted call may carry are exactly these, since CPI cannot add any.
        let header = &message["header"];
        let n_sig = header["numRequiredSignatures"].as_u64().unwrap_or(1) as usize;
        let n_ro_signed = header["numReadonlySignedAccounts"].as_u64().unwrap_or(0) as usize;
        let n_ro_unsigned = header["numReadonlyUnsignedAccounts"].as_u64().unwrap_or(0) as usize;
        let signers: HashSet<String> = static_keys.iter().take(n_sig).cloned().collect();
        let mut writable: HashSet<String> = HashSet::new();
        for (i, k) in static_keys.iter().enumerate() {
            let w = if i < n_sig {
                i < n_sig - n_ro_signed
            } else {
                i < static_keys.len() - n_ro_unsigned
            };
            if w {
                writable.insert(k.clone());
            }
        }
        writable.extend(loaded_writable.iter().cloned());

        // Where each loaded address came from: (table position, index), so
        // the lifted message can reference the same tables.
        let mut table_of: HashMap<String, (usize, u8, bool)> = HashMap::new();
        let lookups = message["addressTableLookups"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        let (mut wi, mut ri) = (0usize, 0usize);
        for (t, l) in lookups.iter().enumerate() {
            for idx in l["writableIndexes"].as_array().cloned().unwrap_or_default() {
                if let (Some(addr), Some(i)) = (loaded_writable.get(wi), idx.as_u64()) {
                    table_of.insert(addr.clone(), (t, i as u8, true));
                }
                wi += 1;
            }
            for idx in l["readonlyIndexes"].as_array().cloned().unwrap_or_default() {
                if let (Some(addr), Some(i)) = (loaded_readonly.get(ri), idx.as_u64()) {
                    table_of.insert(addr.clone(), (t, i as u8, false));
                }
                ri += 1;
            }
        }

        // Inner calls, grouped by the top-level instruction that made them.
        let mut inner_by_top: HashMap<usize, Vec<serde_json::Value>> = HashMap::new();
        for g in meta["innerInstructions"]
            .as_array()
            .cloned()
            .unwrap_or_default()
        {
            if let Some(i) = g["index"].as_u64() {
                inner_by_top.insert(
                    i as usize,
                    g["instructions"].as_array().cloned().unwrap_or_default(),
                );
            }
        }

        let decode = |ix: &serde_json::Value| -> Result<(String, Vec<String>, Vec<u8>)> {
            let program = keys
                .get(ix["programIdIndex"].as_u64().unwrap_or(u64::MAX) as usize)
                .cloned()
                .ok_or_else(|| Error::MalformedRpcResponse("program index out of range".into()))?;
            let accounts: Vec<String> = ix["accounts"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|i| keys.get(i.as_u64()? as usize).cloned())
                        .collect()
                })
                .unwrap_or_default();
            let data = b58(ix["data"].as_str().unwrap_or(""))?;
            Ok((program, accounts, data))
        };

        let mut routers = Vec::new();
        let mut lifted: Vec<Lifted> = Vec::new();
        let mut dropped_self_calls = 0usize;
        let top: Vec<serde_json::Value> = message["instructions"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        for (i, ix) in top.iter().enumerate() {
            let (program, accounts, data) = decode(ix)?;
            let metas = |accounts: &[String]| -> Vec<(String, bool, bool)> {
                accounts
                    .iter()
                    .map(|a| (a.clone(), signers.contains(a), writable.contains(a)))
                    .collect()
            };
            let lift_this = match router {
                Some(r) => program == r,
                None => !HELPERS.contains(&program.as_str()),
            };
            match inner_by_top.get(&i) {
                Some(inner) if !inner.is_empty() && lift_this => {
                    // Direct children only: their own inner calls stay theirs.
                    let children: Vec<&serde_json::Value> = inner
                        .iter()
                        .filter(|c| c["stackHeight"].as_u64().unwrap_or(2) == 2)
                        .collect();
                    let mut callees = Vec::new();
                    for c in children {
                        let (cp, ca, cd) = decode(c)?;
                        if cp == program {
                            dropped_self_calls += 1;
                            continue;
                        }
                        callees.push(cp.clone());
                        lifted.push(Lifted {
                            program: cp,
                            accounts: metas(&ca),
                            data: cd,
                            router: Some(program.clone()),
                        });
                    }
                    routers.push(RouterLift {
                        index: i,
                        program,
                        inner_calls: callees.len(),
                        callees,
                    });
                }
                _ => lifted.push(Lifted {
                    program,
                    accounts: metas(&accounts),
                    data,
                    router: None,
                }),
            }
        }
        if routers.is_empty() {
            return Err(Error::InvalidSpec(match router {
                Some(r) => format!("{r} is not a top-level instruction of this transaction, or made no inner calls"),
                None => "no instruction in this transaction made inner calls; there is nothing to lift".into(),
            }));
        }

        // Compile the lifted message: signers first, then static accounts
        // writable then readonly, then loaded addresses through the original
        // tables, writable then readonly, which is the v0 key order.
        let mut used: Vec<String> = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();
        for l in &lifted {
            for (a, _, _) in &l.accounts {
                if seen.insert(a.clone()) {
                    used.push(a.clone());
                }
            }
            if seen.insert(l.program.clone()) {
                used.push(l.program.clone());
            }
        }
        let is_signer = |a: &str| signers.contains(a);
        let is_writable = |a: &str| writable.contains(a);
        // Programs invoked at the top level must be static keys (a v0 message
        // may not take a program id from a table), and read-only.
        let programs: HashSet<String> = lifted.iter().map(|l| l.program.clone()).collect();
        // Signers: writable ones first, read-only ones last, payer first of all.
        let mut static_new: Vec<String> = vec![payer.clone()];
        for a in used
            .iter()
            .filter(|a| is_signer(a) && **a != payer && is_writable(a))
        {
            static_new.push(a.clone());
        }
        for a in used
            .iter()
            .filter(|a| is_signer(a) && **a != payer && !is_writable(a))
        {
            static_new.push(a.clone());
        }
        let signed_ro = static_new.iter().filter(|a| !is_writable(a)).count();
        let n_signed = static_new.len();
        let mut loaded_w: Vec<String> = Vec::new();
        let mut loaded_r: Vec<String> = Vec::new();
        let mut static_w: Vec<String> = Vec::new();
        let mut static_r: Vec<String> = Vec::new();
        for a in used.iter().filter(|a| !is_signer(a)) {
            if programs.contains(a) {
                static_r.push(a.clone());
                continue;
            }
            match table_of.get(a) {
                Some((_, _, true)) => loaded_w.push(a.clone()),
                Some((_, _, false)) => loaded_r.push(a.clone()),
                None if is_writable(a) => static_w.push(a.clone()),
                None => static_r.push(a.clone()),
            }
        }
        let ro_unsigned = static_r.len();
        static_new.extend(static_w);
        static_new.extend(static_r);
        let mut order: Vec<String> = static_new.clone();
        order.extend(loaded_w.iter().cloned());
        order.extend(loaded_r.iter().cloned());
        let index_of: HashMap<&str, u8> = order
            .iter()
            .enumerate()
            .map(|(i, a)| (a.as_str(), i as u8))
            .collect();
        if order.len() > 256 {
            return Err(Error::InvalidSpec(format!(
                "the lifted transaction needs {} accounts; a message may hold 256",
                order.len()
            )));
        }
        let mut table_lookups: Vec<MessageAddressTableLookup> = lookups
            .iter()
            .map(|l| MessageAddressTableLookup {
                account_key: Address::from_str(l["accountKey"].as_str().unwrap_or_default())
                    .unwrap_or_default(),
                writable_indexes: Vec::new(),
                readonly_indexes: Vec::new(),
            })
            .collect();
        for a in &loaded_w {
            if let Some((t, i, _)) = table_of.get(a) {
                table_lookups[*t].writable_indexes.push(*i);
            }
        }
        for a in &loaded_r {
            if let Some((t, i, _)) = table_of.get(a) {
                table_lookups[*t].readonly_indexes.push(*i);
            }
        }
        table_lookups.retain(|l| !l.writable_indexes.is_empty() || !l.readonly_indexes.is_empty());

        let instructions: Vec<CompiledInstruction> = lifted
            .iter()
            .map(|l| CompiledInstruction {
                program_id_index: index_of[l.program.as_str()],
                accounts: l
                    .accounts
                    .iter()
                    .map(|(a, _, _)| index_of[a.as_str()])
                    .collect(),
                data: l.data.clone(),
            })
            .collect();
        let v0 = V0Message {
            header: MessageHeader {
                num_required_signatures: n_signed as u8,
                num_readonly_signed_accounts: signed_ro as u8,
                num_readonly_unsigned_accounts: ro_unsigned as u8,
            },
            account_keys: static_new
                .iter()
                .map(|a| Address::from_str(a).unwrap_or_default())
                .collect(),
            recent_blockhash: *original.message.recent_blockhash(),
            instructions,
            address_table_lookups: table_lookups,
        };
        let lifted_message = VersionedMessage::V0(v0);
        let lifted_tx = solana_transaction::versioned::VersionedTransaction {
            signatures: vec![Default::default(); n_signed],
            message: lifted_message.clone(),
        };
        let lifted_bytes = bincode::serialize(&lifted_tx)
            .map_err(|e| Error::InvalidSpec(format!("serialize lifted transaction: {e}")))?;
        let lifted_b64 = base64::engine::general_purpose::STANDARD.encode(&lifted_bytes);
        let original_size = bincode::serialize(&original).map(|b| b.len()).unwrap_or(0);

        // Both run on the world the original ran in, rebuilt as the replay
        // engine always does: balances rewound to before the transaction,
        // the clock at its slot. Current state would fail most swaps on
        // slippage before anything could be compared.
        let replay = if exact {
            self.replay_at_slot(&signature)?
        } else {
            self.replay(&signature)?
        };
        let original_run = replay.run()?;
        let lifted_run = replay.run_transaction(lifted_tx)?;
        let original_stats = run_of(&original_run, &payer, top.len(), original_size);
        let lifted_stats = run_of(&lifted_run, &payer, lifted.len(), lifted_bytes.len());

        // Which lifted instruction failed, and out of which router it came.
        let mut blocked = Vec::new();
        if !lifted_stats.success {
            let err = lifted_stats.error.clone().unwrap_or_default();
            let at = err
                .split("InstructionError(")
                .nth(1)
                .and_then(|s| s.split(',').next())
                .and_then(|s| s.trim().parse::<usize>().ok());
            if let Some(k) = at {
                if let Some(l) = lifted.get(k) {
                    // Anchor's ConstraintSigner (2002) and ConstraintSeeds (2006)
                    // are the same fact in the callee's words: a program-derived
                    // signer that only exists inside a CPI.
                    let pda_signed = err.contains("MissingRequiredSignature")
                        || err.contains("Custom(2002)")
                        || err.contains("Custom(2006)");
                    let logs_tail: Vec<String> = lifted_run
                        .result
                        .logs
                        .iter()
                        .filter(|l| l.starts_with("Program log:") || l.contains("failed"))
                        .rev()
                        .take(3)
                        .map(|l| l.trim_start_matches("Program log: ").to_string())
                        .collect::<Vec<_>>()
                        .into_iter()
                        .rev()
                        .collect();
                    let reason = if pda_signed {
                        format!(
                            "needs a signer only the router's program-derived address could provide; cannot exist as a top-level instruction ({err})"
                        )
                    } else if logs_tail.is_empty() {
                        err.clone()
                    } else {
                        format!("{err}: {}", logs_tail.join(" | "))
                    };
                    blocked.push(Blocked {
                        lifted_index: k,
                        program: l.program.clone(),
                        router: l.router.clone().unwrap_or_default(),
                        reason,
                    });
                }
            }
        }

        let equivalent = original_stats.success
            && lifted_stats.success
            && !original_stats.token_deltas.is_empty()
            && original_stats.token_deltas == lifted_stats.token_deltas;
        let compute_saved = if original_stats.success && lifted_stats.success {
            Some(original_stats.compute_units as i64 - lifted_stats.compute_units as i64)
        } else {
            None
        };
        let fits_packet = lifted_bytes.len() <= PACKET_LIMIT;
        let verdict = if !original_stats.success {
            "the original does not execute on the reconstructed state (its inputs have drifted since it landed), so nothing can be compared".to_string()
        } else if let Some(b) = blocked.first() {
            format!(
                "cannot be lifted: instruction {} ({}) out of {} {}",
                b.lifted_index, b.program, b.router, b.reason
            )
        } else if equivalent {
            format!(
                "lifted: every token balance moves the same, {} instructions instead of {}, {} CU {} ({} bytes{})",
                lifted_stats.instructions,
                original_stats.instructions,
                compute_saved.unwrap_or(0).abs(),
                if compute_saved.unwrap_or(0) >= 0 { "saved" } else { "more" },
                lifted_bytes.len(),
                if fits_packet { "" } else { ", over the packet limit" }
            )
        } else {
            "both execute but token balances differ; the router did something its callees alone do not"
                .to_string()
        };

        let lifted_instructions: Vec<LiftedInstruction> = lifted
            .iter()
            .map(|l| LiftedInstruction {
                program: l.program.clone(),
                from_router: l.router.clone(),
                accounts: l.accounts.clone(),
                data_len: l.data.len(),
            })
            .collect();
        Ok(LiftReport {
            signature,
            payer,
            routers,
            original: original_stats,
            lifted: lifted_stats,
            blocked,
            dropped_self_calls,
            lifted_instructions,
            lifted_logs: if lifted_run.result.success {
                Vec::new()
            } else {
                lifted_run
                    .result
                    .logs
                    .iter()
                    .rev()
                    .take(24)
                    .rev()
                    .cloned()
                    .collect()
            },
            equivalent,
            compute_saved,
            fits_packet,
            lifted_tx_b64: lifted_b64,
            verdict,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_deltas_come_from_amount_fields() {
        use crate::analyze::FieldDiff;
        let diffs = vec![
            AccountDiff {
                address: "ata1".into(),
                owner: SPL_TOKEN.into(),
                lamports_before: 1,
                lamports_after: 1,
                fields: vec![
                    FieldDiff {
                        name: "owner".into(),
                        ty: "pubkey".into(),
                        before: "payer".into(),
                        after: "payer".into(),
                    },
                    FieldDiff {
                        name: "amount".into(),
                        ty: "u64".into(),
                        before: "1000".into(),
                        after: "1500".into(),
                    },
                ],
                raw_data_changed: false,
            },
            AccountDiff {
                address: "someone_else".into(),
                owner: SPL_TOKEN.into(),
                lamports_before: 1,
                lamports_after: 1,
                fields: vec![
                    FieldDiff {
                        name: "owner".into(),
                        ty: "pubkey".into(),
                        before: "other".into(),
                        after: "other".into(),
                    },
                    FieldDiff {
                        name: "amount".into(),
                        ty: "u64".into(),
                        before: "5".into(),
                        after: "0".into(),
                    },
                ],
                raw_data_changed: false,
            },
        ];
        let d = token_deltas(&diffs);
        assert_eq!(d.get("ata1"), Some(&500));
        assert_eq!(d.get("someone_else"), Some(&-5));
    }
}
