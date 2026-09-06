//! svmscope CLI — decode, replay, and mutate a Solana transaction.
//!
//! `--json` emits the whole analysis as JSON; `--mutate <addr>:<lamports>` applies
//! a what-if mutation before replaying.

use std::env;
use std::error::Error;

use std::io::Write;
use svmscope::{spec, Mutation, Replay, ReplayResult, ScenarioOutcome, Scope, Trace};

fn main() -> Result<(), Box<dyn Error>> {
    let args: Vec<String> = env::args().collect();

    let signature = args
        .get(1)
        .ok_or("usage: svmscope <transaction-signature> [--json] [--mutate <addr>:<lamports>]\n       svmscope freeze <transaction-signature> [-o fixture.json]\n       svmscope test <scenarios.json>\n       svmscope report <scenarios.json> [-o report.html]\n       svmscope idl <program-address>\n       svmscope upgrade <fixture.json>\n\n       any command also takes --cluster <mainnet|devnet|testnet|localnet> or --rpc <url>")?;

    // Cluster/RPC selection: --cluster <mainnet|devnet|testnet|localnet> or --rpc <url>.
    let flag = |name: &str| {
        args.iter()
            .position(|a| a == name)
            .and_then(|i| args.get(i + 1))
            .cloned()
    };
    let rpc = svmscope::resolve_rpc_url(
        flag("--cluster").as_deref(),
        flag("--rpc").as_deref(),
        "https://api.mainnet-beta.solana.com",
    )?;
    let scope = Scope::new(rpc);

    // Debugger: `svmscope debug <signature> [--json]` steps through the
    // transaction — every instruction and CPI with what it changed, and the
    // failing step pinpointed.
    if signature == "debug" {
        let sig = args
            .get(2)
            .ok_or("usage: svmscope debug <signature> [--json] [--now]")?;
        // Landed transactions are traced as they happened: balances and the
        // clock rewound to the transaction's slot (exact state when an archive
        // is configured). `--now` traces against today's state instead.
        let replay = if args.iter().any(|a| a == "--now") {
            scope.replay(sig)?
        } else {
            scope.replay_at_slot(sig)?
        };
        let trace = replay.trace(&[])?;
        if args.iter().any(|a| a == "--json") {
            println!("{}", serde_json::to_string_pretty(&trace)?);
        } else {
            print_trace(&trace);
        }
        return Ok(());
    }

    // Symbol corpus: `svmscope symbols dump <path.so> <path.debug> [-o corpus.jsonl]`
    // writes one JSON line per named function shape of a build, for
    // `svmscope profile --corpus corpus.jsonl` to name the same library
    // functions inside stripped programs.
    #[cfg(feature = "profiler")]
    if signature == "symbols" {
        let usage = "usage: svmscope symbols dump <path.so> <path.debug> [-o corpus.jsonl]\n       svmscope symbols exact <program> <path.so> <path.debug> [-o symbols/exact/<program>.json]";
        if args.get(2).map(String::as_str) == Some("exact") {
            let program = args.get(3).ok_or(usage)?;
            let so = std::fs::read(args.get(4).ok_or(usage)?)?;
            let debug = std::fs::read(args.get(5).ok_or(usage)?)?;
            let exact = svmscope::profile::exact_from_build(program, &so, &debug)?;
            let text = serde_json::to_string(&exact)?;
            match args.iter().position(|a| a == "-o") {
                Some(i) => std::fs::write(args.get(i + 1).ok_or(usage)?, &text)?,
                None => println!("{text}"),
            }
            eprintln!(
                "{} symbols, elf sha256 {}",
                exact.symbols.len(),
                exact.elf_sha256
            );
            return Ok(());
        }
        if args.get(2).map(String::as_str) != Some("dump") {
            return Err(usage.into());
        }
        let so = std::fs::read(args.get(3).ok_or(usage)?)?;
        let debug = std::fs::read(args.get(4).ok_or(usage)?)?;
        let entries = svmscope::profile::corpus_from_build(&so, &debug)?;
        let mut out: Box<dyn std::io::Write> = match args.iter().position(|a| a == "-o") {
            Some(i) => Box::new(std::fs::File::create(args.get(i + 1).ok_or(usage)?)?),
            None => Box::new(std::io::stdout()),
        };
        for e in &entries {
            writeln!(out, "{}", serde_json::to_string(e)?)?;
        }
        eprintln!("{} function shapes", entries.len());
        return Ok(());
    }

    // Profiler: `svmscope profile <signature> [--now] [--json]
    // [--symbols <program>=<path/to/program.debug>]...` traces every BPF
    // instruction the transaction executes and attributes it to functions,
    // syscalls and call stacks, per program frame.
    #[cfg(feature = "profiler")]
    if signature == "profile" {
        let sig = args.get(2).ok_or(
            "usage: svmscope profile <signature> [--now] [--json] [--symbols <program>=<path.debug>[,<path.so>]]...",
        )?;
        let replay = if args.iter().any(|a| a == "--now") {
            scope.replay(sig)?
        } else {
            scope.replay_at_slot(sig)?
        };
        let (result, mut profile) = replay.profile(&[])?;
        // `signature` is the subcommand word here; `sig` is the transaction.
        match scope.analyze(sig) {
            Ok(analysis) => {
                profile.attach_names(&analysis.cpi_tree, &result.logs);
            }
            Err(e) => eprintln!("instruction names unavailable: {e}"),
        }
        let mut i = 3;
        while i < args.len() {
            if args[i] == "--corpus" {
                let path = args.get(i + 1).ok_or("--corpus needs <corpus.jsonl>")?;
                let text = std::fs::read_to_string(path).map_err(|e| format!("{path}: {e}"))?;
                let corpus: Vec<svmscope::profile::CorpusEntry> = text
                    .lines()
                    .filter(|l| !l.trim().is_empty())
                    .map(serde_json::from_str)
                    .collect::<Result<_, _>>()?;
                let n = profile.symbolize_from_corpus(&corpus);
                eprintln!(
                    "corpus {path}: {} shapes, named {n} functions",
                    corpus.len()
                );
                i += 2;
                continue;
            }
            if args[i] == "--symbols" {
                let spec = args.get(i + 1).ok_or("--symbols needs <program>=<path>")?;
                let (program, paths) = spec
                    .split_once('=')
                    .ok_or("--symbols needs <program>=<path.debug>[,<path.so>]")?;
                let (debug_path, so_path) = match paths.split_once(',') {
                    Some((d, so)) => (d, Some(so)),
                    None => (paths, None),
                };
                let debug = std::fs::read(debug_path).map_err(|e| format!("{debug_path}: {e}"))?;
                match so_path {
                    // A .debug alone: the same build as what ran — names map by address.
                    None => {
                        let n = profile.symbolize(program, &debug)?;
                        eprintln!("symbolized {n} functions of {program} from {debug_path}");
                    }
                    // .debug plus its .so: another build of the same source — names map by code shape.
                    Some(so_path) => {
                        let so = std::fs::read(so_path).map_err(|e| format!("{so_path}: {e}"))?;
                        let r = profile.symbolize_from_build(program, &so, &debug)?;
                        eprintln!(
                            "symbolized {program} from {so_path} + {debug_path}: {} exact, {} by opcode shape, {} by similarity, {} unmatched{}",
                            r.exact,
                            r.by_opcodes,
                            r.by_similarity,
                            r.unmatched,
                            if r.same_build { " (same build, mapped by address)" } else { "" }
                        );
                    }
                }
                i += 2;
            } else {
                i += 1;
            }
        }
        if args.iter().any(|a| a == "--json") {
            println!(
                "{}",
                serde_json::to_string_pretty(
                    &serde_json::json!({ "result": result, "profile": profile })
                )?
            );
        } else {
            print_profile(&result, &profile);
        }
        return Ok(());
    }

    // Test-runner mode: `svmscope test <scenarios.json>` runs a scenario suite
    // and exits non-zero if any assertion fails — drop it straight into CI.
    if signature == "test" {
        let path = args.get(2).ok_or("usage: svmscope test <scenarios.json>")?;
        return run_tests(&scope, path);
    }

    // Report mode: run a suite and render a shareable HTML report.
    if signature == "report" {
        let path = args
            .get(2)
            .ok_or("usage: svmscope report <scenarios.json> [-o report.html]")?;
        let out = args
            .iter()
            .position(|a| a == "-o")
            .and_then(|i| args.get(i + 1));
        return run_report(&scope, path, out);
    }

    // IDL probe: `svmscope idl <program_id>` prints a program's on-chain Anchor IDL.
    if signature == "idl" {
        let prog = args.get(2).ok_or("usage: svmscope idl <program_id>")?;
        match scope.program_idl(prog)? {
            Some(j) => println!("{}", serde_json::to_string_pretty(&j)?),
            None => println!("no on-chain IDL for {prog}"),
        }
        return Ok(());
    }

    // Upgrade mode: `svmscope upgrade <fixture.json>` re-captures an old
    // fixture in the current schema (adds captured IDLs + the on-chain
    // outcome), preserving the original signature. Needs live RPC.
    if signature == "upgrade" {
        let path = args
            .get(2)
            .ok_or("usage: svmscope upgrade <fixture.json>")?;
        let text = std::fs::read_to_string(path).map_err(|e| format!("cannot read {path}: {e}"))?;
        let old = svmscope::Fixture::from_json(&text)?;
        eprintln!("re-capturing {}…", old.signature);
        let fx = scope.capture(&old.signature)?;
        std::fs::write(path, fx.to_json()?).map_err(|e| format!("write {path}: {e}"))?;
        eprintln!("wrote {path} (v{}, {})", fx.version, fx.summary());
        return Ok(());
    }

    // Freeze mode: `svmscope freeze <sig> [-o fixture.json]` captures a
    // self-contained fixture for deterministic, offline replay.
    if signature == "freeze" {
        let sig = args
            .get(2)
            .ok_or("usage: svmscope freeze <signature> [-o fixture.json]")?;
        let out = args
            .iter()
            .position(|a| a == "-o")
            .and_then(|i| args.get(i + 1));
        eprintln!("freezing {sig}…");
        let fx = scope.capture(sig)?;
        let json = fx.to_json()?;
        match out {
            Some(path) => {
                std::fs::write(path, &json).map_err(|e| format!("write {path}: {e}"))?;
                eprintln!("wrote {path} ({}, {} bytes)", fx.summary(), json.len());
            }
            None => println!("{json}"),
        }
        return Ok(());
    }

    let json_mode = args.iter().any(|a| a == "--json");

    // JSON mode: emit the whole analysis as one JSON blob and stop.
    if json_mode {
        if args.iter().any(|a| a == "--mutate") {
            eprintln!("note: --mutate is ignored in --json mode (it emits the unmutated analysis)");
        }
        let analysis = scope.analyze(signature)?;
        println!("{}", serde_json::to_string_pretty(&analysis)?);
        return Ok(());
    }

    // --- human-readable output ---
    let analysis = scope.analyze(signature)?;

    for e in &analysis.cpi_tree {
        let name = e.name.as_deref().unwrap_or(&e.program);
        if e.stack_height == 1 {
            println!("#{}  {}  ({})", e.index, name, e.program);
        } else {
            let indent = "    ".repeat((e.stack_height - 2) as usize);
            println!(
                "{}└─ [{}] {}  ({})",
                indent, e.stack_height, name, e.program
            );
        }
    }

    println!("\n-- account balance changes --");
    for c in &analysis.balance_change {
        println!("{:<44} {:+} lamports", c.address, c.delta);
    }

    println!("\n-- compute units per program --");
    for c in &analysis.compute {
        println!("{:<44} {} CU", c.program, c.cu);
    }

    println!("\n-- replay --");
    // One fetch pass; the mutated replay below reuses the same world for free.
    let replay_session = scope.replay(&analysis.signature)?;
    print_replay("REPLAY", &replay_session.run()?.result);

    // Mutations come only from the CLI (`--mutate <address>:<lamports>`).
    let mut mutations: Vec<Mutation> = Vec::new();
    let mut i = 2;
    while i < args.len() {
        if args[i] == "--mutate" {
            let spec = args
                .get(i + 1)
                .ok_or("--mutate needs <address>:<lamports>")?;
            let (addr, lamports_str) = spec
                .split_once(':')
                .ok_or("mutation must look like <address>:<lamports>")?;
            let value: u64 = lamports_str
                .parse()
                .map_err(|_| "lamports must be a number")?;
            mutations.push(Mutation::lamports(addr, value));
            i += 2;
        } else {
            i += 1;
        }
    }

    if !mutations.is_empty() {
        print_replay(
            "MUTATED REPLAY",
            &replay_session.simulate(&mutations)?.result,
        );
    }

    Ok(())
}

/// Load a suite file and run it, preferring a frozen `fixture` (deterministic,
/// offline) over a live `signature`. Returns a human label + the outcomes.
fn load_and_run_suite(
    scope: &Scope,
    path: &str,
) -> Result<(String, Vec<ScenarioOutcome>), Box<dyn Error>> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("cannot read {path}: {e}"))?;
    let req: spec::SuiteRequest =
        serde_json::from_str(&text).map_err(|e| format!("bad scenario file: {e}"))?;

    let scenarios = req
        .scenarios
        .into_iter()
        .map(|s| s.into_scenario())
        .collect::<Result<Vec<_>, _>>()?;
    let features = svmscope::spec::feature_toggles(req.features)?;

    // A suite file may carry its own `cluster`/`rpc` for the live-signature path.
    // Honor them (overriding the CLI-level scope) so a `"cluster": "devnet"`
    // suite doesn't silently query mainnet when run without a matching --cluster.
    let suite_scope = if req.signature.is_some() && (req.cluster.is_some() || req.rpc.is_some()) {
        let url = svmscope::resolve_rpc_url(
            req.cluster.as_deref(),
            req.rpc.as_deref(),
            "https://api.mainnet-beta.solana.com",
        )?;
        Some(Scope::new(url))
    } else {
        None
    };
    let scope = suite_scope.as_ref().unwrap_or(scope);

    let (label, mut replay) = if let Some(fx_ref) = &req.fixture {
        // Fixture path is resolved relative to the suite file's directory.
        let fx_path = std::path::Path::new(path)
            .parent()
            .map(|d| d.join(fx_ref))
            .unwrap_or_else(|| std::path::PathBuf::from(fx_ref));
        let fx_text = std::fs::read_to_string(&fx_path)
            .map_err(|e| format!("cannot read fixture {}: {e}", fx_path.display()))?;
        let fx = svmscope::Fixture::from_json(&fx_text)?;
        let label = format!(
            "fixture {} ({}) [deterministic, offline]",
            fx.signature,
            fx.summary()
        );
        (label, Replay::from_fixture(&fx)?)
    } else if let Some(sig) = &req.signature {
        let label = format!("{sig} [live RPC — may drift]");
        (label, scope.replay(sig)?)
    } else {
        return Err("suite must specify either \"fixture\" or \"signature\"".into());
    };

    replay.set_time_travel(req.time_travel);
    replay.set_features(features);
    Ok((label, replay.run_suite(&scenarios)?))
}

/// Run a scenario suite and render a shareable, self-contained HTML report.
fn run_report(scope: &Scope, path: &str, out: Option<&String>) -> Result<(), Box<dyn Error>> {
    let (label, outcomes) = load_and_run_suite(scope, path)?;
    let html = svmscope::report::render_html(&label, &outcomes);
    match out {
        Some(p) => {
            std::fs::write(p, &html).map_err(|e| format!("write {p}: {e}"))?;
            let passed = outcomes.iter().filter(|o| o.pass).count();
            eprintln!(
                "wrote {p} — {passed}/{} scenarios ({} bytes)",
                outcomes.len(),
                html.len()
            );
        }
        None => println!("{html}"),
    }
    Ok(())
}

/// Run a scenario suite from a JSON file and print a test-runner report.
/// Exits the process with code 1 if any scenario fails its assertion.
fn run_tests(scope: &Scope, path: &str) -> Result<(), Box<dyn Error>> {
    let (label, outcomes) = load_and_run_suite(scope, path)?;
    println!("svmscope test — {label}\n");

    let mut passed = 0;
    for o in &outcomes {
        let got = if o.actual.success {
            "succeeded".to_string()
        } else {
            format!(
                "reverted ({})",
                o.actual.error.as_deref().unwrap_or("error")
            )
        };
        let mark = if o.pass {
            passed += 1;
            "PASS"
        } else {
            "FAIL"
        };
        println!("  {mark}  {}  (expect: {}; got: {})", o.name, o.expect, got);
        for a in &o.asserts {
            let am = if a.pass { "✓" } else { "✗" };
            println!("          {am} assert {}", a.description);
        }
    }

    let total = outcomes.len();
    println!("\n{passed}/{total} passed");
    if passed != total {
        std::process::exit(1);
    }
    Ok(())
}

/// Display a compute profile: per program, then per frame with its top
/// functions and syscalls.
#[cfg(feature = "profiler")]
fn print_profile(result: &ReplayResult, profile: &svmscope::profile::Profile) {
    println!(
        "replay: {} · {} CU charged · {} BPF instructions across {} program frames",
        if result.success {
            "ok ✅"
        } else {
            "failed ❌"
        },
        result.compute_units,
        profile.instructions(),
        profile.frames.len()
    );
    println!("\n-- compute per program --");
    for (program, n) in profile.by_program() {
        println!("{n:>10}  {program}");
    }
    for (i, f) in profile.frames.iter().enumerate() {
        let cu = f
            .compute_units
            .map(|c| format!("{c} CU"))
            .unwrap_or_else(|| "CU unknown".into());
        let overhead = f
            .syscall_overhead
            .map(|c| format!(" · {c} CU beyond instructions"))
            .unwrap_or_default();
        println!(
            "\n== frame {} · {} · {} instructions · {cu}{overhead} ==",
            i + 1,
            f.program,
            f.instructions
        );
        println!(
            "   {:>9} {:>9} {:>6} {:>8}  function",
            "self", "total", "calls", "~CU"
        );
        for func in f.functions.iter().take(12) {
            // A real name wins; an anonymous function shows what it did, with
            // its id beside it.
            let anon = func.name.starts_with("function_");
            let shown = match (&func.label, anon) {
                (Some(label), true) => {
                    format!("{label}  ({})", func.name.replace("function_", "fn@"))
                }
                (_, true) => func.name.replace("function_", "fn@"),
                _ => func.name.clone(),
            };
            println!(
                "   {:>9} {:>9} {:>6} {:>8}  {}",
                func.self_insns,
                func.total_insns,
                func.calls,
                func.compute_units
                    .map(|c| c.to_string())
                    .unwrap_or_else(|| "-".into()),
                shown
            );
        }
        if !f.syscalls.is_empty() {
            let est = f.syscall_estimate();
            let floor: u64 = est.iter().map(|(_, _, cu)| cu).sum();
            println!("   syscalls (calls · at least CU by the runtime's fixed charge):");
            for (name, n, cu) in est.iter().take(10) {
                println!("   {n:>9} · ≥{cu:>7}  {name}");
            }
            if let Some(over) = f.syscall_overhead {
                println!(
                    "   fixed charges explain ≥{floor} of the {over} CU beyond instructions; the rest scales with bytes copied, hashed or passed to CPIs"
                );
            }
        }
    }
    let anon = profile
        .frames
        .iter()
        .flat_map(|f| f.functions.iter())
        .filter(|f| f.name.starts_with("function_"))
        .count();
    if anon > 0 {
        println!(
            "\n{anon} functions are unnamed: those programs are stripped (every mainnet program is). \
             Built one yourself? Pass --symbols <program>=<path/to/program.debug> (the file \
             `cargo build-sbf --debug` writes next to the .so) and every function gets its Rust name."
        );
    }
}

/// Display a replay result (the CLI's job — the module returns data, `main` prints it).
fn print_replay(label: &str, r: &ReplayResult) {
    if r.success {
        println!("{label}: success ✅  (compute units: {})", r.compute_units);
    } else {
        println!(
            "{label}: failed ❌  error: {}",
            r.error.as_deref().unwrap_or("unknown")
        );
    }
    for log in &r.logs {
        println!("  {log}");
    }
}

/// Render a trace as an indented tree for the terminal.
fn print_trace(trace: &Trace) {
    let short = |s: &str| {
        if s.len() > 12 {
            format!("{}…{}", &s[..6], &s[s.len() - 4..])
        } else {
            s.to_string()
        }
    };
    println!(
        "{}  {}  fidelity: {}",
        if trace.result.success { "OK " } else { "FAIL" },
        if trace.signature.is_empty() {
            "(preflight)"
        } else {
            &trace.signature
        },
        trace.fidelity
    );
    if let Some(c) = &trace.clock {
        println!("clock: {c}");
    }
    for (i, s) in trace.steps.iter().enumerate() {
        let indent = "  ".repeat((s.depth as usize).saturating_sub(1));
        let mark = if trace.failed_step == Some(i) {
            "✗"
        } else if s.prefix_artifact {
            "~"
        } else if s.success {
            "✓"
        } else {
            "·"
        };
        let name = s.name.clone().unwrap_or_else(|| "?".into());
        let cu = s.cu_consumed.map(|c| format!("{c} CU")).unwrap_or_default();
        println!(
            "{indent}{mark} [{}] {} · {}  {}",
            s.path,
            short(&s.program),
            name,
            cu
        );
        for a in &s.args {
            println!("{indent}      {} = {}", a.name, a.value);
        }
        for d in &s.diffs {
            let lam = if d.lamports_before != d.lamports_after {
                format!("  lamports {} → {}", d.lamports_before, d.lamports_after)
            } else {
                String::new()
            };
            println!("{indent}    Δ {}{}", short(&d.address), lam);
            for f in &d.fields {
                println!("{indent}        {} : {} → {}", f.name, f.before, f.after);
            }
            if d.fields.is_empty() && d.raw_data_changed {
                println!("{indent}        (data changed, layout unknown)");
            }
        }
        if let Some(e) = &s.error {
            match &e.explain {
                Some(x) => println!("{indent}    ERROR {} — {}", x.title, x.detail),
                None => println!("{indent}    ERROR {}", e.raw),
            }
        }
    }
    if let Some(i) = trace.failed_step {
        let s = &trace.steps[i];
        println!("\nfailed at step {} of {}", s.path, trace.steps.len());
        let n = trace.result.logs.len();
        let (a, b) = s.logs;
        let lines = &trace.result.logs[a.min(n)..b.min(n)];
        if !lines.is_empty() {
            println!("logs of the failing step:");
            for l in lines.iter().take(20) {
                println!("  {l}");
            }
            if lines.len() > 20 {
                println!("  … {} more", lines.len() - 20);
            }
        }
    }
    if trace.drifted() {
        let onchain = if trace.onchain_success == Some(true) {
            "succeeded"
        } else {
            "failed"
        };
        println!(
            "\nnote: on-chain this transaction {onchain}; the replay ran against `{}` state, so \
             the difference is state drift since the transaction's slot, not necessarily a \
             program bug.",
            trace.fidelity
        );
    }
}
