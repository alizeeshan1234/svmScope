# Contributing to svmscope

Thanks for your interest! Issues and pull requests are welcome.

## Building and testing

```bash
cargo build                 # library + CLI
cargo test                  # unit, integration, and doctests (offline)
cargo fmt --check           # CI enforces formatting
cargo clippy --all-targets --features server -- -D warnings   # CI enforces zero warnings
```

The minimum supported Rust version is **1.91** (declared in `Cargo.toml` and
checked in CI).

Two integration tests exercise live submission and are `#[ignore]`d by
default. To run them, start a local validator and then:

```bash
solana-test-validator --reset &
cargo test --test send_and_capture_localnet -- --ignored --test-threads=1
```

## Ground rules for changes

- **A reverting replay is data, not an error.** `Err` is reserved for
  svmscope itself failing (RPC, malformed input, reconstruction). Don't blur
  this line.
- **No silent passes.** A typo'd address, unknown field, or malformed spec
  must be a typed error or a failed check — never something a test suite can
  vacuously pass. New check/mutation kinds must uphold this.
- **No panics on untrusted input.** RPC responses, on-chain IDLs, fixtures,
  and user JSON are all untrusted; malformed input gets a typed `Error`.
  Guard recursion depth and allocations when walking attacker-controllable
  data.
- **Every bug fix ships with a regression test.** Offline if possible —
  fixtures make most things reproducible without a validator.
- README code examples are compile-checked as doctests; if you touch them,
  `cargo test` must stay green.

## Pull requests

Keep PRs focused, describe the behavior change (not just the code change),
and make sure the full CI matrix above passes locally first. For anything
API-shaped, opening an issue to discuss the design beforehand is appreciated.

## Reporting issues

A great bug report includes the transaction signature (or a frozen fixture),
the svmscope version, and what you expected vs. what happened. For anything
security-sensitive, please report privately via GitHub security advisories
rather than a public issue.
