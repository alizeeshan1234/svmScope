//! Probe: the binary a program had before a slot, via the account-changes stream.
//!   SUBSTREAMS_API_KEY=… cargo run --release --example elf_before -- <program> <slot>
use svmscope::Scope;
fn main() {
    let program = std::env::args().nth(1).expect("program");
    let slot: u64 = std::env::args()
        .nth(2)
        .expect("slot")
        .parse()
        .expect("slot");
    let rpc = std::env::var("SVMSCOPE_RPC_URL").expect("SVMSCOPE_RPC_URL");
    let scope = Scope::new(rpc);
    let scope = match svmscope::history::HistoryStream::from_env() {
        Some(s) => scope.with_history_stream(s),
        None => {
            eprintln!("no stream");
            scope
        }
    };
    let started = std::time::Instant::now();
    match scope.deploy_info(&program) {
        Ok(d) => eprintln!("deploy info: {d:?}"),
        Err(e) => eprintln!("deploy info error: {e}"),
    }
    match scope.program_elf_before(&program, slot) {
        Some((at, elf)) => println!(
            "binary before {slot}: deployed at {at}, {} bytes ({:.1}s)",
            elf.len(),
            started.elapsed().as_secs_f32()
        ),
        None => println!(
            "no earlier binary found ({:.1}s)",
            started.elapsed().as_secs_f32()
        ),
    }
}
