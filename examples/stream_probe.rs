//! Probe one stream call: the version of an account in force before a slot.
//!   SUBSTREAMS_API_KEY=… cargo run --release --example stream_probe -- <account> <slot>
fn main() {
    let account = std::env::args().nth(1).expect("account");
    let slot: u64 = std::env::args()
        .nth(2)
        .expect("slot")
        .parse()
        .expect("slot");
    let stream = svmscope::history::HistoryStream::from_env().expect("SUBSTREAMS_API_KEY");
    let started = std::time::Instant::now();
    match stream.latest_before_windows(std::slice::from_ref(&account), slot, 1, 1) {
        Ok(found) => match found.get(&account) {
            Some(v) => println!(
                "found: slot {} data {} bytes ({:.1}s)",
                v.slot,
                v.state.as_ref().map(|s| s.data.len()).unwrap_or(0),
                started.elapsed().as_secs_f32()
            ),
            None => println!(
                "stream answered, no version for the account in that window ({:.1}s)",
                started.elapsed().as_secs_f32()
            ),
        },
        Err(e) => println!(
            "stream error: {e} ({:.1}s)",
            started.elapsed().as_secs_f32()
        ),
    }
}
