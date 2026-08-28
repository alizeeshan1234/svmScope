//! Post-mortem: paste a signature, see what happened — then see *why*.
//!
//! ```sh
//! cargo run --example post_mortem -- <signature> [rpc_url]
//! ```

use svmscope::Scope;

fn main() -> svmscope::Result<()> {
    let mut args = std::env::args().skip(1);
    let sig = args
        .next()
        .expect("usage: post_mortem <signature> [rpc_url]");
    let rpc = args
        .next()
        .unwrap_or_else(|| "https://api.mainnet-beta.solana.com".into());

    let scope = Scope::new(rpc);

    // Decode: every instruction named from its on-chain IDL or native layout.
    let analysis = scope.analyze(&sig)?;
    println!(
        "on-chain: success={} fee={} CU={:?}",
        analysis.overview.success, analysis.overview.fee, analysis.overview.compute_units
    );
    for ix in &analysis.cpi_tree {
        let indent = "  ".repeat(ix.stack_height as usize);
        println!("{indent}{}", ix.name.as_deref().unwrap_or(&ix.program));
    }

    // Replay it locally — the real program binaries, reconstructed state.
    let replay = scope.replay(&analysis.signature)?;
    let out = replay.run()?;
    println!(
        "\nreplay: success={} CU={}",
        out.result.success, out.result.compute_units
    );

    // On failure, the error arrives explained, not as a bare Custom(6001).
    if let Some(explain) = &out.explain {
        println!("why: {} — {}", explain.title, explain.detail);
    }
    // And every account change arrives as named fields where a layout is known.
    for diff in &out.diffs {
        for f in &diff.fields {
            println!("  {}: {} → {}", f.name, f.before, f.after);
        }
    }
    Ok(())
}
