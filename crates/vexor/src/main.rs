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
    /// Start a script or an ecosystem file, or start existing apps by name
    /// (or `all`).
    Start(StartArgs),
    /// Check an ecosystem file without starting anything: shows each app's
    /// translation and every unsupported or ignored setting.
    Check {
        /// Ecosystem file (.js, .cjs, .mjs, .json).
        file: PathBuf,
        /// Only these apps (comma-separated).
        #[arg(long)]
        only: Option<String>,
        /// Apply `env_<NAME>` from the file.
        #[arg(long = "env", value_name = "NAME")]
        env: Option<String>,
        /// Node.js used to evaluate a JavaScript ecosystem file.
        #[arg(long)]
        interpreter: Option<String>,
        /// Machine-readable output.
        #[arg(long)]
        json: bool,
    },
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
    /// Show stored lifecycle history (newest last).
    Events {
        /// Only this app.
        app: Option<String>,
        /// Only events newer than this, e.g. 30s, 10m, 2h, 7d.
        #[arg(long, value_parser = parse_duration)]
        since: Option<std::time::Duration>,
        /// At most this many events.
        #[arg(long, default_value_t = 50)]
        limit: u32,
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
    /// Script, ecosystem file, name of an existing app, or `all`.
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
    /// Set an environment variable (repeatable). With an ecosystem file:
    /// the environment name whose `env_<NAME>` to apply, as in PM2.
    #[arg(long = "env", value_name = "KEY=VALUE|NAME")]
    pub env: Vec<String>,
    /// Ecosystem file: only start these apps (comma-separated).
    #[arg(long)]
    pub only: Option<String>,
    /// Ecosystem file: start even if some settings are unsupported in a way
    /// that changes how the app behaves (they are listed first).
    #[arg(long)]
    pub allow_unsupported: bool,
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
        Command::Start(args) => match spec::start_plan(&args, &home) {
            Ok(plan) => run_plan(&home, plan, args.allow_unsupported),
            Err(msg) => fail(&msg),
        },
        Command::Check {
            file,
            only,
            env,
            interpreter,
            json,
        } => check(&home, file, only, env, interpreter, json),
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
        Command::Events {
            app,
            since,
            limit,
            json,
        } => {
            let since_ms = since.map(|d| {
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default();
                u64::try_from(now.saturating_sub(d).as_millis()).unwrap_or(0)
            });
            send(
                &home,
                Request::Events {
                    app,
                    since_ms,
                    limit,
                },
                json,
            )
        }
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
        Response::Events { events, warnings } => {
            print_warnings(&warnings);
            if json {
                match serde_json::to_string_pretty(&events) {
                    Ok(s) => println!("{s}"),
                    Err(e) => return fail(&e.to_string()),
                }
            } else {
                print_events(&events);
            }
            ExitCode::SUCCESS
        }
        Response::Ok { apps, warnings } => {
            print_warnings(&warnings);
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

/// Print an ecosystem file's findings to stderr.
fn print_notes(notes: &[vexor_pm2::Note]) {
    use vexor_pm2::Severity;
    for n in notes {
        let level = match n.severity {
            Severity::Fatal => "error",
            Severity::Partial => "warning",
            Severity::Ignored => "note",
        };
        eprintln!("vexor: {level}: {}: {}: {}", n.app, n.field, n.message);
    }
}

fn run_plan(home: &std::path::Path, plan: spec::Plan, allow_unsupported: bool) -> ExitCode {
    print_notes(&plan.notes);
    if plan.has_fatal() && !allow_unsupported {
        eprintln!(
            "vexor: nothing was started: the errors above would change how the apps behave. \
             Fix them, or pass --allow-unsupported to start anyway."
        );
        return ExitCode::from(code::FAILED);
    }
    if plan.requests.len() == 1 {
        let req = plan.requests.into_iter().next().expect("one request");
        return send(home, req, false);
    }
    let total = plan.requests.len();
    let mut apps = Vec::new();
    let mut failed = 0;
    for req in plan.requests {
        match client::request(home, &req) {
            Ok(Response::Ok { apps: a, warnings }) => {
                print_warnings(&warnings);
                apps.extend(a);
            }
            Ok(Response::Error { message, .. }) => {
                failed += 1;
                eprintln!("vexor: {message}");
            }
            Ok(Response::Events { .. }) => {}
            Err(e) => {
                eprintln!("vexor: {e}");
                return ExitCode::from(code::DAEMON_UNREACHABLE);
            }
        }
    }
    print_table(&apps);
    match failed {
        0 => ExitCode::SUCCESS,
        n if n == total => ExitCode::from(code::FAILED),
        _ => ExitCode::from(code::PARTIAL),
    }
}

fn check(
    home: &std::path::Path,
    file: PathBuf,
    only: Option<String>,
    env: Option<String>,
    interpreter: Option<String>,
    json: bool,
) -> ExitCode {
    let args = StartArgs {
        target: Some(file.to_string_lossy().into_owned()),
        name: None,
        instances: None,
        interpreter,
        cwd: None,
        env: env.into_iter().collect(),
        inherit_env: false,
        spec: None,
        args: Vec::new(),
        only,
        allow_unsupported: false,
    };
    if !spec::is_ecosystem(&file) {
        return fail(&format!("{} is not an ecosystem file", file.display()));
    }
    let plan = match spec::start_plan(&args, home) {
        Ok(p) => p,
        Err(e) => return fail(&e),
    };
    let specs: Vec<serde_json::Value> = plan
        .requests
        .iter()
        .filter_map(|r| match r {
            Request::Start { spec, start } => {
                let mut v = serde_json::to_value(spec).ok()?;
                // Values may be secrets: show the keys only.
                if let Some(env) = v.get_mut("env").and_then(|e| e.as_object_mut()) {
                    for val in env.values_mut() {
                        *val = serde_json::Value::String("<redacted>".into());
                    }
                }
                v["autostart"] = serde_json::Value::Bool(*start);
                Some(v)
            }
            _ => None,
        })
        .collect();
    if json {
        let out = serde_json::json!({ "apps": specs, "notes": plan.notes });
        println!("{}", serde_json::to_string_pretty(&out).unwrap_or_default());
    } else {
        for s in &specs {
            println!(
                "{}: {} {} (instances: {}, cwd: {}{})",
                s["name"].as_str().unwrap_or("?"),
                s["exec"].as_str().unwrap_or("?"),
                s["args"]
                    .as_array()
                    .map(|a| a
                        .iter()
                        .filter_map(|x| x.as_str())
                        .collect::<Vec<_>>()
                        .join(" "))
                    .unwrap_or_default(),
                s["instances"],
                s["cwd"].as_str().unwrap_or("?"),
                if s["autostart"] == false {
                    ", autostart: false"
                } else {
                    ""
                }
            );
        }
        print_notes(&plan.notes);
    }
    if plan.has_fatal() {
        ExitCode::from(code::FAILED)
    } else {
        ExitCode::SUCCESS
    }
}

fn print_warnings(warnings: &[String]) {
    for w in warnings {
        eprintln!("vexor: warning: {w}");
    }
}

fn print_events(events: &[serde_json::Value]) {
    let mut rows = vec![[
        "TIME (UTC)".to_owned(),
        "APP".to_owned(),
        "INST".to_owned(),
        "EVENT".to_owned(),
        "DETAIL".to_owned(),
    ]];
    for e in events {
        let kind = e["type"].as_str().unwrap_or("?");
        let event = if kind == "state" {
            e["state"].as_str().unwrap_or("?").to_owned()
        } else {
            kind.to_owned()
        };
        let mut detail = Vec::new();
        if let Some(p) = e["pid"].as_u64() {
            detail.push(format!("pid={p}"));
        }
        if let Some(c) = e["exit"]["code"].as_i64() {
            detail.push(format!("exit={c}"));
        }
        if let Some(s) = e["exit"]["signal"].as_i64() {
            detail.push(format!("signal={s}"));
        }
        if let Some(r) = e["restarts"].as_u64().filter(|r| *r > 0) {
            detail.push(format!("restarts={r}"));
        }
        if let Some(r) = e["error"]["reason"].as_str() {
            detail.push(format!("error={r}"));
        }
        if let Some(s) = e["signal"].as_str() {
            detail.push(format!("signal={s}"));
        }
        if let Some(err) = e["error"].as_str() {
            detail.push(err.to_owned());
        }
        rows.push([
            e["ts_ms"]
                .as_u64()
                .map_or_else(|| "-".to_owned(), format_utc),
            e["app"].as_str().unwrap_or("-").to_owned(),
            e["instance"]
                .as_u64()
                .map_or_else(|| "-".to_owned(), |i| i.to_string()),
            event,
            detail.join(" "),
        ]);
    }
    print_rows(&rows);
}

/// `YYYY-MM-DD HH:MM:SS.mmm` in UTC, without a date library.
fn format_utc(ms: u64) -> String {
    let secs = ms / 1000;
    let days = i64::try_from(secs / 86_400).unwrap_or(0);
    let rem = secs % 86_400;
    // Howard Hinnant's civil_from_days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02}:{:02}.{:03}",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60,
        ms % 1000
    )
}

fn parse_duration(s: &str) -> Result<std::time::Duration, String> {
    let (num, unit) = s.split_at(s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len()));
    let n: u64 = num
        .parse()
        .map_err(|_| format!("expected a number with s/m/h/d, got {s:?}"))?;
    let mult = match unit {
        "s" | "" => 1,
        "m" => 60,
        "h" => 3600,
        "d" => 86_400,
        _ => return Err(format!("unknown unit {unit:?}; use s, m, h or d")),
    };
    Ok(std::time::Duration::from_secs(n.saturating_mul(mult)))
}

fn print_rows<const N: usize>(rows: &[[String; N]]) {
    let widths: Vec<usize> = (0..N)
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
    print_rows(&rows);
}

fn fail(msg: &str) -> ExitCode {
    eprintln!("vexor: {msg}");
    ExitCode::from(code::FAILED)
}

fn exit_code(code: i32) -> ExitCode {
    ExitCode::from(u8::try_from(code).unwrap_or(1))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utc_formatting() {
        assert_eq!(format_utc(0), "1970-01-01 00:00:00.000");
        assert_eq!(format_utc(951_782_400_123), "2000-02-29 00:00:00.123");
        assert_eq!(format_utc(1_792_655_356_357), "2026-10-22 07:49:16.357");
    }

    #[test]
    fn durations() {
        assert_eq!(
            parse_duration("90").ok(),
            Some(std::time::Duration::from_secs(90))
        );
        assert_eq!(
            parse_duration("10m").ok(),
            Some(std::time::Duration::from_secs(600))
        );
        assert_eq!(
            parse_duration("7d").ok(),
            Some(std::time::Duration::from_secs(604_800))
        );
        assert!(parse_duration("3w").is_err());
        assert!(parse_duration("h").is_err());
    }
}
