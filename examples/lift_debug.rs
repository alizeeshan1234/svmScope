//! Debug a lift: SVMSCOPE_RPC_URL=… cargo run --release --example lift_debug -- <signature>
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let sig = std::env::args().nth(1).expect("signature");
    let scope = svmscope::Scope::new(std::env::var("SVMSCOPE_RPC_URL")?);
    let replay = scope.replay(&sig)?;
    let run = replay.run()?;
    println!(
        "original success={} cu={}",
        run.result.success, run.result.compute_units
    );
    for d in &run.diffs {
        println!(
            "  diff {} owner={} lamports {}->{} fields={:?}",
            &d.address[..8],
            &d.owner[..8],
            d.lamports_before,
            d.lamports_after,
            d.fields
                .iter()
                .map(|f| (f.name.as_str(), f.before.as_str(), f.after.as_str()))
                .collect::<Vec<_>>()
        );
    }
    let exact = std::env::var("LIFT_EXACT").is_ok();
    let r = scope.lift_with(&sig, None, exact)?;
    println!("verdict: {}", r.verdict);
    {
        let mut o: Vec<(&String, &i128)> = r.original.token_deltas.iter().collect();
        o.sort();
        let mut l: Vec<(&String, &i128)> = r.lifted.token_deltas.iter().collect();
        l.sort();
        println!(
            "  maps equal: {} | original {:?} | lifted {:?}",
            r.original.token_deltas == r.lifted.token_deltas,
            o.iter().map(|(k, v)| (&k[..6], **v)).collect::<Vec<_>>(),
            l.iter().map(|(k, v)| (&k[..6], **v)).collect::<Vec<_>>()
        );
    }
    println!("equivalent={} original.success={} lifted.success={} deltas original={} lifted={} compute {} vs {}", r.equivalent, r.original.success, r.lifted.success, r.original.token_deltas.len(), r.lifted.token_deltas.len(), r.original.compute_units, r.lifted.compute_units);
    for (i, ix) in r.lifted_instructions.iter().enumerate() {
        println!(
            "  lifted[{i}] {} from={:?} data={} accounts={:?}",
            &ix.program[..8],
            ix.from_router.as_ref().map(|s| &s[..6]),
            ix.data_len,
            ix.accounts
                .iter()
                .map(|(a, s, w)| format!(
                    "{}{}{}",
                    &a[..6],
                    if *s { "*" } else { "" },
                    if *w { "w" } else { "" }
                ))
                .collect::<Vec<_>>()
        );
    }
    for b in &r.blocked {
        println!(
            "  blocked[{}] {} out of {}: {}",
            b.lifted_index,
            &b.program[..8],
            b.router,
            b.reason
        );
    }
    for l in &r.lifted_logs {
        println!("    log: {}", &l[..l.len().min(160)]);
    }
    Ok(())
}
