//! Why did my transaction fail — and how do I fix it?
//!   RPC=<url> cargo run --example diagnose_demo -- <signature>
use svmscope::Scope;
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let rpc = std::env::var("RPC").unwrap_or_else(|_| "https://api.mainnet-beta.solana.com".into());
    let sig = std::env::args().nth(1).expect("usage: … <signature>");
    let d = Scope::new(&rpc).diagnose(&sig)?;
    println!("failed:   {}", d.failed);
    println!("headline: {}", d.headline);
    println!("what:     {}", d.explanation.replace("**", ""));
    if let Some(f) = &d.fix {
        println!("fix:      {f}");
    }
    if let Some(p) = &d.program {
        println!("program:  {p}");
    }
    Ok(())
}
