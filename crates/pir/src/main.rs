//! `pir` binary entry point (skeleton, T01).
//!
//! The real CLI (clap-based, mirroring `packages/coding-agent/src/cli/`) lands
//! with the headless/interactive mode tasks (T10/T12).

fn main() {
    println!(
        "pir {} (workspace skeleton; see docs/plan/v0.1 for the implementation plan)",
        env!("CARGO_PKG_VERSION")
    );
}
