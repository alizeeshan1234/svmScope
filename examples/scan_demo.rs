//! Auto breaking-point scan: find every numeric threshold that flips a
//! transaction's outcome, across all its accounts' fields.
//!
//!   RPC=<url> cargo run --example scan_demo -- <signature>

use svmscope::{scan_breaking_points, ScanOptions, Scope};

fn short(a: &str) -> String {
    if a.len() > 12 {
        format!("{}…{}", &a[..4], &a[a.len() - 4..])
    } else {
        a.to_string()
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let rpc =
        std::env::var("RPC").unwrap_or_else(|_| "https://api.mainnet-beta.solana.com".to_string());
    let sig = std::env::args().nth(1).expect("usage: … <signature>");

    let scope = Scope::new(&rpc);
    let analysis = scope.analyze(&sig)?;
    let accounts: Vec<String> = analysis.accounts.iter().map(|a| a.address.clone()).collect();

    println!("scanning {} accounts for breaking points…", accounts.len());
    let bps = scan_breaking_points(&scope, &sig, &accounts, ScanOptions::default())?;

    println!("\n{} breaking point(s) found — this transaction breaks when:", bps.len());
    for bp in &bps {
        let dir = if bp.breaks_below {
            format!("drops below {}", bp.flips_at)
        } else {
            format!("reaches {} or above", bp.flips_at)
        };
        println!(
            "  · {}.{} {}   (currently {}, {} replays)",
            short(&bp.account),
            bp.field,
            dir,
            bp.current,
            bp.evaluations
        );
    }
    Ok(())
}
