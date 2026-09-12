//! Record versions of a few accounts for a while, so a later `replay_at_slot`
//! inside that window is exact from the recording.
//!
//!   RPC=<url> RECORD_DIR=<dir> cargo run --example record_demo -- <seconds> <address>...

use svmscope::records::{poll_once, LogStore, StateStore};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let rpc =
        std::env::var("RPC").unwrap_or_else(|_| "https://api.mainnet-beta.solana.com".to_string());
    let dir = std::env::var("RECORD_DIR").expect("RECORD_DIR");
    let mut args = std::env::args().skip(1);
    let seconds: u64 = args.next().and_then(|s| s.parse().ok()).unwrap_or(60);
    let addresses: Vec<String> = args.collect();
    let store = LogStore::open(dir)?;
    for a in &addresses {
        store.watch(a)?;
    }
    let client = solana_client::rpc_client::RpcClient::new(rpc);
    let start = std::time::Instant::now();
    let mut total = 0;
    while start.elapsed().as_secs() < seconds {
        let n = poll_once(&client, &store, &store.watched()?)?;
        total += n;
        if n > 0 {
            println!(
                "{:>5.1}s recorded {n} version(s)",
                start.elapsed().as_secs_f64()
            );
        }
        std::thread::sleep(std::time::Duration::from_millis(1500));
    }
    println!("done: {total} versions over {seconds}s");
    Ok(())
}
