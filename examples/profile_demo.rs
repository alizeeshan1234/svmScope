//! Profile a mainnet transaction: BPF instructions per program, per function,
//! and per syscall, from LiteSVM's register trace.
//!
//!   cargo run --features profiler --example profile_demo -- <signature> [rpc-url] [<program>=<path/to/program.debug> ...]
//!
//! Each `program=path` names that program's functions from the unstripped
//! ELF of the same build (`cargo build-sbf --debug` writes it next to the .so).
#[cfg(not(feature = "profiler"))]
fn main() {
    eprintln!(
        "build with the `profiler` feature (on by default): cargo run --example profile_demo"
    );
}

#[cfg(feature = "profiler")]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    use svmscope::Scope;
    let args: Vec<String> = std::env::args().collect();
    let sig = args
        .get(1)
        .ok_or("usage: profile_demo <signature> [rpc-url]")?;
    let rpc = args
        .get(2)
        .cloned()
        .unwrap_or_else(|| "https://api.mainnet-beta.solana.com".to_string());
    println!("profiling {sig} via {rpc}");
    let scope = Scope::new(&rpc);
    // The transaction's own slot, reconstructed: balances and clock as they
    // were, so most on-chain successes replay instead of drifting.
    let replay = scope.replay_at_slot(sig)?;
    let (result, mut profile) = replay.profile(&[])?;
    for spec in args.iter().skip(3) {
        let Some((program, path)) = spec.split_once('=') else {
            continue;
        };
        let elf = std::fs::read(path)?;
        let n = profile.symbolize(program, &elf)?;
        println!("symbolized {n} functions of {program} from {path}");
    }
    println!(
        "replay: {} · {} CU reported · {} BPF instructions traced across {} frames",
        if result.success { "ok" } else { "failed" },
        result.compute_units,
        profile.instructions(),
        profile.frames.len()
    );
    println!("\n-- by program --");
    for (p, n) in profile.by_program() {
        println!("{n:>10}  {p}");
    }
    for f in &profile.frames {
        println!(
            "\n== frame {} · {} instructions · {} CU (overhead beyond instructions: {}) ==",
            f.program,
            f.instructions,
            f.compute_units.map(|c| c.to_string()).unwrap_or("?".into()),
            f.syscall_overhead
                .map(|c| c.to_string())
                .unwrap_or("?".into())
        );
        println!("   top functions (self / total / calls):");
        for func in f.functions.iter().take(8) {
            println!(
                "   {:>9} / {:>9} / {:>5}  ~{:>6} CU  {}",
                func.self_insns,
                func.total_insns,
                func.calls,
                func.compute_units
                    .map(|c| c.to_string())
                    .unwrap_or("?".into()),
                func.name
            );
        }
        if !f.syscalls.is_empty() {
            println!("   syscalls:");
            for (name, n) in f.syscalls.iter().take(8) {
                println!("   {n:>9}  {name}");
            }
        }
        println!(
            "   deepest hot stack: {}",
            f.stacks
                .first()
                .map(|(s, n)| format!("{n} · {s}"))
                .unwrap_or_default()
        );
    }
    Ok(())
}
