//! Turning `vexor start ...` into a request.
//!
//! The CLI resolves everything the app needs from the caller's environment
//! (Layer C, docs/architecture.md §1): absolute script, interpreter and cwd
//! paths, and the environment. The daemon and shim never look anything up on
//! their own PATH.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use vexor_core::{AppSpec, LogSpec, PolicySpec};
use vexor_proto::Request;

use crate::{StartArgs, target_of};

/// Variables passed to the app by default. Everything else from the caller's
/// shell stays out unless `--inherit-env` or `--env` (docs/security.md T2).
const ENV_ALLOWLIST: &[&str] = &[
    "PATH", "HOME", "USER", "LOGNAME", "SHELL", "LANG", "TZ", "TMPDIR", "NODE_ENV",
];
const ENV_PREFIX_ALLOWLIST: &[&str] = &["LC_"];

pub fn start_request(args: &StartArgs, home: &Path) -> Result<Request, String> {
    if let Some(path) = &args.spec {
        let text = std::fs::read_to_string(path)
            .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        let spec: AppSpec = serde_json::from_str(&text)
            .map_err(|e| format!("invalid spec {}: {e}", path.display()))?;
        return Ok(Request::Start {
            spec: Box::new(spec),
        });
    }
    let Some(target) = &args.target else {
        return Err("nothing to start: give a script, an app name, `all`, or --spec".into());
    };
    let as_path = Path::new(target);
    if target == "all" || !as_path.exists() {
        // Not a file: an existing app.
        if args.name.is_some()
            || args.instances.is_some()
            || args.interpreter.is_some()
            || !args.env.is_empty()
            || !args.args.is_empty()
        {
            return Err(format!(
                "{target:?} is not a file; options like --name/-i/--env only apply when starting a script"
            ));
        }
        return Ok(Request::StartExisting {
            target: target_of(target.clone()),
        });
    }
    Ok(Request::Start {
        spec: Box::new(build(args, as_path, home)?),
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

    let is_js = script
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| matches!(e, "js" | "cjs" | "mjs"));
    let interpreter = match (&args.interpreter, is_js) {
        (Some(i), _) => Some(resolve_exec(i)?),
        (None, true) => Some(resolve_exec("node").map_err(|_| {
            "`node` was not found on your PATH; pass --interpreter /path/to/node".to_owned()
        })?),
        (None, false) => None,
    };
    let (exec, mut app_args, node_ipc) = match interpreter {
        Some(i) => {
            let node = i
                .file_name()
                .and_then(|f| f.to_str())
                .is_some_and(|f| f.starts_with("node"));
            (i, vec![script.to_string_lossy().into_owned()], node)
        }
        None => (script, Vec::new(), false),
    };
    app_args.extend(args.args.iter().cloned());

    let mut env: BTreeMap<String, String> = std::env::vars()
        .filter(|(k, _)| {
            args.inherit_env
                || ENV_ALLOWLIST.contains(&k.as_str())
                || ENV_PREFIX_ALLOWLIST.iter().any(|p| k.starts_with(p))
        })
        .collect();
    for kv in &args.env {
        let (k, v) = kv
            .split_once('=')
            .filter(|(k, _)| !k.is_empty())
            .ok_or_else(|| format!("--env expects KEY=VALUE, got {kv:?}"))?;
        env.insert(k.to_owned(), v.to_owned());
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
        policy: PolicySpec::default(),
    })
}

/// Absolute path of an executable: as given if it contains `/`, otherwise
/// the first match on the caller's PATH. Symlinks are resolved so that the
/// app keeps the same runtime even if a version manager's shim directory
/// changes later.
fn resolve_exec(name: &str) -> Result<PathBuf, String> {
    use std::os::unix::fs::PermissionsExt;

    let executable = |p: &Path| {
        p.metadata()
            .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
    };
    let found = if name.contains('/') {
        let p = PathBuf::from(name);
        executable(&p).then_some(p)
    } else {
        std::env::var_os("PATH").and_then(|path| {
            std::env::split_paths(&path)
                .map(|dir| dir.join(name))
                .find(|p| executable(p))
        })
    };
    found
        .ok_or_else(|| format!("{name:?} is not an executable file or on PATH"))?
        .canonicalize()
        .map_err(|e| format!("cannot resolve {name}: {e}"))
}
