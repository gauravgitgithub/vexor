//! Turning `vexor start ...` into requests.
//!
//! The CLI resolves everything the app needs from the caller's environment
//! (Layer C, docs/architecture.md §1): absolute script, interpreter and cwd
//! paths, and the environment. The daemon and shim never look anything up on
//! their own PATH.

use std::collections::BTreeMap;
use std::path::Path;

use vexor_core::{AppSpec, ClusterSpec, LogSpec, PolicySpec};
use vexor_pm2::{Context, Note, Severity, allowed_env, resolve_exec};
use vexor_proto::Request;

use crate::{StartArgs, target_of};

/// What `vexor start` (or `vexor check`) will do.
pub struct Plan {
    pub requests: Vec<Request>,
    /// Findings about the configuration (ecosystem files only).
    pub notes: Vec<Note>,
}

impl Plan {
    pub fn has_fatal(&self) -> bool {
        self.notes.iter().any(|n| n.severity == Severity::Fatal)
    }
}

fn one(request: Request) -> Plan {
    Plan {
        requests: vec![request],
        notes: Vec::new(),
    }
}

fn caller_env() -> BTreeMap<String, String> {
    std::env::vars().collect()
}

/// PM2's rule: a process file is JSON/YAML, or named `ecosystem*` or
/// `*.config.{js,cjs,mjs}`.
pub fn is_ecosystem(path: &Path) -> bool {
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
    matches!(ext, "json" | "yml" | "yaml")
        || name.starts_with("ecosystem")
        || [".config.js", ".config.cjs", ".config.mjs"]
            .iter()
            .any(|s| name.ends_with(s))
}

pub fn start_plan(args: &StartArgs, home: &Path) -> Result<Plan, String> {
    if let Some(path) = &args.spec {
        let text = std::fs::read_to_string(path)
            .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        let spec: AppSpec = serde_json::from_str(&text)
            .map_err(|e| format!("invalid spec {}: {e}", path.display()))?;
        return Ok(one(Request::Start {
            spec: Box::new(spec),
            start: true,
        }));
    }
    let Some(target) = &args.target else {
        return Err(
            "nothing to start: give a script, an ecosystem file, an app name, `all`, or --spec"
                .into(),
        );
    };
    let as_path = Path::new(target);
    if target == "all" || !as_path.exists() {
        // Not a file: an existing app.
        if args.name.is_some()
            || args.instances.is_some()
            || args.interpreter.is_some()
            || !args.env.is_empty()
            || !args.args.is_empty()
            || args.only.is_some()
            || args.max_memory_restart.is_some()
            || args.cron_restart.is_some()
            || args.watch
            || args.health_check.is_some()
        {
            return Err(format!(
                "{target:?} is not a file; options like --name/-i/--env/--only only apply when starting a file"
            ));
        }
        return Ok(one(Request::StartExisting {
            target: target_of(target.clone()),
        }));
    }
    if is_ecosystem(as_path) {
        return ecosystem_plan(args, as_path, home);
    }
    if args.only.is_some() {
        return Err("--only applies to ecosystem files".into());
    }
    Ok(one(Request::Start {
        spec: Box::new(build(args, as_path, home)?),
        start: true,
    }))
}

fn ecosystem_plan(args: &StartArgs, path: &Path, home: &Path) -> Result<Plan, String> {
    if args.name.is_some()
        || args.instances.is_some()
        || args.fork
        || !args.args.is_empty()
        || args.max_memory_restart.is_some()
        || args.cron_restart.is_some()
        || args.watch
        || args.health_check.is_some()
    {
        return Err(
            "--name, -i, --max-memory-restart, --cron-restart, --watch, --health-check and app arguments don't apply to ecosystem files; set them in the file"
                .into(),
        );
    }
    // `--env NAME` selects `env_NAME`, as in PM2.
    let env_name = match args.env.as_slice() {
        [] => None,
        [one] if !one.contains('=') => Some(one.clone()),
        _ => {
            return Err(
                "with an ecosystem file, --env takes one environment name (e.g. --env production); set variables in the file".into(),
            );
        }
    };
    let caller = caller_env();
    let node = match &args.interpreter {
        Some(i) => Some(resolve_exec(i, &caller)?),
        None => resolve_exec("node", &caller).ok(),
    };
    let config = vexor_pm2::load(path, node.as_deref())?;
    let config_dir = path
        .canonicalize()
        .map_err(|e| format!("cannot resolve {}: {e}", path.display()))?
        .parent()
        .map(Path::to_path_buf)
        .ok_or("ecosystem file has no directory")?;
    let ctx = Context {
        config_dir,
        env_name,
        caller_env: caller,
        inherit_env: args.inherit_env,
        log_dir: home.join("logs"),
    };
    let only: Vec<String> = args
        .only
        .as_deref()
        .map(|o| {
            o.split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default();
    let (apps, notes) = vexor_pm2::translate_all(&config, &ctx, &only)?;
    if apps.is_empty() && notes.is_empty() {
        return Err("the ecosystem file defines no apps".into());
    }
    Ok(Plan {
        requests: apps
            .into_iter()
            .map(|t| Request::Start {
                spec: Box::new(t.spec),
                start: t.autostart,
            })
            .collect(),
        notes,
    })
}

fn build(args: &StartArgs, script: &Path, home: &Path) -> Result<AppSpec, String> {
    let script = script
        .canonicalize()
        .map_err(|e| format!("cannot resolve {}: {e}", script.display()))?;
    let cwd = match &args.cwd {
        Some(c) => c.clone(),
        None => std::env::current_dir().map_err(|e| format!("current directory: {e}"))?,
    }
    .canonicalize()
    .map_err(|e| format!("cannot resolve working directory: {e}"))?;
    let name = match &args.name {
        Some(n) => n.clone(),
        None => script
            .file_stem()
            .and_then(|s| s.to_str())
            .ok_or("cannot derive an app name from the script; pass --name")?
            .to_owned(),
    };

    let caller = caller_env();
    let is_js = script
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| matches!(e, "js" | "cjs" | "mjs"));
    let interpreter = match (&args.interpreter, is_js) {
        (Some(i), _) => Some(resolve_exec(i, &caller)?),
        (None, true) => Some(resolve_exec("node", &caller).map_err(|_| {
            "`node` was not found on your PATH; pass --interpreter /path/to/node".to_owned()
        })?),
        (None, false) => None,
    };
    let is_node = interpreter.as_ref().is_some_and(|i| {
        i.file_name()
            .and_then(|f| f.to_str())
            .is_some_and(|f| f.starts_with("node"))
    });
    // PM2: `-i N` on a Node.js script means cluster mode.
    let cluster = (is_node && args.instances.is_some() && !args.fork).then(|| ClusterSpec {
        script: script.clone(),
        node_args: Vec::new(),
        args: args.args.clone(),
    });
    let (exec, mut app_args, node_ipc) = match interpreter {
        Some(i) => (i, vec![script.to_string_lossy().into_owned()], is_node),
        None => (script, Vec::new(), false),
    };
    app_args.extend(args.args.iter().cloned());

    let mut env = allowed_env(&caller, args.inherit_env);
    for kv in &args.env {
        let (k, v) = kv
            .split_once('=')
            .filter(|(k, _)| !k.is_empty())
            .ok_or_else(|| format!("--env expects KEY=VALUE, got {kv:?}"))?;
        env.insert(k.to_owned(), v.to_owned());
    }

    let watch = args.watch.then(|| vexor_core::WatchSpec {
        paths: vec![cwd.clone()],
        ignore: vexor_core::WatchSpec::default_ignore(),
        delay_ms: 0,
    });
    let max_memory_bytes = match &args.max_memory_restart {
        Some(m) => Some(
            vexor_pm2::parse_memory(&serde_json::Value::String(m.clone()))
                .filter(|b| *b > 0)
                .ok_or_else(|| {
                    format!("--max-memory-restart: expected bytes or a size like 200M, got {m:?}")
                })?,
        ),
        None => None,
    };
    let health_check = match &args.health_check {
        Some(t) => {
            let (http, tcp) = if t.starts_with("http") {
                (Some(t.clone()), None)
            } else {
                (None, Some(t.clone()))
            };
            let hc = vexor_core::HealthCheckSpec {
                http,
                tcp,
                interval_ms: 10_000,
                timeout_ms: 2_000,
            };
            hc.target(cluster.is_none().then_some(0))
                .map_err(|e| format!("--health-check: {e}"))?;
            Some(hc)
        }
        None => None,
    };
    let mut policy = PolicySpec {
        max_memory_bytes,
        ..PolicySpec::default()
    };
    if health_check.is_some() {
        policy.health_unhealthy_after = Some(3);
        policy.health_restart_after = Some(6);
        if cluster.is_none() {
            policy.ready_timeout_ms = Some(30_000);
        }
    }
    let logs = home.join("logs");
    Ok(AppSpec {
        logs: LogSpec::new(
            logs.join(format!("{name}-out.log")),
            logs.join(format!("{name}-error.log")),
        ),
        name,
        exec,
        args: app_args,
        cwd,
        env,
        instances: args.instances.unwrap_or(1),
        instance_var: "NODE_APP_INSTANCE".to_owned(),
        node_ipc,
        treekill: true,
        policy,
        cluster,
        cron_restart: match &args.cron_restart {
            Some(p) => {
                vexor_pm2::check_cron(p).map_err(|e| format!("--cron-restart: {e}"))?;
                Some(p.trim().to_owned())
            }
            None => None,
        },
        watch,
        health_check,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ecosystem_file_names() {
        for yes in [
            "ecosystem.config.js",
            "ecosystem.json",
            "pm2.config.cjs",
            "x.config.mjs",
            "apps.json",
            "procs.yml",
        ] {
            assert!(is_ecosystem(Path::new(yes)), "{yes}");
        }
        for no in ["app.js", "server.mjs", "config.js", "run.sh"] {
            assert!(!is_ecosystem(Path::new(no)), "{no}");
        }
    }
}
