//! Drive the durable records queue by hand: push a store's versions to the
//! GitHub releases queue, restore a store from it, pop expired days, list
//! what the queue holds, or compare two stores version by version.
//!
//!   SVMSCOPE_RECORD_GITHUB=owner/repo SVMSCOPE_GITHUB_TOKEN=... RECORD_DIR=<dir> \
//!     cargo run --example records_queue -- push [since-slot]
//!     cargo run --example records_queue -- restore
//!     cargo run --example records_queue -- pop
//!     cargo run --example records_queue -- status
//!     cargo run --example records_queue -- diff <other-dir>

use svmscope::records::{github::GithubQueue, LogStore, StateStore};

fn queue() -> Result<GithubQueue, Box<dyn std::error::Error>> {
    let repo = std::env::var("SVMSCOPE_RECORD_GITHUB")?;
    let (owner, name) = repo
        .trim()
        .split_once('/')
        .ok_or("SVMSCOPE_RECORD_GITHUB must be owner/repo")?;
    let token = std::env::var("SVMSCOPE_GITHUB_TOKEN")?;
    Ok(GithubQueue::new(owner, name, token.trim())?)
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let cmd = args.next().unwrap_or_default();
    let dir = std::env::var("RECORD_DIR").unwrap_or_else(|_| "target/records".to_string());
    let store = LogStore::open(&dir)?;
    match cmd.as_str() {
        "push" => {
            let since: u64 = args.next().and_then(|s| s.parse().ok()).unwrap_or(0);
            let n = queue()?.push_hour(&store, since)?;
            println!("pushed {n} bytes (versions after slot {since})");
        }
        "restore" => {
            let n = queue()?.restore(&store)?;
            println!(
                "restored {n} versions into {dir}: {} watched, coverage from {:?}, {:.2} MB",
                store.watched()?.len(),
                store.covered_from()?,
                store.size_bytes() as f64 / 1e6
            );
        }
        "status" => {
            println!("cutoff tag: {}", GithubQueue::cutoff_tag());
            for (tag, id, assets) in queue()?.releases()? {
                let names: Vec<String> = assets.iter().map(|(n, _)| n.clone()).collect();
                println!("{tag} (id {id}): {}", names.join(", "));
            }
        }
        "pop" => {
            let n = queue()?.pop_old()?;
            println!("popped {n} expired day(s)");
        }
        "diff" => {
            let other = LogStore::open(args.next().ok_or("diff needs <other-dir>")?)?;
            let (mut same, mut differ, mut missing) = (0, 0, 0);
            for address in store.watched()? {
                let a = store.latest_at_or_before(&address, u64::MAX)?;
                let b = other.latest_at_or_before(&address, u64::MAX)?;
                match (a, b) {
                    (Some(a), Some(b)) => {
                        let eq = a.slot == b.slot
                            && a.state.as_ref().map(|s| (&s.data, s.lamports, &s.owner))
                                == b.state.as_ref().map(|s| (&s.data, s.lamports, &s.owner));
                        if eq {
                            same += 1;
                        } else {
                            differ += 1;
                            println!("differs: {address} ({} vs {})", a.slot, b.slot);
                        }
                    }
                    (Some(_), None) => {
                        missing += 1;
                        println!("missing in other: {address}");
                    }
                    _ => {}
                }
            }
            println!(
                "{same} newest versions identical, {differ} differ, {missing} missing; coverage {:?} vs {:?}",
                store.covered_from()?,
                other.covered_from()?
            );
        }
        _ => eprintln!(
            "usage: records_queue push [since] | restore | pop | status | diff <other-dir>"
        ),
    }
    Ok(())
}
