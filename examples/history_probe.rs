//! Ask the account-changes stream for accounts' last change before a slot,
//! straight through the native client, and time it.
//!
//!   SUBSTREAMS_API_KEY=... cargo run --release --example history_probe -- <slot> <account>... [--first N] [--max N]
//!
//! Prints, per account, the slot of the version found and its size, or
//! "not found within N slots".

use svmscope::history::HistoryStream;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let flag = |name: &str, default: u64| {
        args.iter()
            .position(|a| a == name)
            .and_then(|i| args.get(i + 1))
            .and_then(|v| v.parse().ok())
            .unwrap_or(default)
    };
    let positional: Vec<&String> = {
        let mut out = Vec::new();
        let mut skip = false;
        for a in &args {
            if skip {
                skip = false;
                continue;
            }
            if a.starts_with("--") {
                skip = true;
                continue;
            }
            out.push(a);
        }
        out
    };
    let Some(slot) = positional.first().and_then(|s| s.parse::<u64>().ok()) else {
        eprintln!("usage: history_probe <slot> <account>... [--first N] [--max N]");
        std::process::exit(2);
    };
    let accounts: Vec<String> = positional[1..].iter().map(|s| s.to_string()).collect();
    let stream = HistoryStream::from_env().ok_or("SUBSTREAMS_API_KEY not set")?;
    let first = flag("--first", stream.lookback);
    let max = flag("--max", stream.deep_lookback);
    let started = std::time::Instant::now();
    let found = stream.latest_before_windows(&accounts, slot, first, max)?;
    for a in &accounts {
        match found.get(a) {
            Some(v) => println!(
                "{a}: slot {} ({} bytes, owner {})",
                v.slot,
                v.state.as_ref().map_or(0, |s| s.data.len()),
                v.state.as_ref().map_or("deleted", |s| s.owner.as_str())
            ),
            None => println!("{a}: not found within {max} slots"),
        }
    }
    println!("{:.1} s", started.elapsed().as_secs_f64());
    Ok(())
}
