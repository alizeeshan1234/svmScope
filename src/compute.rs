//! Prints how many compute units each program consumed, parsed from the logs.

use serde_json::Value;

/// Print how many compute units each program consumed.
pub fn print_cu_per_program(tx: &Value) {
    let log_messages = tx["meta"]["logMessages"]
        .as_array()
        .expect("logMessages should be an array");

    for line in log_messages {
        let line = line.as_str().expect("logMessage should be str");
        if line.contains("consumed") {
            let words: Vec<&str> = line.split_whitespace().collect();
            let program_id = words[1];
            let cu: u64 = words[3].parse().expect("CU should be a number");
            println!("{:<44} {} CU", program_id, cu);
        }
    }
}
