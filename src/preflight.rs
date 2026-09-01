//! The pre-sign overview: what a not-yet-sent transaction *is* and what signing
//! it would do — size, fees, fee payer, IDL-named instructions, and
//! plain-English actions with danger flags. This is the layer a wallet's
//! "Base64 message" deserves beyond raw simulation logs: the person pasting it
//! is asking "what am I about to sign, and is it safe?"

use serde::Serialize;
use solana_client::rpc_client::RpcClient;
use solana_transaction::versioned::VersionedTransaction;
use std::collections::HashMap;

use crate::cpi_tree::{IxAccount, IxArg};
use crate::ixname;

const SYSTEM: &str = "11111111111111111111111111111111";
const TOKEN: &str = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA";
const TOKEN_2022: &str = "TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb";
const ATA: &str = "ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL";
const COMPUTE_BUDGET: &str = "ComputeBudget111111111111111111111111111111";

/// Solana's transaction size ceiling (bytes over the wire).
const MAX_TX_SIZE: usize = 1232;
/// Lamports per required signature.
const LAMPORTS_PER_SIGNATURE: u64 = 5_000;
/// The runtime's per-transaction compute ceiling, used to bound a defaulted
/// compute-unit limit when only a price was set.
const MAX_COMPUTE_UNITS: u64 = 1_400_000;
/// Default compute-unit limit granted per (non-ComputeBudget) instruction when
/// no SetComputeUnitLimit is present.
const DEFAULT_CU_PER_IX: u64 = 200_000;

/// One decoded top-level instruction of a pre-flight transaction.
#[derive(Debug, Clone, Serialize)]
pub struct PreflightIx {
    /// Position in the message (0-based).
    pub index: usize,
    /// The program id, base58.
    pub program: String,
    /// The instruction's name from a native layout or the program's IDL.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Decoded arguments, where the layout/IDL is known.
    pub args: Vec<IxArg>,
    /// The instruction's accounts, IDL-named where known.
    pub accounts: Vec<IxAccount>,
}

/// The pre-sign overview block of a [`crate::SimulationReport`].
#[derive(Debug, Clone, Serialize)]
pub struct PreflightOverview {
    /// Serialized transaction size in bytes (signatures included).
    pub serialized_size: usize,
    /// The network's hard cap (1232 bytes).
    pub max_size: usize,
    /// Base fee: 5000 lamports per required signature.
    pub base_fee_lamports: u64,
    /// Priority fee from the ComputeBudget instructions, when a price is set.
    /// Computed from the requested compute-unit limit — or a defaulted one,
    /// flagged in `priority_fee_note`.
    pub priority_fee_lamports: u64,
    /// Present when the priority fee had to assume a defaulted CU limit.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub priority_fee_note: Option<String>,
    /// The requested compute-unit limit, when one was set.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub compute_unit_limit: Option<u64>,
    /// The fee payer (first required signer).
    pub fee_payer: String,
    /// The fee payer's live balance, when it could be fetched.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fee_payer_balance: Option<u64>,
    /// Whether the balance covers base + priority fees.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fee_payer_can_pay: Option<bool>,
    /// Top-level instructions, decoded and named.
    pub instructions: Vec<PreflightIx>,
    /// Plain-English "what signing this does" lines.
    pub actions: Vec<String>,
    /// Danger flags: authority changes, delegations, closes, reassignments.
    pub warnings: Vec<String>,
}

fn short(a: &str) -> String {
    if a.len() > 12 {
        format!("{}…{}", &a[..4], &a[a.len() - 4..])
    } else {
        a.to_string()
    }
}

fn arg<'a>(args: &'a [IxArg], name: &str) -> Option<&'a str> {
    args.iter()
        .find(|a| a.name.eq_ignore_ascii_case(name))
        .map(|a| a.value.as_str())
}

fn lamports_to_sol(l: u64) -> String {
    let sol = l as f64 / 1e9;
    if sol >= 0.0001 {
        format!("{sol:.4} SOL")
    } else {
        format!("{l} lamports")
    }
}

fn account_addr(accounts: &[IxAccount], i: usize) -> String {
    accounts
        .get(i)
        .map(|a| short(&a.address))
        .unwrap_or_else(|| "?".into())
}

/// Translate one decoded instruction into action lines and warnings. Only
/// well-understood native shapes produce output — an unknown instruction adds
/// nothing rather than guessing.
fn describe(
    program: &str,
    name: Option<&str>,
    args: &[IxArg],
    accounts: &[IxAccount],
    actions: &mut Vec<String>,
    warnings: &mut Vec<String>,
) {
    let name = name.unwrap_or("");
    match (program, name) {
        (SYSTEM, "Transfer") => {
            let amt = arg(args, "lamports")
                .and_then(|v| v.parse::<u64>().ok())
                .map(lamports_to_sol)
                .unwrap_or_else(|| "SOL".into());
            actions.push(format!(
                "sends {amt} from {} to {}",
                account_addr(accounts, 0),
                account_addr(accounts, 1)
            ));
        }
        (SYSTEM, "Create Account") | (SYSTEM, "CreateAccount") => {
            actions.push(format!("creates account {}", account_addr(accounts, 1)));
        }
        (SYSTEM, "Assign") => warnings.push(format!(
            "reassigns ownership of {} to another program",
            account_addr(accounts, 0)
        )),
        (TOKEN | TOKEN_2022, "Transfer" | "Transfer Checked" | "TransferChecked") => {
            let amt = arg(args, "amount").unwrap_or("tokens").to_string();
            actions.push(format!(
                "sends {amt} tokens from {} to {}",
                account_addr(accounts, 0),
                // TransferChecked inserts the mint at position 1.
                if name.contains("Checked") {
                    account_addr(accounts, 2)
                } else {
                    account_addr(accounts, 1)
                }
            ));
        }
        (TOKEN | TOKEN_2022, "Approve" | "Approve Checked" | "ApproveChecked") => {
            let amt = arg(args, "amount").unwrap_or("an amount of").to_string();
            warnings.push(format!(
                "delegates {amt} tokens of {} to {} — the delegate can move them without you",
                account_addr(accounts, 0),
                account_addr(accounts, if name.contains("Checked") { 2 } else { 1 })
            ));
        }
        (TOKEN | TOKEN_2022, "Set Authority" | "SetAuthority") => warnings.push(format!(
            "changes the authority of {} — control of the account moves",
            account_addr(accounts, 0)
        )),
        (TOKEN | TOKEN_2022, "Close Account" | "CloseAccount") => warnings.push(format!(
            "closes token account {} and sends its rent to {}",
            account_addr(accounts, 0),
            account_addr(accounts, 1)
        )),
        (TOKEN | TOKEN_2022, "Revoke") => actions.push(format!(
            "revokes the delegate on {}",
            account_addr(accounts, 0)
        )),
        (ATA, "Create" | "Create Idempotent") => {
            actions.push("creates a token account (≈0.002 SOL rent, refundable on close)".into());
        }
        _ => {}
    }
}

/// Build the pre-sign overview for a parsed unsigned transaction. All decoding
/// is local; the only RPC calls are the (cached) IDL fetches for non-native
/// programs, ALT resolution for v0 messages, and one balance lookup.
pub(crate) fn build_overview(
    client: &RpcClient,
    idl_cache: &mut HashMap<String, Option<serde_json::Value>>,
    tx: &VersionedTransaction,
) -> PreflightOverview {
    // Full runtime account ordering: static keys, then ALT writable, then
    // ALT readonly — instruction indexes point into this combined list.
    let mut keys: Vec<String> = tx
        .message
        .static_account_keys()
        .iter()
        .map(|k| k.to_string())
        .collect();
    let (alt_w, alt_r) = crate::replay::resolve_alt_addresses(client, tx);
    keys.extend(alt_w);
    keys.extend(alt_r);

    let serialized_size = bincode::serialize(tx).map(|b| b.len()).unwrap_or(0);
    let n_sigs = tx.message.header().num_required_signatures as u64;
    let base_fee_lamports = n_sigs * LAMPORTS_PER_SIGNATURE;
    let fee_payer = keys.first().cloned().unwrap_or_default();

    // Decode every top-level instruction, and pick up the ComputeBudget
    // requests along the way.
    let mut instructions = Vec::new();
    let mut actions = Vec::new();
    let mut warnings = Vec::new();
    let mut cu_limit: Option<u64> = None;
    let mut cu_price_micro: Option<u64> = None;
    let mut non_cb_ix = 0u64;

    for (index, ix) in tx.message.instructions().iter().enumerate() {
        let program = keys
            .get(ix.program_id_index as usize)
            .cloned()
            .unwrap_or_default();
        let account_indexes: Vec<usize> = ix.accounts.iter().map(|&i| i as usize).collect();
        let (name, args, accounts) = ixname::enrich(
            client,
            idl_cache,
            &program,
            &ix.data,
            &account_indexes,
            &keys,
        );

        if program == COMPUTE_BUDGET {
            // Tags: 2 = SetComputeUnitLimit(u32), 3 = SetComputeUnitPrice(u64).
            match ix.data.first() {
                Some(2) if ix.data.len() >= 5 => {
                    cu_limit =
                        Some(u32::from_le_bytes(ix.data[1..5].try_into().unwrap()) as u64);
                }
                Some(3) if ix.data.len() >= 9 => {
                    cu_price_micro =
                        Some(u64::from_le_bytes(ix.data[1..9].try_into().unwrap()));
                }
                _ => {}
            }
        } else {
            non_cb_ix += 1;
        }

        describe(
            &program,
            name.as_deref(),
            &args,
            &accounts,
            &mut actions,
            &mut warnings,
        );
        instructions.push(PreflightIx {
            index,
            program,
            name,
            args,
            accounts,
        });
    }

    // Priority fee = price (µ-lamports / CU) × CU limit. When no limit was
    // requested, the runtime defaults to 200k per instruction capped at 1.4M —
    // say so instead of silently guessing.
    let (priority_fee_lamports, priority_fee_note) = match cu_price_micro {
        Some(price) => {
            let (limit, note) = match cu_limit {
                Some(l) => (l, None),
                None => (
                    (non_cb_ix * DEFAULT_CU_PER_IX).min(MAX_COMPUTE_UNITS),
                    Some("no compute-unit limit was requested — priority fee estimated at the runtime default".to_string()),
                ),
            };
            ((price * limit).div_ceil(1_000_000), note)
        }
        None => (0, None),
    };

    let fee_payer_balance = client
        .get_balance(&fee_payer.parse().unwrap_or_default())
        .ok();
    let fee_payer_can_pay =
        fee_payer_balance.map(|b| b >= base_fee_lamports + priority_fee_lamports);

    PreflightOverview {
        serialized_size,
        max_size: MAX_TX_SIZE,
        base_fee_lamports,
        priority_fee_lamports,
        priority_fee_note,
        compute_unit_limit: cu_limit,
        fee_payer,
        fee_payer_balance,
        fee_payer_can_pay,
        instructions,
        actions,
        warnings,
    }
}
