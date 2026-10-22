//! `vexor`: process manager CLI. Lifecycle commands and the `daemon`/`shim`
//! subcommands land in Phase 2 (see docs/plan.md).

use clap::Parser;

#[derive(Debug, Parser)]
#[command(
    name = "vexor",
    version,
    about = "Rust-native process manager, PM2-compatible"
)]
struct Cli {}

fn main() {
    let _cli = Cli::parse();
}
