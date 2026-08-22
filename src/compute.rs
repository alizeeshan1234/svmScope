//! Compute units (CU) per program — brick #3, the last one for v0.1.
//!
//! The runtime logs how much compute each program burned. Those lines live in:
//!   tx["meta"]["logMessages"]   -> an array of strings.
//!
//! The lines you care about look EXACTLY like this:
//!   "Program TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA consumed 3049 of 200000 compute units"
//!
//! GOAL — print each program and what it consumed, e.g.:
//!
//!   TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA   3049 CU
//!   SwapProg1111111111111111111111111111111111   42817 CU
//!
//! THE NEW SKILL HERE: string parsing. Walk the log lines and pull two things
//! out of each "consumed" line — the program id and the number.
//!
//! HINTS:
//!   - Loop the log lines: `for line in logs { let line = line.as_str().unwrap(); ... }`
//!   - Keep only the ones you want: `if line.contains("consumed") { ... }`
//!   - Split a string on spaces into pieces:
//!         let words: Vec<&str> = line.split_whitespace().collect();
//!     For the example line, `words` becomes:
//!         ["Program", "Tokenkeg...", "consumed", "3049", "of", "200000", "compute", "units"]
//!            index 0      index 1      index 2    index 3
//!     So words[1] = the program id, words[3] = the CU number (as a &str).
//!   - Turn the number string into a real number:
//!         let cu: u64 = words[3].parse().unwrap();
//!     (You don't strictly need to parse it just to print it — but parsing is
//!      good practice and lets you sum/sort later.)

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
