//! Inspect a record store: `RECORD_DIR=<dir> cargo run --example records_inspect -- <address> [slot]`
use svmscope::records::{LogStore, StateStore};
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let dir = std::env::var("RECORD_DIR")?;
    let mut args = std::env::args().skip(1);
    let address = args.next().expect("address");
    let slot: u64 = args.next().and_then(|s| s.parse().ok()).unwrap_or(u64::MAX);
    let store = LogStore::open(&dir)?;
    println!("covers({slot}) = {:?}", store.covers(slot));
    match store.latest_at_or_before(&address, slot) {
        Ok(Some(v)) => println!(
            "version slot={} present={} len={}",
            v.slot,
            v.state.is_some(),
            v.state.as_ref().map(|s| s.data.len()).unwrap_or(0)
        ),
        Ok(None) => println!("no version"),
        Err(e) => println!("ERROR: {e}"),
    }
    Ok(())
}
