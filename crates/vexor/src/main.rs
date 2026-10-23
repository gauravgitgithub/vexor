//! `vexor`: process manager CLI.

mod apply;
mod client;
mod describe;
mod logs;
mod migrate;
mod spec;
mod startup;

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
    /// Replace apps' processes without a gap where possible: cluster mode
    /// (and fork mode with `reload_strategy: start_first`) starts the new
    /// process and waits until it is healthy before stopping the old one;
    /// otherwise this is a restart.
    Reload { target: String },
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
    /// Write every app and its desired state to a snapshot (PM2 `save`).
    /// Vexor also keeps its state on every change; the snapshot is for
    /// restoring later with `resurrect`.
    Save,
    /// Restore apps from the `save` snapshot: add missing ones and start
    /// those saved as running.
    Resurrect,
    /// Start the daemon at boot (systemd) or login (launchd). Prints the
    /// `sudo` command when root is needed.
    Startup {
        /// systemd or launchd [default: detected].
        init: Option<String>,
        /// User the daemon runs as [default: you, or SUDO_USER under sudo].
        #[arg(long)]
        user: Option<String>,
        /// Print the service file instead of installing it.
        #[arg(long)]
        print: bool,
    },
    /// Remove the boot/login service. Running apps are not stopped.
    Unstartup {
        /// systemd or launchd [default: detected].
        init: Option<String>,
        /// User the service was installed for.
        #[arg(long)]
        user: Option<String>,
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
    /// Show an app's configuration, status and recent events.
    #[command(visible_alias = "show")]
    Describe {
        name: String,
        /// Machine-readable output.
        #[arg(long)]
        json: bool,
        /// Show environment variable values (hidden by default).
        #[arg(long)]
        show_env: bool,
    },
    /// Show app logs: the last lines of each app's log files, or with
    /// --follow, new lines as they are written.
    Logs {
        /// Only this app [default: all apps].
        name: Option<String>,
        /// Lines to show from each log file.
        #[arg(long, default_value_t = 15)]
        lines: usize,
        /// Keep printing new lines (Ctrl-C to stop).
        #[arg(short, long)]
        follow: bool,
        /// Only standard output.
        #[arg(long, conflicts_with = "err")]
        out: bool,
        /// Only standard error.
        #[arg(long)]
        err: bool,
        /// No "app|stream |" prefix or headers.
        #[arg(long)]
        raw: bool,
    },
    /// Plan a move from PM2: reads the process list PM2 saved (`pm2 save`)
    /// and shows how each app would run under Vexor, what can't be moved
    /// yet, and the expected downtime. Changes nothing.
    Migrate {
        /// Migrate from PM2.
        #[arg(long, required_unless_present_any = ["rollback", "status"])]
        from_pm2: bool,
        /// Only report, change nothing (the default; applying comes in a
        /// later release).
        #[arg(long)]
        dry_run: bool,
        /// Copy PM2's state (dump.pm2, its boot service, …) and this plan to
        /// a new directory under <home>/migrations/. PM2 is not touched.
        #[arg(long, conflicts_with = "dry_run")]
        backup: bool,
        /// Move the apps: for each, stop it under PM2, start it under Vexor
        /// and wait until healthy; put it back under PM2 if it isn't.
        /// Applies the plan of the newest backup (or --backup-id), which must
        /// match PM2's current dump. Asks for confirmation unless --yes.
        #[arg(long, conflicts_with_all = ["dry_run", "backup"])]
        apply: bool,
        /// With --apply: the backup whose plan to apply [default: newest].
        #[arg(long, requires = "apply")]
        backup_id: Option<String>,
        /// Undo a migration (ID, default: the newest): each migrated app,
        /// newest first, is deleted from Vexor and started again under PM2.
        /// Asks before each app unless --yes.
        #[arg(long, value_name = "ID", num_args = 0..=1, conflicts_with_all = ["dry_run", "backup", "apply", "status"])]
        rollback: Option<Option<String>>,
        /// Show migrations (or one, by ID) and where each app stands.
        #[arg(long, value_name = "ID", num_args = 0..=1, conflicts_with_all = ["dry_run", "backup", "apply"])]
        status: Option<Option<String>>,
        /// With --apply or --rollback: only these apps (comma-separated).
        #[arg(long)]
        only: Option<String>,
        /// With --apply or --rollback: don't ask for confirmation.
        #[arg(long, short = 'y')]
        yes: bool,
        /// With --apply or --rollback: the pm2 command [default: pm2 on PATH].
        #[arg(long)]
        pm2: Option<PathBuf>,
        /// With --apply or --rollback: seconds to wait for each app to
        /// become healthy (with --apply, plus its listen_timeout) [default: 30].
        #[arg(long)]
        timeout: Option<u64>,
        /// PM2 home directory [default: $PM2_HOME, else ~/.pm2]
        #[arg(long)]
        pm2_home: Option<PathBuf>,
        /// PM2 dump file [default: <PM2 home>/dump.pm2]
        #[arg(long)]
        dump: Option<PathBuf>,
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
    /// Number of instances. For a Node.js script this means cluster mode,
    /// as in PM2: the instances share listening ports.
    #[arg(short = 'i', long)]
    pub instances: Option<u32>,
    /// With -i: run independent processes instead of cluster mode.
    #[arg(long)]
    pub fork: bool,
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
        Command::Reload { target } => send(
            &home,
            Request::Reload {
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
        Command::Save => send(&home, Request::Save, false),
        Command::Resurrect => send(&home, Request::Resurrect, false),
        Command::Startup { init, user, print } => startup_cmd(&home, init, user, Some(print)),
        Command::Unstartup { init, user } => startup_cmd(&home, init, user, None),
        Command::Describe {
            name,
            json,
            show_env,
        } => describe_cmd(&home, name, json, show_env),
        Command::Logs {
            name,
            lines,
            follow,
            out,
            err,
            raw,
        } => logs_cmd(&home, name, lines, follow, (!err, !out), raw),
        Command::Migrate {
            from_pm2: _,
            dry_run: _,
            backup,
            apply,
            backup_id,
            rollback,
            status,
            only,
            yes,
            pm2,
            timeout,
            pm2_home,
            dump,
            json,
        } => {
            let moving = apply || rollback.is_some();
            if !moving && (only.is_some() || yes || pm2.is_some() || timeout.is_some()) {
                eprintln!(
                    "vexor: --only, --yes, --pm2 and --timeout go with --apply or --rollback"
                );
                return ExitCode::from(2);
            }
            let only: Vec<String> = only
                .map(|o| {
                    o.split(',')
                        .map(|s| s.trim().to_owned())
                        .filter(|s| !s.is_empty())
                        .collect()
                })
                .unwrap_or_default();
            let timeout = std::time::Duration::from_secs(timeout.unwrap_or(30));
            if let Some(id) = status {
                ExitCode::from(apply::status(&home, id.as_deref(), json))
            } else if let Some(id) = rollback {
                ExitCode::from(apply::run_rollback(
                    &home,
                    apply::RollbackOptions {
                        backup_id: id,
                        only,
                        yes,
                        pm2,
                        timeout,
                    },
                ))
            } else if apply {
                let pm2_home = pm2_home.unwrap_or_else(migrate::default_pm2_home);
                let dump = dump.unwrap_or_else(|| pm2_home.join("dump.pm2"));
                ExitCode::from(apply::run(
                    &home,
                    &pm2_home,
                    &dump,
                    apply::Options {
                        backup_id,
                        only,
                        yes,
                        pm2,
                        timeout,
                    },
                ))
            } else {
                migrate_cmd(&home, pm2_home, dump, backup, json)
            }
        }
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
        Response::Describe {
            status,
            spec,
            events,
            warnings,
        } => {
            print_warnings(&warnings);
            describe::print(&status, &spec, &events, false);
            ExitCode::SUCCESS
        }
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
            Ok(Response::Events { .. } | Response::Describe { .. }) => {}
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
        fork: false,
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

/// An app's status, spec and recent events, from `Request::Describe`.
type Described = (AppStatus, Box<vexor_core::AppSpec>, Vec<serde_json::Value>);

/// On failure: (exit code, message).
fn describe_app(home: &std::path::Path, name: String) -> Result<Described, (u8, String)> {
    match client::request(home, &Request::Describe { name }) {
        Ok(Response::Describe {
            status,
            spec,
            events,
            warnings,
        }) => {
            print_warnings(&warnings);
            Ok((status, spec, events))
        }
        Ok(Response::Error { message, .. }) => Err((code::FAILED, message)),
        Ok(_) => Err((code::FAILED, "unexpected answer from the daemon".into())),
        Err(e) => Err((code::DAEMON_UNREACHABLE, e)),
    }
}

fn describe_cmd(home: &std::path::Path, name: String, json: bool, show_env: bool) -> ExitCode {
    let (status, spec, events) = match describe_app(home, name) {
        Ok(d) => d,
        Err((c, msg)) => {
            eprintln!("vexor: {msg}");
            return ExitCode::from(c);
        }
    };
    if json {
        let out = serde_json::json!({
            "status": status,
            "spec": describe::spec_json(&spec, show_env),
            "events": events,
        });
        println!("{}", serde_json::to_string_pretty(&out).unwrap_or_default());
    } else {
        describe::print(&status, &spec, &events, show_env);
    }
    ExitCode::SUCCESS
}

fn logs_cmd(
    home: &std::path::Path,
    name: Option<String>,
    lines: usize,
    follow: bool,
    (want_out, want_err): (bool, bool),
    raw: bool,
) -> ExitCode {
    let names = match name {
        Some(n) => vec![n],
        None => match client::request(home, &Request::List) {
            Ok(Response::Ok { apps, .. }) => apps.into_iter().map(|a| a.name).collect(),
            Ok(Response::Error { message, .. }) => return fail(&message),
            Ok(_) => return fail("unexpected answer from the daemon"),
            Err(e) => {
                eprintln!("vexor: {e}");
                return ExitCode::from(code::DAEMON_UNREACHABLE);
            }
        },
    };
    let mut sources = Vec::new();
    for n in names {
        let spec = match describe_app(home, n) {
            Ok((_, spec, _)) => spec,
            Err((c, msg)) => {
                eprintln!("vexor: {msg}");
                return ExitCode::from(c);
            }
        };
        let l = &spec.logs;
        let merged = l.out == l.err;
        if want_out || merged {
            sources.push(logs::Source {
                label: if merged {
                    spec.name.clone()
                } else {
                    format!("{}|out", spec.name)
                },
                path: l.out.clone(),
            });
        }
        if want_err && !merged {
            sources.push(logs::Source {
                label: format!("{}|err", spec.name),
                path: l.err.clone(),
            });
        }
    }
    sources.retain(|s| s.path != std::path::Path::new("/dev/null"));
    if sources.is_empty() {
        return fail("no log files to show");
    }
    if let Err(e) = logs::print_tail(&sources, lines, raw) {
        return fail(&e.to_string());
    }
    if follow && let Err(e) = logs::follow(sources, raw) {
        return fail(&e.to_string());
    }
    ExitCode::SUCCESS
}

/// `print`: Some(print_only) for startup, None for unstartup.
fn startup_cmd(
    home: &std::path::Path,
    init: Option<String>,
    user: Option<String>,
    print: Option<bool>,
) -> ExitCode {
    let init = match init
        .as_deref()
        .map_or_else(startup::Init::detect, startup::Init::parse)
    {
        Ok(i) => i,
        Err(e) => return fail(&e),
    };
    let user = user
        .or_else(|| std::env::var("SUDO_USER").ok())
        .or_else(|| std::env::var("USER").ok());
    let Some(user) = user else {
        return fail("cannot tell which user to run as; pass --user");
    };
    let exe = match std::env::current_exe().and_then(|e| e.canonicalize()) {
        Ok(e) => e,
        Err(e) => return fail(&format!("cannot locate the vexor binary: {e}")),
    };
    let home = match std::path::absolute(home) {
        Ok(h) => h,
        Err(e) => return fail(&format!("cannot resolve {}: {e}", home.display())),
    };
    let target = startup::Target {
        user,
        home,
        exe,
        user_home: std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_default(),
    };
    let result = match print {
        Some(print_only) => startup::install(init, &target, print_only),
        None => startup::uninstall(init, &target),
    };
    match result {
        Ok(msg) => {
            println!("{}", msg.trim_end());
            ExitCode::SUCCESS
        }
        Err(e) => fail(&e),
    }
}

fn print_warnings(warnings: &[String]) {
    for w in warnings {
        eprintln!("vexor: warning: {w}");
    }
}

pub(crate) fn print_events(events: &[serde_json::Value]) {
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
pub(crate) fn format_utc(ms: u64) -> String {
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

pub(crate) fn print_rows<const N: usize>(rows: &[[String; N]]) {
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
    let now = describe::now_ms();
    let mut rows = vec![[
        "NAME".to_owned(),
        "INST".to_owned(),
        "STATUS".to_owned(),
        "PID".to_owned(),
        "UPTIME".to_owned(),
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
                "-".into(),
            ]);
        }
        for s in &app.snapshot {
            // A shim that has gone means its instances are not running,
            // whatever their last reported state.
            let stopped = app.shim_pid.is_none() && app.desired == Desired::Stopped;
            let state = if stopped {
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
                    .filter(|_| !stopped)
                    .map_or_else(|| "-".to_owned(), |p| p.to_string()),
                describe::uptime(s, now)
                    .filter(|_| !stopped)
                    .unwrap_or_else(|| "-".into()),
                s["restarts"].to_string(),
            ]);
        }
    }
    print_rows(&rows);
}

fn migrate_cmd(
    home: &std::path::Path,
    pm2_home: Option<PathBuf>,
    dump: Option<PathBuf>,
    backup: bool,
    json: bool,
) -> ExitCode {
    let pm2_home = pm2_home.unwrap_or_else(migrate::default_pm2_home);
    let dump = dump.unwrap_or_else(|| pm2_home.join("dump.pm2"));
    let report = match migrate::plan(&dump, &pm2_home) {
        Ok(r) => r,
        Err(e) => return fail(&e),
    };
    if backup {
        let (dir, manifest) =
            match migrate::backup(home, &pm2_home, &dump, &report, describe::now_ms()) {
                Ok(x) => x,
                Err(e) => return fail(&format!("backup failed, nothing was kept: {e}")),
            };
        if json {
            let out = serde_json::json!({ "backup": dir, "manifest": manifest });
            println!("{}", serde_json::to_string_pretty(&out).unwrap_or_default());
        } else {
            println!("Backup {} saved to {}:", manifest.id, dir.display());
            for f in &manifest.files {
                println!("  {} ({} bytes) <- {}", f.copy, f.size, f.source.display());
            }
            println!("  plan.json (this migration plan), manifest.json (SHA-256 of each file)");
            println!(
                "PM2 was not touched. To restore its process list from this backup: \
                 cp {}/pm2/dump.pm2 {} && pm2 resurrect",
                dir.display(),
                pm2_home.join("dump.pm2").display()
            );
            let blocked = report.blocked();
            if blocked > 0 {
                println!(
                    "{blocked} of {} apps are blocked; see `vexor migrate --from-pm2`.",
                    report.apps.len()
                );
            }
        }
        return ExitCode::SUCCESS;
    }
    if json {
        match serde_json::to_string_pretty(&report) {
            Ok(s) => println!("{s}"),
            Err(e) => return fail(&e.to_string()),
        }
    } else {
        migrate::print(&report);
    }
    if report.blocked() > 0 {
        ExitCode::from(code::PARTIAL)
    } else {
        ExitCode::SUCCESS
    }
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
