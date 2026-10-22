//! `vexor`: process manager CLI.

mod client;
mod spec;

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Args, Parser, Subcommand};
use vexor_proto::{AppStatus, Desired, ErrorCode, Request, Response, Target};

#[derive(Debug, Parser)]
#[command(
    name = "vexor",
    version,
    about = "Rust-native process manager, PM2-compatible"
)]
struct Cli {
    /// Vexor home directory [default: $VEXOR_HOME, else ~/.vexor]
    #[arg(long, global = true)]
    home: Option<PathBuf>,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Start a script, or start existing apps by name (or `all`).
    Start(StartArgs),
    /// Stop apps (by name, or `all`). They stay stopped across daemon restarts.
    Stop { target: String },
    /// Restart apps (by name, or `all`).
    Restart { target: String },
    /// Stop apps and remove them (by name, or `all`).
    #[command(visible_alias = "del")]
    Delete { target: String },
    /// List apps and their instances.
    #[command(visible_aliases = ["ls", "status"])]
    List {
        /// Machine-readable output.
        #[arg(long)]
        json: bool,
    },
    /// Run the supervisor daemon in the foreground. Other commands start it
    /// in the background automatically.
    Daemon,
    /// Supervise one app in the foreground from a resolved spec file.
    /// Internal: normally started by the daemon.
    #[command(hide = true)]
    Shim {
        /// Path to the app spec (JSON).
        #[arg(long)]
        spec: PathBuf,
        /// Run daemon-managed under --home: take the per-app lock and serve
        /// the control socket.
        #[arg(long)]
        managed: bool,
    },
}

#[derive(Debug, Args)]
pub struct StartArgs {
    /// Script or executable to start, or the name of an existing app, or `all`.
    pub target: Option<String>,
    /// App name [default: script file name without extension]
    #[arg(long)]
    pub name: Option<String>,
    /// Number of instances.
    #[arg(short = 'i', long)]
    pub instances: Option<u32>,
    /// Interpreter to run the script with (name on PATH or a path).
    /// Default: `node` for .js/.cjs/.mjs, none otherwise.
    #[arg(long)]
    pub interpreter: Option<String>,
    /// Working directory [default: current directory]
    #[arg(long)]
    pub cwd: Option<PathBuf>,
    /// Set an environment variable (repeatable).
    #[arg(long = "env", value_name = "KEY=VALUE")]
    pub env: Vec<String>,
    /// Pass this shell's whole environment to the app (PM2 behaviour).
    /// By default only a small allowlist is passed (see docs/security.md T2).
    #[arg(long)]
    pub inherit_env: bool,
    /// Start from a complete app spec file (JSON) instead of a script.
    #[arg(long, conflicts_with_all = ["target", "name", "instances", "interpreter", "cwd", "env", "inherit_env"])]
    pub spec: Option<PathBuf>,
    /// Arguments for the app, after `--`.
    #[arg(last = true)]
    pub args: Vec<String>,
}

/// Exit codes (docs/compatibility.md §4).
mod code {
    pub const FAILED: u8 = 1;
    pub const DAEMON_UNREACHABLE: u8 = 3;
    pub const PARTIAL: u8 = 4;
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let home = cli.home.unwrap_or_else(default_home);
    let Some(command) = cli.command else {
        return ExitCode::SUCCESS;
    };
    match command {
        Command::Daemon => exit_code(vexor_daemon::run(&home)),
        Command::Shim { spec, managed } => exit_code(vexor_shim::run_from_file(
            &spec,
            managed.then_some(home.as_path()),
        )),
        Command::Start(args) => match spec::start_request(&args, &home) {
            Ok(req) => send(&home, req, false),
            Err(msg) => fail(&msg),
        },
        Command::Stop { target } => send(
            &home,
            Request::Stop {
                target: target_of(target),
            },
            false,
        ),
        Command::Restart { target } => send(
            &home,
            Request::Restart {
                target: target_of(target),
            },
            false,
        ),
        Command::Delete { target } => send(
            &home,
            Request::Delete {
                target: target_of(target),
            },
            false,
        ),
        Command::List { json } => send(&home, Request::List, json),
    }
}

fn default_home() -> PathBuf {
    if let Some(h) = std::env::var_os("VEXOR_HOME") {
        return PathBuf::from(h);
    }
    std::env::var_os("HOME").map_or_else(
        || PathBuf::from(".vexor"),
        |h| PathBuf::from(h).join(".vexor"),
    )
}

pub fn target_of(t: String) -> Target {
    if t == "all" {
        Target::All
    } else {
        Target::Name(t)
    }
}

fn send(home: &std::path::Path, req: Request, json: bool) -> ExitCode {
    let response = match client::request(home, &req) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("vexor: {e}");
            return ExitCode::from(code::DAEMON_UNREACHABLE);
        }
    };
    match response {
        Response::Ok { apps } => {
            if json {
                match serde_json::to_string_pretty(&apps) {
                    Ok(s) => println!("{s}"),
                    Err(e) => return fail(&e.to_string()),
                }
            } else {
                print_table(&apps);
            }
            ExitCode::SUCCESS
        }
        Response::Error { code, message } => {
            eprintln!("vexor: {message}");
            if code == ErrorCode::NotFound
                && let Request::StartExisting { .. } = req
            {
                eprintln!("vexor: (no file by that name either)");
            }
            ExitCode::from(if code == ErrorCode::Partial {
                code::PARTIAL
            } else {
                code::FAILED
            })
        }
    }
}

fn print_table(apps: &[AppStatus]) {
    let mut rows = vec![[
        "NAME".to_owned(),
        "INST".to_owned(),
        "STATUS".to_owned(),
        "PID".to_owned(),
        "RESTARTS".to_owned(),
    ]];
    for app in apps {
        if app.snapshot.is_empty() {
            let status = match app.desired {
                Desired::Stopped => "stopped",
                Desired::Running => "launching",
            };
            rows.push([
                app.name.clone(),
                "-".into(),
                status.into(),
                "-".into(),
                "-".into(),
            ]);
        }
        for s in &app.snapshot {
            // A shim that has gone means its instances are not running,
            // whatever their last reported state.
            let state = if app.shim_pid.is_none() && app.desired == Desired::Stopped {
                "stopped"
            } else {
                s["state"].as_str().unwrap_or("?")
            };
            rows.push([
                app.name.clone(),
                s["instance"].to_string(),
                state.to_owned(),
                s["pid"]
                    .as_u64()
                    .map_or_else(|| "-".to_owned(), |p| p.to_string()),
                s["restarts"].to_string(),
            ]);
        }
    }
    let widths: Vec<usize> = (0..5)
        .map(|c| rows.iter().map(|r| r[c].len()).max().unwrap_or(0))
        .collect();
    for r in rows {
        let line: Vec<String> = r
            .iter()
            .zip(&widths)
            .map(|(cell, w)| format!("{cell:<w$}"))
            .collect();
        println!("{}", line.join("  ").trim_end());
    }
}

fn fail(msg: &str) -> ExitCode {
    eprintln!("vexor: {msg}");
    ExitCode::from(code::FAILED)
}

fn exit_code(code: i32) -> ExitCode {
    ExitCode::from(u8::try_from(code).unwrap_or(1))
}
