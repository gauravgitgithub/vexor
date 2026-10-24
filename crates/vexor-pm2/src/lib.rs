//! PM2 compatibility: loading ecosystem files and translating PM2 app
//! definitions into Vexor [`AppSpec`]s (docs/compatibility.md §5).
//!
//! Every PM2 field is classified. Supported fields are translated; fields
//! Vexor ignores, or supports only partly, produce a [`Note`]; fields whose
//! absence would change how the app behaves are [`Severity::Fatal`] and stop
//! the start unless the user allows them. Unknown fields are always reported.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::Serialize;
use serde_json::Value;
use vexor_core::{AppSpec, ClusterSpec, LogSpec, PolicySpec};

pub mod dump;

const LOADER: &str = include_str!("loader.js");

/// PM2 default `listen_timeout` when `wait_ready` is set, in milliseconds.
const PM2_LISTEN_TIMEOUT_MS: u64 = 3000;

/// How long a fork-mode app with a health check (and no `wait_ready`) has
/// to pass its first check, unless `listen_timeout` says otherwise.
const HEALTH_READY_TIMEOUT_MS: u64 = 30_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    /// Has no effect in Vexor; nothing about the app's lifecycle changes.
    Ignored,
    /// Applied, but not exactly as PM2 does.
    Partial,
    /// Not supported, and ignoring it would change how the app behaves.
    Fatal,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Note {
    pub app: String,
    pub field: String,
    pub severity: Severity,
    pub message: String,
}

/// Inputs from the caller's environment.
#[derive(Debug, Clone)]
pub struct Context {
    /// Directory of the ecosystem file; relative paths resolve against it.
    pub config_dir: PathBuf,
    /// `--env NAME`: applies `env_NAME`.
    pub env_name: Option<String>,
    /// The CLI caller's environment.
    pub caller_env: BTreeMap<String, String>,
    /// Pass the whole caller environment (PM2 behaviour) instead of the
    /// allowlist.
    pub inherit_env: bool,
    /// Where default log files go.
    pub log_dir: PathBuf,
}

/// Variables from the caller passed by default (docs/security.md T2).
pub const ENV_ALLOWLIST: &[&str] = &[
    "PATH", "HOME", "USER", "LOGNAME", "SHELL", "LANG", "TZ", "TMPDIR", "NODE_ENV",
];
const ENV_PREFIX_ALLOWLIST: &[&str] = &["LC_"];

pub fn allowed_env(caller: &BTreeMap<String, String>, inherit: bool) -> BTreeMap<String, String> {
    caller
        .iter()
        .filter(|(k, _)| {
            inherit
                || ENV_ALLOWLIST.contains(&k.as_str())
                || ENV_PREFIX_ALLOWLIST.iter().any(|p| k.starts_with(p))
        })
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect()
}

// ------------------------------------------------------------------ loading

/// Read an ecosystem file. `.json` is parsed directly; `.js`, `.cjs` and
/// `.mjs` are evaluated by `node` (the app's runtime), which runs code from
/// the file, exactly as PM2 does (docs/security.md T7).
pub fn load(path: &Path, node: Option<&Path>) -> Result<Value, String> {
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
    match ext {
        "json" => {
            let text = std::fs::read_to_string(path)
                .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
            serde_json::from_str(&text)
                .map_err(|e| format!("{}: invalid JSON: {e}", path.display()))
        }
        "js" | "cjs" | "mjs" => {
            let node = node.ok_or_else(|| {
                format!(
                    "{} is JavaScript and needs Node.js to load; `node` was not found (pass --interpreter /path/to/node)",
                    path.display()
                )
            })?;
            let abs = path
                .canonicalize()
                .map_err(|e| format!("cannot resolve {}: {e}", path.display()))?;
            let out = Command::new(node)
                .arg("-e")
                .arg(LOADER)
                .arg(&abs)
                .output()
                .map_err(|e| format!("cannot run {}: {e}", node.display()))?;
            let stdout = String::from_utf8_lossy(&out.stdout);
            let line = stdout.lines().last().unwrap_or("");
            let parsed: Value = serde_json::from_str(line).map_err(|_| {
                format!(
                    "loading {} failed: {}",
                    path.display(),
                    String::from_utf8_lossy(&out.stderr).trim()
                )
            })?;
            if let Some(e) = parsed.get("error").and_then(Value::as_str) {
                return Err(format!("loading {} failed: {e}", path.display()));
            }
            parsed
                .get("ok")
                .cloned()
                .ok_or_else(|| format!("loading {} returned nothing", path.display()))
        }
        "yaml" | "yml" => Err("YAML process files are not supported yet; use .js or .json".into()),
        _ => Err(format!(
            "{}: expected an ecosystem file ending in .js, .cjs, .mjs or .json",
            path.display()
        )),
    }
}

/// The app list in a loaded config: `{apps: [...]}`, `[...]`, or one app.
pub fn apps(config: &Value) -> Result<Vec<Value>, String> {
    match config {
        Value::Object(m) if m.contains_key("apps") => match &m["apps"] {
            Value::Array(a) => Ok(a.clone()),
            v @ Value::Object(_) => Ok(vec![v.clone()]),
            _ => Err("`apps` must be an array of app definitions".into()),
        },
        Value::Array(a) => Ok(a.clone()),
        Value::Object(m) if m.contains_key("script") => Ok(vec![config.clone()]),
        _ => Err("no `apps` array found in the configuration".into()),
    }
}

// --------------------------------------------------------------- translation

/// Result of translating one app.
#[derive(Debug, Clone)]
pub struct Translated {
    pub spec: AppSpec,
    pub notes: Vec<Note>,
    /// `autostart: false`: add the app but don't start it.
    pub autostart: bool,
}

/// How PM2 runs an app's script.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ExecMode {
    Fork,
    Cluster,
}

struct Ctx {
    app: String,
    notes: Vec<Note>,
}

impl Ctx {
    fn note(&mut self, field: &str, severity: Severity, message: impl Into<String>) {
        self.notes.push(Note {
            app: self.app.clone(),
            field: field.to_owned(),
            severity,
            message: message.into(),
        });
    }
}

/// Fields Vexor accepts and has no effect on (PM2 monitoring, deploy, …).
const IGNORED: &[(&str, &str)] = &[
    ("vizion", "git metadata collection is a PM2 Plus feature"),
    ("pmx", "PM2 Plus / @pm2/io integration"),
    ("automation", "PM2 Plus / @pm2/io integration"),
    ("trace", "PM2 Plus tracing"),
    ("disable_trace", "PM2 Plus tracing"),
    ("v8", "PM2 Plus metrics"),
    ("event_loop_inspector", "PM2 Plus metrics"),
    ("deep_monitoring", "PM2 Plus metrics"),
    (
        "source_map_support",
        "PM2 injects source-map-support; Vexor doesn't modify the app",
    ),
    (
        "disable_source_map_support",
        "PM2 injects source-map-support; Vexor doesn't modify the app",
    ),
    ("install_url", "PM2 module installation"),
    ("post_update", "PM2 deploy hooks"),
    ("execute_command", "PM2 internal"),
    ("port", "used by `pm2 serve`"),
    ("write", "PM2 internal"),
    ("io", "PM2 Plus / @pm2/io integration"),
    (
        "namespace",
        "namespaces are not supported; target apps by name",
    ),
    (
        "pid_file",
        "Vexor tracks processes by PID and start time, not PID files",
    ),
    (
        "kill_retry_time",
        "Vexor sends SIGKILL once after kill_timeout",
    ),
    ("windowsHide", "Windows only"),
    ("force", "Vexor never starts two apps with the same name"),
    ("log_type", "log lines are written as the app prints them"),
    (
        "filter_env",
        "Vexor passes only an allowlist of the caller's environment by default",
    ),
];

/// Fields whose absence changes behaviour.
const FATAL: &[(&str, &str)] = &[];

/// A user or group given as a name or a number.
fn id_field(v: Option<&Value>) -> Option<String> {
    match v? {
        Value::String(s) if !s.is_empty() => Some(s.clone()),
        Value::Number(n) => n.as_u64().map(|n| n.to_string()),
        _ => None,
    }
}

/// Every field this translator knows, for unknown-field reporting.
const HANDLED: &[&str] = &[
    "name",
    "script",
    "cwd",
    "args",
    "interpreter",
    "exec_interpreter",
    "node_args",
    "interpreter_args",
    "env",
    "instances",
    "exec_mode",
    "autorestart",
    "autostart",
    "max_restarts",
    "min_uptime",
    "restart_delay",
    "exp_backoff_restart_delay",
    "kill_timeout",
    "wait_ready",
    "listen_timeout",
    "stop_exit_codes",
    "out_file",
    "error_file",
    "log_file",
    "merge_logs",
    "combine_logs",
    "disable_logs",
    "instance_var",
    "increment_var",
    "shutdown_with_message",
    "uid",
    "gid",
    "username",
    "treekill",
    "watch",
    "ignore_watch",
    "watch_delay",
    "watch_options",
    "append_env_to_name",
    "kill_signal",
    "name_prefix",
    "max_memory_restart",
    "cron_restart",
    "health_check",
    "time",
    "log_date_format",
];

/// Translate one PM2 app definition.
pub fn translate(app: &Value, ctx: &Context) -> Result<Translated, Vec<Note>> {
    let obj = match app {
        Value::Object(m) => m,
        _ => {
            return Err(vec![Note {
                app: "?".into(),
                field: "-".into(),
                severity: Severity::Fatal,
                message: "an app definition must be an object".into(),
            }]);
        }
    };
    let script_field = obj.get("script").and_then(Value::as_str).map(str::to_owned);
    let mut name = match obj.get("name").and_then(Value::as_str) {
        Some(n) => n.to_owned(),
        None => script_field
            .as_deref()
            .and_then(|s| Path::new(s).file_stem())
            .and_then(|s| s.to_str())
            .unwrap_or("app")
            .to_owned(),
    };
    let mut c = Ctx {
        app: name.clone(),
        notes: Vec::new(),
    };

    for (k, v) in obj {
        if let Some(marker) = v.get("__vexor_unsupported__") {
            c.note(
                k,
                Severity::Fatal,
                format!(
                    "contains a {} value, which can't be used",
                    marker.as_str().unwrap_or("non-JSON")
                ),
            );
        }
    }
    for (field, why) in IGNORED {
        if obj.contains_key(*field) {
            c.note(field, Severity::Ignored, *why);
        }
    }
    for (field, why) in FATAL {
        if let Some(v) = obj.get(*field)
            && !v.is_null()
            && v != &Value::Bool(false)
        {
            c.note(field, Severity::Fatal, *why);
        }
    }
    for k in obj.keys() {
        let known = HANDLED.contains(&k.as_str())
            || IGNORED.iter().any(|(f, _)| f == k)
            || FATAL.iter().any(|(f, _)| f == k)
            || k.starts_with("env_");
        if !known {
            c.note(
                k,
                Severity::Ignored,
                "unknown field; not used by Vexor or PM2 7",
            );
        }
    }

    // Execution mode.
    let mode = match obj.get("exec_mode").and_then(Value::as_str) {
        None | Some("fork" | "fork_mode") => ExecMode::Fork,
        Some("cluster" | "cluster_mode") => ExecMode::Cluster,
        Some(other) => {
            c.note(
                "exec_mode",
                Severity::Fatal,
                format!("unknown exec_mode {other:?}"),
            );
            ExecMode::Fork
        }
    };
    let instances = instances(obj.get("instances"), &mut c);
    // `instances` alone means N fork-mode processes, as in PM2 ecosystem
    // files; `exec_mode: cluster` runs them under Vexor's cluster primary.

    // Environment: caller (allowlist), then env, then env_<name>.
    let mut env = allowed_env(&ctx.caller_env, ctx.inherit_env);
    merge_env(&mut env, obj.get("env"), "env", &mut c);
    if let Some(n) = &ctx.env_name {
        let key = format!("env_{n}");
        match obj.get(&key) {
            Some(v) => merge_env(&mut env, Some(v), &key, &mut c),
            None => c.note(
                &key,
                Severity::Ignored,
                format!("--env {n} given, but this app has no {key}"),
            ),
        }
        if obj.get("append_env_to_name").and_then(Value::as_bool) == Some(true) {
            name = format!("{name}-{n}");
            c.app.clone_from(&name);
        }
    }

    // Paths.
    let cwd = match obj.get("cwd").and_then(Value::as_str) {
        Some(d) => normalize(&ctx.config_dir.join(d)),
        None => ctx.config_dir.clone(),
    };
    let Some(script) = script_field else {
        c.note("script", Severity::Fatal, "`script` is required");
        return Err(c.notes);
    };

    // What to execute: interpreter + its args + script + app args.
    let interp_field = obj
        .get("interpreter")
        .or_else(|| obj.get("exec_interpreter"))
        .and_then(Value::as_str);
    let script_path = normalize(&cwd.join(&script));
    let ext = Path::new(&script)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("");
    let interpreter: Option<String> = match interp_field {
        Some("none") => None,
        Some(i) => Some(i.to_owned()),
        None => match ext {
            "js" | "cjs" | "mjs" => Some("node".into()),
            "py" => Some("python3".into()),
            "sh" => Some("bash".into()),
            "rb" => Some("ruby".into()),
            "php" => Some("php".into()),
            "pl" => Some("perl".into()),
            "ts" | "coffee" | "ls" => {
                c.note(
                    "interpreter",
                    Severity::Fatal,
                    format!(".{ext} scripts need an explicit `interpreter` (PM2 bundles its own; Vexor doesn't)"),
                );
                None
            }
            _ => None,
        },
    };
    let mut args: Vec<String> = Vec::new();
    let interp_args = arg_list(
        obj.get("interpreter_args").or_else(|| obj.get("node_args")),
        "node_args",
        &mut c,
    );
    let app_args = arg_list(obj.get("args"), "args", &mut c);
    let (exec, node_ipc) = match &interpreter {
        Some(i) => {
            let exec = match resolve_exec(i, &env) {
                Ok(p) => p,
                Err(e) => {
                    c.note("interpreter", Severity::Fatal, e);
                    PathBuf::from(i)
                }
            };
            let is_node = exec
                .file_name()
                .and_then(|f| f.to_str())
                .is_some_and(|f| f.starts_with("node"));
            args.extend(interp_args.iter().cloned());
            args.push(script_path.to_string_lossy().into_owned());
            (exec, is_node)
        }
        None => {
            if !interp_args.is_empty() {
                c.note(
                    "node_args",
                    Severity::Ignored,
                    "no interpreter, so interpreter arguments are not used",
                );
            }
            // A script path is used as written (not resolved through
            // symlinks, so `current -> releases/N` deploys keep working).
            // A bare command name (`script: "npm"`) is looked up on PATH,
            // unless it already failed for lack of an interpreter.
            let needs_interpreter = matches!(ext, "ts" | "coffee" | "ls");
            let exec = if needs_interpreter || script.contains('/') || script_path.exists() {
                script_path.clone()
            } else {
                match resolve_exec(&script, &env) {
                    Ok(p) => p,
                    Err(e) => {
                        c.note("script", Severity::Fatal, e);
                        script_path.clone()
                    }
                }
            };
            (exec, false)
        }
    };
    args.extend(app_args.iter().cloned());

    let cluster = if mode == ExecMode::Cluster {
        if node_ipc {
            Some(ClusterSpec {
                script: script_path.clone(),
                node_args: interp_args,
                args: app_args,
            })
        } else {
            c.note(
                "exec_mode",
                Severity::Fatal,
                "cluster mode needs a Node.js script (PM2 uses Node's cluster module)",
            );
            None
        }
    } else {
        None
    };

    // Restart and readiness policy.
    let mut policy = PolicySpec::default();
    if let Some(b) = obj.get("autorestart").and_then(Value::as_bool) {
        policy.autorestart = b;
    }
    if let Some(n) = number(obj.get("max_restarts"), "max_restarts", &mut c) {
        policy.max_restarts = if n == 0 {
            // PM2 treats 0 as unset (its default, 16), not "never restart".
            c.note(
                "max_restarts",
                Severity::Partial,
                "0 means PM2's default (16 runs in a row) in PM2; Vexor does the same. Use autorestart: false to never restart",
            );
            16
        } else {
            u32::try_from(n).unwrap_or(u32::MAX)
        };
    }
    if let Some(ms) = duration_ms(obj.get("min_uptime"), "min_uptime", &mut c) {
        policy.min_uptime_ms = ms;
    }
    if let Some(ms) = duration_ms(obj.get("restart_delay"), "restart_delay", &mut c) {
        policy.restart_delay_ms = ms;
    }
    if let Some(ms) = duration_ms(
        obj.get("exp_backoff_restart_delay"),
        "exp_backoff_restart_delay",
        &mut c,
    ) {
        policy.exp_backoff_initial_ms = Some(ms);
    }
    if let Some(ms) = duration_ms(obj.get("kill_timeout"), "kill_timeout", &mut c) {
        policy.kill_timeout_ms = ms;
    }
    match obj.get("kill_signal") {
        None | Some(Value::Null) => {}
        Some(Value::String(sig)) => match vexor_core::KillSignal::parse(sig) {
            Some(k) => policy.kill_signal = k,
            None => c.note(
                "kill_signal",
                Severity::Fatal,
                format!("unsupported signal {sig:?}; use SIGINT, SIGTERM, SIGHUP, SIGQUIT, SIGUSR1 or SIGUSR2"),
            ),
        },
        Some(_) => c.note("kill_signal", Severity::Fatal, "expected a signal name such as \"SIGTERM\""),
    }
    match obj.get("max_memory_restart") {
        None | Some(Value::Null) => {}
        Some(v) => match parse_memory(v) {
            Some(0) | None => c.note(
                "max_memory_restart",
                Severity::Fatal,
                format!("expected bytes or a size like \"200M\" (K, M, G), got {v}"),
            ),
            Some(bytes) => policy.max_memory_bytes = Some(bytes),
        },
    }
    let cron_restart = match obj.get("cron_restart") {
        None | Some(Value::Null | Value::Bool(false)) => None,
        Some(Value::String(p)) => match check_cron(p) {
            Ok(()) => Some(p.trim().to_owned()),
            Err(e) => {
                c.note("cron_restart", Severity::Fatal, e);
                None
            }
        },
        Some(_) => {
            c.note(
                "cron_restart",
                Severity::Fatal,
                "expected a cron pattern such as \"0 3 * * *\"",
            );
            None
        }
    };
    let watch = watch_spec(obj, &cwd, &mut c);
    let health_check = health_check_spec(
        obj.get("health_check"),
        mode == ExecMode::Cluster,
        &mut policy,
        &mut c,
    );
    let wait_ready = obj.get("wait_ready").and_then(Value::as_bool) == Some(true);
    let listen_timeout = duration_ms(obj.get("listen_timeout"), "listen_timeout", &mut c);
    let shutdown_with_message =
        obj.get("shutdown_with_message").and_then(Value::as_bool) == Some(true);
    if shutdown_with_message && !node_ipc {
        c.note(
            "shutdown_with_message",
            Severity::Fatal,
            "shutdown_with_message needs a Node.js app (it gets process.on('message'))",
        );
    }
    if wait_ready {
        if !node_ipc {
            c.note(
                "wait_ready",
                Severity::Fatal,
                "wait_ready needs a Node.js app (it listens for process.send('ready'))",
            );
        }
        policy.ready_timeout_ms = Some(listen_timeout.unwrap_or(PM2_LISTEN_TIMEOUT_MS));
    } else if health_check.is_some() && mode == ExecMode::Fork {
        // Ready when the health check first passes.
        policy.ready_timeout_ms = Some(listen_timeout.unwrap_or(HEALTH_READY_TIMEOUT_MS));
    } else if listen_timeout.is_some() {
        c.note(
            "listen_timeout",
            Severity::Partial,
            "without wait_ready, Vexor marks the app online when it starts; listen_timeout is not used",
        );
    }
    match obj.get("stop_exit_codes") {
        None | Some(Value::Null) => {}
        Some(Value::Number(n)) => {
            policy.stop_exit_codes = n
                .as_i64()
                .and_then(|n| i32::try_from(n).ok())
                .into_iter()
                .collect()
        }
        Some(Value::Array(a)) => {
            policy.stop_exit_codes = a
                .iter()
                .filter_map(|v| v.as_i64().and_then(|n| i32::try_from(n).ok()))
                .collect();
        }
        Some(_) => c.note(
            "stop_exit_codes",
            Severity::Fatal,
            "expected a number or an array of numbers",
        ),
    }

    // Logs.
    let disabled = obj.get("disable_logs").and_then(Value::as_bool) == Some(true);
    let log_path = |field: &str, default: String| -> PathBuf {
        match obj.get(field).and_then(Value::as_str) {
            Some(p) => cwd.join(p),
            None => ctx.log_dir.join(default),
        }
    };
    let mut logs = if disabled {
        LogSpec::new("/dev/null".into(), "/dev/null".into())
    } else {
        LogSpec::new(
            log_path("out_file", format!("{name}-out.log")),
            log_path("error_file", format!("{name}-error.log")),
        )
    };
    if logs.out == Path::new("/dev/null") || logs.err == Path::new("/dev/null") {
        // Never rotate (rename) a device file.
        logs.max_size_bytes = 0;
    }
    // Vexor timestamps plain-text log lines by default; `time: false` turns
    // it off (PM2's `time: true` turns it on, which is already the case).
    if obj.get("time").and_then(Value::as_bool) == Some(false) {
        logs.timestamps = false;
    }
    if obj.contains_key("log_date_format") {
        c.note(
            "log_date_format",
            Severity::Ignored,
            "lines are timestamped in Vexor's own format (ISO 8601 with milliseconds and UTC offset)",
        );
    }
    if obj.contains_key("log_file") {
        c.note(
            "log_file",
            Severity::Partial,
            "a combined out+err log is not written; out_file and error_file are",
        );
    }
    if instances > 1 && obj.get("merge_logs").and_then(Value::as_bool) != Some(true) {
        c.note(
            "merge_logs",
            Severity::Partial,
            "all instances write to the same log files (PM2 adds an -<id> suffix per instance unless merge_logs is set)",
        );
    }

    let spec = AppSpec {
        name: name.clone(),
        exec,
        args,
        cwd,
        env,
        instances,
        instance_var: obj
            .get("instance_var")
            .and_then(Value::as_str)
            .unwrap_or("NODE_APP_INSTANCE")
            .to_owned(),
        node_ipc,
        treekill: obj.get("treekill").and_then(Value::as_bool).unwrap_or(true),
        logs,
        policy,
        // Cluster mode always uses the IPC channel to control the primary.
        cluster,
        cron_restart,
        watch,
        health_check,
        increment_var: obj
            .get("increment_var")
            .and_then(Value::as_str)
            .map(str::to_owned),
        // Without IPC it can't apply: reported above as fatal, and with
        // --allow-unsupported the app gets the stop signal instead.
        shutdown_with_message: shutdown_with_message && node_ipc,
        user: id_field(obj.get("uid").or_else(|| obj.get("username"))),
        group: id_field(obj.get("gid")),
    };
    let autostart = obj
        .get("autostart")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    Ok(Translated {
        spec,
        notes: c.notes,
        autostart,
    })
}

/// Drop `.` components and resolve `..` lexically (no filesystem access):
/// `/srv/./app/../x.js` → `/srv/x.js`.
fn normalize(p: &Path) -> PathBuf {
    use std::path::Component;
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    out.push("..");
                }
            }
            other => out.push(other),
        }
    }
    out
}

fn merge_env(env: &mut BTreeMap<String, String>, v: Option<&Value>, field: &str, c: &mut Ctx) {
    match v {
        None | Some(Value::Null) => {}
        Some(Value::Object(m)) => {
            for (k, v) in m {
                let s = match v {
                    Value::String(s) => s.clone(),
                    Value::Null => continue,
                    // PM2 stringifies non-string values (objects as JSON).
                    other => other.to_string(),
                };
                env.insert(k.clone(), s);
            }
        }
        Some(_) => c.note(field, Severity::Fatal, "expected an object of KEY: value"),
    }
}

fn instances(v: Option<&Value>, c: &mut Ctx) -> u32 {
    let cpus =
        std::thread::available_parallelism().map_or(1, |n| u32::try_from(n.get()).unwrap_or(1));
    let n: i64 = match v {
        None | Some(Value::Null) => return 1,
        Some(Value::String(s)) if s == "max" => 0,
        Some(Value::String(s)) => match s.parse() {
            Ok(n) => n,
            Err(_) => {
                c.note(
                    "instances",
                    Severity::Fatal,
                    format!("expected a number or \"max\", got {s:?}"),
                );
                return 1;
            }
        },
        Some(Value::Number(n)) => n.as_i64().unwrap_or(1),
        Some(_) => {
            c.note("instances", Severity::Fatal, "expected a number or \"max\"");
            return 1;
        }
    };
    // PM2: 0 or "max" = one per CPU; -N = CPUs minus N (at least 1).
    match n {
        0 => cpus,
        n if n < 0 => {
            let minus = u32::try_from(-n).unwrap_or(u32::MAX);
            cpus.saturating_sub(minus).max(1)
        }
        n => u32::try_from(n).unwrap_or(u32::MAX),
    }
}

fn number(v: Option<&Value>, field: &str, c: &mut Ctx) -> Option<u64> {
    match v {
        None | Some(Value::Null) => None,
        Some(Value::Number(n)) => n.as_u64(),
        Some(Value::String(s)) => s.parse().ok().or_else(|| {
            c.note(
                field,
                Severity::Fatal,
                format!("expected a number, got {s:?}"),
            );
            None
        }),
        Some(_) => {
            c.note(field, Severity::Fatal, "expected a number");
            None
        }
    }
}

/// Milliseconds from a number, or a string like "500ms", "30s", "5m", "1h".
fn duration_ms(v: Option<&Value>, field: &str, c: &mut Ctx) -> Option<u64> {
    match v {
        Some(Value::String(s)) => match parse_duration_ms(s) {
            Some(ms) => Some(ms),
            None => {
                c.note(
                    field,
                    Severity::Fatal,
                    format!("expected milliseconds or a duration like \"30s\", got {s:?}"),
                );
                None
            }
        },
        other => number(other, field, c),
    }
}

/// Vexor's `health_check` extension (PM2 has none outside PM2 Plus):
/// `{ http: "http://127.0.0.1:3000/health" | tcp: "127.0.0.1:3000",
///    interval: "10s", timeout: "2s", unhealthy_after: 3, restart_after: 6 }`.
fn health_check_spec(
    v: Option<&Value>,
    cluster: bool,
    policy: &mut PolicySpec,
    c: &mut Ctx,
) -> Option<vexor_core::HealthCheckSpec> {
    let obj = match v {
        None | Some(Value::Null) => return None,
        Some(Value::Object(o)) => o,
        Some(_) => {
            c.note(
                "health_check",
                Severity::Fatal,
                "expected an object such as { http: \"http://127.0.0.1:3000/health\" }",
            );
            return None;
        }
    };
    for k in obj.keys() {
        if !matches!(
            k.as_str(),
            "http" | "tcp" | "interval" | "timeout" | "unhealthy_after" | "restart_after"
        ) {
            c.note(
                "health_check",
                Severity::Fatal,
                format!("unknown setting health_check.{k}"),
            );
            return None;
        }
    }
    let text = |k: &str| obj.get(k).and_then(Value::as_str).map(str::to_owned);
    let spec = vexor_core::HealthCheckSpec {
        http: text("http"),
        tcp: text("tcp"),
        interval_ms: duration_ms(obj.get("interval"), "health_check.interval", c).unwrap_or(10_000),
        timeout_ms: duration_ms(obj.get("timeout"), "health_check.timeout", c).unwrap_or(2_000),
    };
    if let Err(e) = spec.target((!cluster).then_some(0)) {
        c.note("health_check", Severity::Fatal, e);
        return None;
    }
    let count = |k: &str, default: u32, c: &mut Ctx| {
        number(obj.get(k), &format!("health_check.{k}"), c)
            .map_or(default, |n| u32::try_from(n).unwrap_or(u32::MAX).max(1))
    };
    let unhealthy_after = count("unhealthy_after", 3, c);
    let restart_after = count("restart_after", 6, c).max(unhealthy_after);
    policy.health_unhealthy_after = Some(unhealthy_after);
    policy.health_restart_after = Some(restart_after);
    Some(spec)
}

/// PM2 `watch` / `ignore_watch` / `watch_delay` (docs/compatibility.md).
fn watch_spec(
    obj: &serde_json::Map<String, Value>,
    cwd: &Path,
    c: &mut Ctx,
) -> Option<vexor_core::WatchSpec> {
    let list = |v: &Value| -> Option<Vec<String>> {
        match v {
            Value::String(s) => Some(vec![s.clone()]),
            Value::Array(a) => a.iter().map(|x| x.as_str().map(str::to_owned)).collect(),
            _ => None,
        }
    };
    let paths: Vec<PathBuf> = match obj.get("watch") {
        None | Some(Value::Null | Value::Bool(false)) => return None,
        Some(Value::Bool(true)) => vec![cwd.to_path_buf()],
        Some(v) => match list(v) {
            Some(l) if l.is_empty() => vec![cwd.to_path_buf()],
            Some(l) => l.iter().map(|p| normalize(&cwd.join(p))).collect(),
            None => {
                c.note(
                    "watch",
                    Severity::Fatal,
                    "expected true, a path, or an array of paths",
                );
                return None;
            }
        },
    };
    for p in &paths {
        if !p.exists() {
            c.note(
                "watch",
                Severity::Partial,
                format!(
                    "{} does not exist; it is watched once it does only if its parent is watched",
                    p.display()
                ),
            );
        }
    }
    let ignore = match obj.get("ignore_watch") {
        None | Some(Value::Null) => vexor_core::WatchSpec::default_ignore(),
        Some(v) => match list(v) {
            Some(l) => l,
            None => {
                c.note(
                    "ignore_watch",
                    Severity::Fatal,
                    "expected a pattern or an array of patterns",
                );
                return None;
            }
        },
    };
    for p in &ignore {
        if let Err(e) = globset::Glob::new(p.trim_end_matches('/')) {
            c.note(
                "ignore_watch",
                Severity::Fatal,
                format!("invalid pattern {p:?}: {e}"),
            );
            return None;
        }
        if p.contains('\\') || p.starts_with('^') || p.ends_with('$') || p.contains('|') {
            c.note(
                "ignore_watch",
                Severity::Partial,
                format!("{p:?} looks like a regular expression; Vexor matches ignore_watch entries as glob patterns (e.g. \"*.log\", \"node_modules\")"),
            );
        }
    }
    let delay_ms = duration_ms(obj.get("watch_delay"), "watch_delay", c).unwrap_or(0);
    if obj.contains_key("watch_options") {
        c.note(
            "watch_options",
            Severity::Ignored,
            "chokidar options are not used; Vexor watches with inotify (Linux) or by polling every second",
        );
    }
    Some(vexor_core::WatchSpec {
        paths,
        ignore,
        delay_ms,
    })
}

/// Validate a `cron_restart` pattern as PM2 7 reads it (its `croner`
/// library: 5 fields, or 6 with seconds first). The shim parses it the same
/// way (`vexor-shim/src/cron.rs`).
pub fn check_cron(pattern: &str) -> Result<(), String> {
    use croner::parser::{CronParser, Seconds, Year};
    CronParser::builder()
        .seconds(Seconds::Optional)
        .year(Year::Disallowed)
        .build()
        .parse(pattern)
        .map(|_| ())
        .map_err(|e| format!("invalid cron pattern {pattern:?}: {e}"))
}

/// PM2 memory sizes: bytes as a number or string, or a number followed by
/// K, M or G (powers of 1024), e.g. "200M".
pub fn parse_memory(v: &Value) -> Option<u64> {
    match v {
        Value::Number(n) => n.as_u64(),
        Value::String(s) => {
            let s = s.trim();
            let (num, mult) = match s.chars().last()?.to_ascii_uppercase() {
                'K' => (&s[..s.len() - 1], 1u64 << 10),
                'M' => (&s[..s.len() - 1], 1 << 20),
                'G' => (&s[..s.len() - 1], 1 << 30),
                _ => (s, 1),
            };
            num.trim().parse::<u64>().ok()?.checked_mul(mult)
        }
        _ => None,
    }
}

pub fn parse_duration_ms(s: &str) -> Option<u64> {
    let s = s.trim();
    let split = s
        .find(|ch: char| !ch.is_ascii_digit() && ch != '.')
        .unwrap_or(s.len());
    let (num, unit) = s.split_at(split);
    let n: f64 = num.parse().ok()?;
    let mult = match unit.trim() {
        "" | "ms" => 1.0,
        "s" => 1000.0,
        "m" => 60_000.0,
        "h" => 3_600_000.0,
        _ => return None,
    };
    let ms = n * mult;
    (0.0..1e15).contains(&ms).then(|| {
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let v = ms.round() as u64;
        v
    })
}

fn arg_list(v: Option<&Value>, field: &str, c: &mut Ctx) -> Vec<String> {
    match v {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::String(s)) => split_args(s).unwrap_or_else(|e| {
            c.note(field, Severity::Fatal, e);
            Vec::new()
        }),
        Some(Value::Array(a)) => a
            .iter()
            .map(|x| match x {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            })
            .collect(),
        Some(_) => {
            c.note(
                field,
                Severity::Fatal,
                "expected a string or an array of strings",
            );
            Vec::new()
        }
    }
}

/// Split an argument string on whitespace, honouring single and double
/// quotes and backslash escapes. No variable or glob expansion: arguments
/// never pass through a shell.
pub fn split_args(s: &str) -> Result<Vec<String>, String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_word = false;
    let mut chars = s.chars();
    while let Some(ch) = chars.next() {
        match ch {
            '\'' => {
                in_word = true;
                loop {
                    match chars.next() {
                        Some('\'') => break,
                        Some(c) => cur.push(c),
                        None => return Err(format!("unclosed ' in {s:?}")),
                    }
                }
            }
            '"' => {
                in_word = true;
                loop {
                    match chars.next() {
                        Some('"') => break,
                        Some('\\') => match chars.next() {
                            Some(c) => cur.push(c),
                            None => return Err(format!("trailing \\ in {s:?}")),
                        },
                        Some(c) => cur.push(c),
                        None => return Err(format!("unclosed \" in {s:?}")),
                    }
                }
            }
            '\\' => {
                in_word = true;
                match chars.next() {
                    Some(c) => cur.push(c),
                    None => return Err(format!("trailing \\ in {s:?}")),
                }
            }
            c if c.is_whitespace() => {
                if in_word {
                    out.push(std::mem::take(&mut cur));
                    in_word = false;
                }
            }
            c => {
                in_word = true;
                cur.push(c);
            }
        }
    }
    if in_word {
        out.push(cur);
    }
    Ok(out)
}

/// Absolute path of an executable: as given if it contains `/`, otherwise
/// the first match on `env`'s PATH. Symlinks are resolved so the app keeps
/// the same runtime if a version manager's shim directory changes later.
pub fn resolve_exec(name: &str, env: &BTreeMap<String, String>) -> Result<PathBuf, String> {
    use std::os::unix::fs::PermissionsExt;
    let executable = |p: &Path| {
        p.metadata()
            .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
    };
    let found = if name.contains('/') {
        let p = PathBuf::from(name);
        executable(&p).then_some(p)
    } else {
        env.get("PATH").and_then(|path| {
            std::env::split_paths(path)
                .map(|dir| dir.join(name))
                .find(|p| executable(p))
        })
    };
    found
        .ok_or_else(|| format!("{name:?} is not an executable file or on PATH"))?
        .canonicalize()
        .map_err(|e| format!("cannot resolve {name}: {e}"))
}

/// Translate every app; `only` limits to the named apps.
pub fn translate_all(
    config: &Value,
    ctx: &Context,
    only: &[String],
) -> Result<(Vec<Translated>, Vec<Note>), String> {
    let list = apps(config)?;
    let mut ok = Vec::new();
    let mut notes = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    for app in &list {
        let name = app.get("name").and_then(Value::as_str).unwrap_or("");
        if !only.is_empty() && !only.iter().any(|o| o == name) {
            continue;
        }
        match translate(app, ctx) {
            Ok(t) => {
                if !seen.insert(t.spec.name.clone()) {
                    notes.push(Note {
                        app: t.spec.name.clone(),
                        field: "name".into(),
                        severity: Severity::Fatal,
                        message: "two apps have this name".into(),
                    });
                }
                notes.extend(t.notes.iter().cloned());
                ok.push(t);
            }
            Err(n) => notes.extend(n),
        }
    }
    for o in only {
        if !list
            .iter()
            .any(|a| a.get("name").and_then(Value::as_str) == Some(o))
        {
            return Err(format!(
                "--only {o}: no app with that name in the configuration"
            ));
        }
    }
    Ok((ok, notes))
}

#[cfg(test)]
mod tests;
