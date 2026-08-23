//! Prints how many compute units each program consumed, parsed from the logs.

use serde_json::Value;

#[derive(serde::Serialize)]
pub struct  CuUsage {
    pub program: String,
    pub cu: u64
}

/// Print how many compute units each program consumed.
pub fn cu_per_program(tx: &Value) -> Vec<CuUsage> {
    let log_messages = tx["meta"]["logMessages"]
        .as_array()
        .expect("logMessages should be an array");

    let mut cu_usage: Vec<CuUsage> = Vec::new();
    for line in log_messages {
        let line = line.as_str().expect("logMessage should be str");
        // Only "consumed" lines carry CU info; every other line is skipped.
        if line.contains("consumed") {
            let words: Vec<&str> = line.split_whitespace().collect();
            let program_id = words[1];
            let cu: u64 = words[3].parse().expect("CU should be a number");

            cu_usage.push(CuUsage {
                program: program_id.to_string(),
                cu: cu
            });
        }
    }

    cu_usage
}
