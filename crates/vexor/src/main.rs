//! `vexor`: process manager CLI. Lifecycle commands and the `daemon`
//! subcommand land in later Phase 2 steps (see docs/plan.md).

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(
    name = "vexor",
    version,
    about = "Rust-native process manager, PM2-compatible"
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Supervise one app in the foreground from a resolved spec file.
    /// Internal: normally started by the daemon.
    #[command(hide = true)]
    Shim {
        /// Path to the app spec (JSON).
        #[arg(long)]
        spec: PathBuf,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match cli.command {
        Some(Command::Shim { spec }) => exit_code(vexor_shim::run_from_file(&spec)),
        None => ExitCode::SUCCESS,
    }
}

fn exit_code(code: i32) -> ExitCode {
    ExitCode::from(u8::try_from(code).unwrap_or(1))
}
