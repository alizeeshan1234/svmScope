//! How many compute units each program consumed.
//!
//! SBF programs log `consumed N of M compute units`, so their usage is exact.
//! Native/builtin programs (System, the loaders, Vote, …) don't log CU at all —
//! but the transaction's *total* is in metadata, and the simplest builtins have
//! fixed per-instruction costs. So we attribute: SBF from logs, the fixed
//! builtins from their known cost × invocation count, and whatever's left of the
//! metadata total to the remaining native program(s). That reconstructs the
//! total exactly for the common shapes (transfers, priority fees, deploys) —
//! e.g. a program deploy becomes `System 300 · loader <the rest>` instead of an
//! empty "no compute recorded".

use serde_json::Value;
use std::collections::HashMap;

#[derive(serde::Serialize)]
pub struct CuUsage {
    pub program: String,
    pub cu: u64,
}

/// Per-instruction CU for the builtins whose cost is a stable constant. Anything
/// not here (loaders with size-dependent deploy cost, etc.) is left to absorb the
/// metadata remainder, which is its actual cost when it's the only such program.
const FIXED_BUILTIN_CU: &[(&str, u64)] = &[
    ("11111111111111111111111111111111", 150), // System Program
    ("ComputeBudget111111111111111111111111111111", 150), // Compute Budget
];

fn fixed_cu(program: &str) -> Option<u64> {
    FIXED_BUILTIN_CU
        .iter()
        .find(|(id, _)| *id == program)
        .map(|(_, c)| *c)
}

/// Compute units per program, aggregated (a program invoked N times is summed
/// into one row) and sorted by consumption, descending.
pub fn cu_per_program(tx: &Value) -> Vec<CuUsage> {
    let Some(log_messages) = tx["meta"]["logMessages"].as_array() else {
        return vec![];
    };
    let total = tx["meta"]["computeUnitsConsumed"].as_u64();

    let mut order: Vec<String> = Vec::new(); // first-seen order, stable output
    let push = |order: &mut Vec<String>, p: &str| {
        if !order.iter().any(|x| x == p) {
            order.push(p.to_string());
        }
    };
    let mut sbf: HashMap<String, u64> = HashMap::new(); // exact, from "consumed" lines
    let mut invokes: HashMap<String, u64> = HashMap::new(); // invocation counts

    for line in log_messages {
        let line = line.as_str().unwrap_or_default();
        let w: Vec<&str> = line.split_whitespace().collect();
        // "Program <id> invoke [n]"
        if w.len() >= 3 && w[0] == "Program" && w[2] == "invoke" {
            push(&mut order, w[1]);
            *invokes.entry(w[1].to_string()).or_insert(0) += 1;
        }
        // "Program <id> consumed N of M compute units"
        if w.len() >= 4
            && w[0] == "Program"
            && line.contains("consumed")
            && line.contains("compute units")
        {
            if let Ok(cu) = w[3].parse::<u64>() {
                push(&mut order, w[1]);
                *sbf.entry(w[1].to_string()).or_insert(0) += cu;
            }
        }
    }

    // Assign what we know exactly; collect the rest for the remainder.
    let mut cu: HashMap<String, u64> = HashMap::new();
    let mut attributed: u64 = 0;
    let mut unattributed: Vec<String> = Vec::new();
    for p in &order {
        if let Some(&v) = sbf.get(p) {
            cu.insert(p.clone(), v);
            attributed += v;
        } else if let Some(fc) = fixed_cu(p) {
            let v = fc * invokes.get(p).copied().unwrap_or(1);
            cu.insert(p.clone(), v);
            attributed += v;
        } else {
            unattributed.push(p.clone());
        }
    }

    // Distribute the metadata remainder across the native program(s) we couldn't
    // price directly. One such program (the common case) gets its exact cost; if
    // several, split by invocation count with the last absorbing rounding so the
    // rows still sum to the true total.
    if let Some(total) = total {
        let remainder = total.saturating_sub(attributed);
        if remainder > 0 && !unattributed.is_empty() {
            let inv_total: u64 = unattributed
                .iter()
                .map(|p| invokes.get(p).copied().unwrap_or(1))
                .sum::<u64>()
                .max(1);
            let mut assigned = 0u64;
            let n = unattributed.len();
            for (i, p) in unattributed.iter().enumerate() {
                let share = if i == n - 1 {
                    remainder - assigned
                } else {
                    remainder * invokes.get(p).copied().unwrap_or(1) / inv_total
                };
                cu.insert(p.clone(), share);
                assigned += share;
            }
        }
    }

    let mut out: Vec<CuUsage> = order
        .iter()
        .filter_map(|p| {
            let v = *cu.get(p).unwrap_or(&0);
            (v > 0).then(|| CuUsage {
                program: p.clone(),
                cu: v,
            })
        })
        .collect();
    out.sort_by_key(|c| std::cmp::Reverse(c.cu));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn sums_per_program_and_sorts_descending() {
        let tx = json!({ "meta": { "logMessages": [
            "Program AAA invoke [1]",
            "Program AAA consumed 100 of 200000 compute units",
            "Program BBB consumed 5000 of 200000 compute units",
            "Program AAA consumed 400 of 200000 compute units",
            "Program AAA success"
        ]}});
        let cu = cu_per_program(&tx);
        assert_eq!(cu.len(), 2);
        assert_eq!((cu[0].program.as_str(), cu[0].cu), ("BBB", 5000));
        assert_eq!((cu[1].program.as_str(), cu[1].cu), ("AAA", 500));
    }

    #[test]
    fn no_logs_means_no_rows() {
        assert!(cu_per_program(&json!({})).is_empty());
    }

    #[test]
    fn attributes_native_only_deploy_from_total_and_builtin_costs() {
        // A program-deploy tx: only native programs, none of which log CU. System
        // is invoked twice (150 each), the loader's cost is the metadata remainder.
        let tx = json!({ "meta": {
            "computeUnitsConsumed": 2670,
            "logMessages": [
                "Program 11111111111111111111111111111111 invoke [1]",
                "Program 11111111111111111111111111111111 success",
                "Program BPFLoaderUpgradeab1e11111111111111111111111 invoke [1]",
                "Program 11111111111111111111111111111111 invoke [2]",
                "Program 11111111111111111111111111111111 success",
                "Program BPFLoaderUpgradeab1e11111111111111111111111 success"
            ]
        }});
        let cu = cu_per_program(&tx);
        // loader gets 2670 - (2 * 150) = 2370; System gets 300; rows sum to total.
        assert_eq!(cu.len(), 2);
        assert_eq!(
            (cu[0].program.as_str(), cu[0].cu),
            ("BPFLoaderUpgradeab1e11111111111111111111111", 2370)
        );
        assert_eq!(
            (cu[1].program.as_str(), cu[1].cu),
            ("11111111111111111111111111111111", 300)
        );
        assert_eq!(cu.iter().map(|c| c.cu).sum::<u64>(), 2670);
    }

    #[test]
    fn sol_transfer_shows_system_cost() {
        let tx = json!({ "meta": {
            "computeUnitsConsumed": 150,
            "logMessages": [
                "Program 11111111111111111111111111111111 invoke [1]",
                "Program 11111111111111111111111111111111 success"
            ]
        }});
        let cu = cu_per_program(&tx);
        assert_eq!(cu.len(), 1);
        assert_eq!(
            (cu[0].program.as_str(), cu[0].cu),
            ("11111111111111111111111111111111", 150)
        );
    }
}
