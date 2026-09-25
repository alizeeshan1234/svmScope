//! Lift one landed transaction: SVMSCOPE_RPC_URL=… cargo run --release --example lift_demo -- <signature>
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let sig = std::env::args().nth(1).expect("signature");
    let scope = svmscope::Scope::new(std::env::var("SVMSCOPE_RPC_URL")?);
    let started = std::time::Instant::now();
    let router = std::env::args().nth(2);
    let r = scope.lift(&sig, router.as_deref())?;
    println!(
        "verdict: {} ({:.1}s)",
        r.verdict,
        started.elapsed().as_secs_f32()
    );
    println!(
        "routers: {:?}",
        r.routers
            .iter()
            .map(|x| (x.index, &x.program[..6], x.inner_calls))
            .collect::<Vec<_>>()
    );
    println!(
        "original: success={} cu={} ixs={} bytes={} tokens={}",
        r.original.success,
        r.original.compute_units,
        r.original.instructions,
        r.original.size_bytes,
        r.original.token_deltas.len()
    );
    println!(
        "lifted:   success={} cu={} ixs={} bytes={} tokens={} err={:?}",
        r.lifted.success,
        r.lifted.compute_units,
        r.lifted.instructions,
        r.lifted.size_bytes,
        r.lifted.token_deltas.len(),
        r.lifted.error
    );
    println!(
        "equivalent={} compute_saved={:?} fits={} blocked={:?}",
        r.equivalent,
        r.compute_saved,
        r.fits_packet,
        r.blocked
            .iter()
            .map(|b| (
                b.lifted_index,
                &b.program[..6],
                &b.reason[..40.min(b.reason.len())]
            ))
            .collect::<Vec<_>>()
    );
    Ok(())
}
