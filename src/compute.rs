//! How many compute units each program consumed, parsed from the logs.

use serde_json::Value;

#[derive(serde::Serialize)]
pub struct CuUsage {
    pub program: String,
    pub cu: u64,
}

/// Compute units per program, aggregated (a program invoked N times is summed
/// into one row) and sorted by consumption, descending.
pub fn cu_per_program(tx: &Value) -> Vec<CuUsage> {
    let log_messages = match tx["meta"]["logMessages"].as_array() {
        Some(l) => l,
        None => return vec![],
    };

    // First-seen order, so a stable list; totals summed across invocations.
    let mut order: Vec<String> = Vec::new();
    let mut totals: std::collections::HashMap<String, u64> = std::collections::HashMap::new();

    for line in log_messages {
        let line = line.as_str().unwrap_or_default();
        // e.g. "Program <id> consumed 179 of 797649 compute units"
        if line.contains("consumed") && line.contains("compute units") {
            let words: Vec<&str> = line.split_whitespace().collect();
            if words.len() < 4 {
                continue;
            }
            let program_id = words[1].to_string();
            if let Ok(cu) = words[3].parse::<u64>() {
                if !totals.contains_key(&program_id) {
                    order.push(program_id.clone());
                }
                *totals.entry(program_id).or_insert(0) += cu;
            }
        }
    }

    let mut cu_usage: Vec<CuUsage> = order
        .into_iter()
        .map(|program| {
            let cu = totals[&program];
            CuUsage { program, cu }
        })
        .collect();
    cu_usage.sort_by_key(|c| std::cmp::Reverse(c.cu));
    cu_usage
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
}
