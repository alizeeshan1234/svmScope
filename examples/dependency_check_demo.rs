//! Read the devnet registry and run one dependency check.
//!
//!   cargo run --release --example dependency_check_demo -- [registry_program]
use svmscope::{dependency_watch::DEVNET_REGISTRY, Scope};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let registry = std::env::args()
        .nth(1)
        .unwrap_or_else(|| DEVNET_REGISTRY.to_string());
    let scope = Scope::new("https://api.devnet.solana.com");
    let reg = scope.registry(&registry)?;
    println!(
        "registry {}: {} protocols, {} dependencies",
        registry,
        reg.protocols.len(),
        reg.dependencies.len()
    );
    for p in &reg.protocols {
        println!(
            "  protocol {} watches {} -> alerts to {} (corpus {})",
            p.address, p.program_id, p.alert_url, p.corpus_size
        );
        for d in reg.dependencies.iter().filter(|d| d.protocol == p.address) {
            let deploy = scope.deploy_info(&d.program_id)?;
            println!(
                "    depends on {} alerts={} deploy={:?}",
                d.program_id,
                d.alerts_enabled,
                deploy.map(|x| x.last_deploy_slot)
            );
        }
    }
    if let (Some(p), Some(d)) = (reg.protocols.first(), reg.dependencies.first()) {
        println!(
            "\nchecking {} against {} (3 txs)…",
            p.program_id, d.program_id
        );
        let started = std::time::Instant::now();
        let report =
            scope.dependency_check(&p.program_id, &d.program_id, 3, svmscope::Baseline::Current)?;
        println!(
            "verdict: {} ({:.1}s)",
            report.verdict(),
            started.elapsed().as_secs_f32()
        );
        println!("{}", serde_json::to_string_pretty(&report.summary)?);
        for t in &report.transactions {
            println!(
                "  {} slot {:?} chain={} before={} after={} delta={} {}",
                &t.signature[..12],
                t.slot,
                t.onchain_success,
                t.before.success,
                t.after.success,
                t.compute_delta,
                t.error.clone().unwrap_or_default()
            );
        }
    }
    Ok(())
}
