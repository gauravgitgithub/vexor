//! Reading PM2's `dump.pm2` (written by `pm2 save`) for migration
//! (docs/migration.md).
//!
//! The dump holds one entry per running process, so a cluster app with four
//! instances appears four times. Each entry mixes the app's configuration
//! with PM2's runtime bookkeeping, and its `env` holds the shell environment
//! `pm2` was started from as well as the app's own variables (PM2 can't tell
//! them apart either). This module groups the entries back into apps and
//! rebuilds an ecosystem-style definition for each, so the same translator
//! and field classification as `vexor check` apply.

use std::collections::BTreeMap;

use serde_json::{Map, Value};

use crate::{Note, Severity};

/// One app rebuilt from the dump.
#[derive(Debug, Clone)]
pub struct DumpApp {
    pub name: String,
    pub namespace: String,
    /// PM2's status of each instance (`online`, `stopped`, `errored`, …).
    pub statuses: Vec<String>,
    /// Node.js version PM2 ran the app with, when recorded.
    pub node_version: Option<String>,
    /// Ecosystem-style app definition for [`crate::translate`].
    pub app: Value,
    /// Findings from reading the dump itself.
    pub notes: Vec<Note>,
}

impl DumpApp {
    /// Whether PM2 was running the app when the dump was saved: any instance
    /// online (or on its way there).
    pub fn running(&self) -> bool {
        self.statuses
            .iter()
            .any(|s| matches!(s.as_str(), "online" | "launching" | "waiting restart"))
    }
}

/// The dump as a whole.
#[derive(Debug, Clone)]
pub struct Dump {
    pub apps: Vec<DumpApp>,
    /// PM2 version that wrote the dump, when recorded.
    pub pm2_version: Option<String>,
    /// Number of process entries (instances) in the dump.
    pub processes: usize,
}

/// Runtime bookkeeping PM2 stores next to the configuration. Never part of
/// the app definition.
const INTERNAL: &[&str] = &[
    "status",
    "pm_id",
    "pm_uptime",
    "created_at",
    "restart_time",
    "unstable_restarts",
    "prev_restart_delay",
    "exit_code",
    "_pm2_version",
    "version",
    "versioning",
    "node_version",
    "unique_id",
    "pm_pid_path",
    "pm_exec_path",
    "pm_cwd",
    "pm_out_log_path",
    "pm_err_log_path",
    "pm_log_path",
    "axm_actions",
    "axm_monitor",
    "axm_options",
    "axm_dynamic",
    "km_link",
    "vizion_running",
    "env",
    "username",
    "instances",
    "script",
    "cwd",
    "out_file",
    "error_file",
    "log_file",
];

/// Settings PM2 writes into every entry with its default value. Dropped when
/// they hold that default, so they aren't reported as findings for every app.
fn is_pm2_default(key: &str, v: &Value) -> bool {
    match key {
        "pmx" | "automation" | "vizion" | "windowsHide" | "autostart" | "treekill"
        | "autorestart" => v == &Value::Bool(true),
        "kill_retry_time" => v.as_u64() == Some(100),
        "namespace" => v.as_str() == Some("default"),
        "instance_var" => v.as_str() == Some("NODE_APP_INSTANCE"),
        "merge_logs" => v == &Value::Bool(false),
        "filter_env" => v == &Value::Bool(false) || v.as_array().is_some_and(Vec::is_empty),
        _ => false,
    }
}

/// Variables PM2 adds to every app's environment itself.
fn is_pm2_env(key: &str, app: &str, instance_var: &str) -> bool {
    key.starts_with("PM2_")
        || key == app
        || key == instance_var
        || matches!(key, "unique_id" | "PWD" | "OLDPWD" | "SHLVL" | "_")
}

type Group = (String, String);

/// Parse `dump.pm2` text. `user` is the user running the migration: PM2
/// records its own user in every entry, which only matters when it differs.
pub fn parse(text: &str, user: Option<&str>) -> Result<Dump, String> {
    let v: Value = serde_json::from_str(text).map_err(|e| format!("invalid JSON: {e}"))?;
    let Value::Array(entries) = v else {
        return Err("expected a JSON array of processes".into());
    };
    // (namespace, name) and the entries of its instances, in dump order.
    let mut groups: Vec<(Group, Vec<Map<String, Value>>)> = Vec::new();
    let mut pm2_version = None;
    for (i, e) in entries.iter().enumerate() {
        let Value::Object(m) = e else {
            return Err(format!("process {i} is not an object"));
        };
        let Some(name) = m.get("name").and_then(Value::as_str) else {
            return Err(format!("process {i} has no name"));
        };
        let ns = m
            .get("namespace")
            .and_then(Value::as_str)
            .unwrap_or("default")
            .to_owned();
        if pm2_version.is_none() {
            pm2_version = m
                .get("_pm2_version")
                .and_then(Value::as_str)
                .map(str::to_owned);
        }
        let key = (ns, name.to_owned());
        match groups.iter_mut().find(|(k, _)| *k == key) {
            Some((_, list)) => list.push(m.clone()),
            None => groups.push((key, vec![m.clone()])),
        }
    }

    let mut apps: Vec<DumpApp> = groups
        .into_iter()
        .map(|((ns, name), list)| rebuild(&name, &ns, &list, user))
        .collect();

    // Vexor has no namespaces: one name, one app.
    let mut by_name: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for a in &apps {
        by_name
            .entry(a.name.clone())
            .or_default()
            .push(a.namespace.clone());
    }
    for a in &mut apps {
        let ns = &by_name[&a.name];
        if ns.len() > 1 {
            a.notes.push(Note {
                app: a.name.clone(),
                field: "namespace".into(),
                severity: Severity::Fatal,
                message: format!(
                    "apps in namespaces {} share this name; Vexor app names must be unique",
                    ns.join(", ")
                ),
            });
        }
    }

    Ok(Dump {
        apps,
        pm2_version,
        processes: entries.len(),
    })
}

/// Rebuild one app from its instances' entries.
fn rebuild(name: &str, ns: &str, list: &[Map<String, Value>], user: Option<&str>) -> DumpApp {
    let first = &list[0];
    let mut notes = Vec::new();
    let mut note = |field: &str, severity, message: String| {
        notes.push(Note {
            app: name.to_owned(),
            field: field.to_owned(),
            severity,
            message,
        });
    };
    let instance_var = first
        .get("instance_var")
        .and_then(Value::as_str)
        .unwrap_or("NODE_APP_INSTANCE")
        .to_owned();
    let env: Map<String, Value> = first
        .get("env")
        .and_then(Value::as_object)
        .map(|e| {
            e.iter()
                .filter(|(k, _)| !is_pm2_env(k, name, &instance_var))
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect()
        })
        .unwrap_or_default();

    let mut app = Map::new();
    app.insert("name".into(), Value::String(name.to_owned()));
    for (k, v) in first {
        // PM2 also copies every environment variable to the top level.
        let env_copy = first
            .get("env")
            .and_then(|e| e.get(k))
            .is_some_and(|e| e == v);
        if INTERNAL.contains(&k.as_str()) || env_copy || k == &instance_var || is_pm2_default(k, v)
        {
            continue;
        }
        app.insert(k.clone(), v.clone());
    }
    let s = |k: &str| first.get(k).and_then(Value::as_str);
    match s("pm_exec_path") {
        Some(p) => {
            app.insert("script".into(), Value::String(p.to_owned()));
        }
        None => note(
            "script",
            Severity::Fatal,
            "the dump has no script path (pm_exec_path)".into(),
        ),
    }
    if let Some(d) = s("pm_cwd").or_else(|| s("cwd")) {
        app.insert("cwd".into(), Value::String(d.to_owned()));
    }
    // Keep PM2's log files, so log shippers and habits keep working. With
    // several instances PM2 writes one file per instance (`api-out-0.log`,
    // `api-out-1.log`, suffixed with its process id); Vexor writes all
    // instances to one file, named as PM2 names the set (`api-out.log`).
    for (field, key) in [
        ("out_file", "pm_out_log_path"),
        ("error_file", "pm_err_log_path"),
        ("log_file", "pm_log_path"),
    ] {
        let Some(p) = s(key) else { continue };
        let per_instance = list
            .iter()
            .any(|m| m.get(key).and_then(Value::as_str).is_some_and(|q| q != p));
        let path = if per_instance {
            let base = strip_instance_suffix(p);
            note(
                field,
                Severity::Partial,
                format!(
                    "PM2 wrote one file per instance ({p}, …); Vexor writes all instances to {base}"
                ),
            );
            base
        } else {
            p.to_owned()
        };
        app.insert(field.into(), Value::String(path));
    }
    app.insert("instances".into(), Value::from(list.len()));
    app.insert("env".into(), Value::Object(env));

    if let Some(u) = s("username")
        && user.is_some_and(|me| me != u)
    {
        note(
            "username",
            Severity::Fatal,
            format!(
                "PM2 ran this app as {u:?}; Vexor would run it as {:?} (run the migration as {u:?})",
                user.unwrap_or("?")
            ),
        );
    }
    if s("exec_interpreter") == Some("node") {
        note(
            "interpreter",
            Severity::Partial,
            "PM2 runs Node.js apps with the Node.js that runs PM2 itself; Vexor uses the `node` on the app's saved PATH".into(),
        );
    }

    DumpApp {
        name: name.to_owned(),
        namespace: ns.to_owned(),
        statuses: list
            .iter()
            .map(|m| {
                m.get("status")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown")
                    .to_owned()
            })
            .collect(),
        node_version: s("node_version").map(str::to_owned),
        app: Value::Object(app),
        notes,
    }
}

/// `/logs/api-out-3.log` → `/logs/api-out.log`.
fn strip_instance_suffix(p: &str) -> String {
    let (dir, file) = p.rsplit_once('/').map_or(("", p), |(d, f)| (d, f));
    let (stem, ext) = file
        .rsplit_once('.')
        .map_or((file, None), |(s, e)| (s, Some(e)));
    let stem = match stem.rsplit_once('-') {
        Some((base, n)) if !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()) => base,
        _ => stem,
    };
    let file = ext.map_or_else(|| stem.to_owned(), |e| format!("{stem}.{e}"));
    if p.contains('/') {
        format!("{dir}/{file}")
    } else {
        file
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A trimmed `dump.pm2` written by PM2 7.1.0 (`pm2 start` of an
    /// ecosystem file plus one CLI app, one app stopped, then `pm2 save`).
    const DUMP: &str = include_str!("../tests/fixtures/dump-7.1.0.json");

    fn parsed() -> Dump {
        parse(DUMP, Some("root")).expect("parses")
    }

    fn app<'a>(d: &'a Dump, name: &str) -> &'a DumpApp {
        d.apps.iter().find(|a| a.name == name).expect(name)
    }

    #[test]
    fn groups_instances_into_apps() {
        let d = parsed();
        assert_eq!(d.pm2_version.as_deref(), Some("7.1.0"));
        assert_eq!(d.processes, 6);
        let names: Vec<_> = d.apps.iter().map(|a| a.name.as_str()).collect();
        assert_eq!(names, ["api", "worker", "stopped-one", "py", "cli-app"]);
        let api = app(&d, "api");
        assert_eq!(api.app["instances"], 2);
        assert_eq!(api.app["exec_mode"], "cluster_mode");
        assert!(api.running());
        assert!(!app(&d, "stopped-one").running());
    }

    #[test]
    fn rebuilds_paths_args_and_logs() {
        let d = parsed();
        let w = &app(&d, "worker").app;
        assert_eq!(w["script"], "/srv/worker/w.js");
        assert_eq!(w["cwd"], "/srv/worker");
        assert_eq!(w["args"], serde_json::json!(["--queue", "high prio"]));
        assert_eq!(
            w["node_args"],
            serde_json::json!(["--max-old-space-size=256"])
        );
        assert_eq!(w["out_file"], "/root/.pm2/logs/worker-out.log");
        assert_eq!(w["max_restarts"], 5);
        let api = &app(&d, "api").app;
        assert_eq!(api["out_file"], "/srv/logs/api.out");
        assert_eq!(app(&d, "py").app["exec_interpreter"], "none");
    }

    #[test]
    fn drops_pm2_bookkeeping_and_defaults() {
        let d = parsed();
        let api = app(&d, "api").app.as_object().expect("object");
        for k in [
            "status",
            "pm_uptime",
            "unique_id",
            "axm_options",
            "pmx",
            "automation",
            "windowsHide",
            "kill_retry_time",
            "namespace",
            "username",
            "NODE_APP_INSTANCE",
            "PORT",
            "HOSTNAME",
        ] {
            assert!(!api.contains_key(k), "{k} kept");
        }
        let env = api["env"].as_object().expect("env");
        assert_eq!(env["PORT"], 3000);
        assert_eq!(env["SECRET_TOKEN"], "hunter2");
        assert!(
            env.contains_key("SHELL_SECRET"),
            "shell env is what PM2 runs with"
        );
        for k in ["PM2_HOME", "PM2_USAGE", "api", "unique_id", "PWD", "SHLVL"] {
            assert!(!env.contains_key(k), "{k} kept in env");
        }
    }

    #[test]
    fn unsupported_settings_survive_for_the_translator() {
        let d = parsed();
        assert_eq!(app(&d, "api").app["max_memory_restart"], 209_715_200);
        assert_eq!(app(&d, "worker").app["cron_restart"], "0 3 * * *");
    }

    #[test]
    fn another_user_is_fatal() {
        let d = parse(DUMP, Some("deploy")).expect("parses");
        assert!(
            app(&d, "api")
                .notes
                .iter()
                .any(|n| n.field == "username" && n.severity == Severity::Fatal)
        );
        assert!(!parsed().apps[0].notes.iter().any(|n| n.field == "username"));
    }

    #[test]
    fn same_name_in_two_namespaces_is_fatal() {
        let text = r#"[
          {"name":"a","namespace":"one","pm_exec_path":"/x.js","status":"online"},
          {"name":"a","namespace":"two","pm_exec_path":"/y.js","status":"online"}
        ]"#;
        let d = parse(text, None).expect("parses");
        assert_eq!(d.apps.len(), 2);
        assert!(d.apps.iter().all(|a| {
            a.notes
                .iter()
                .any(|n| n.field == "namespace" && n.severity == Severity::Fatal)
        }));
    }

    #[test]
    fn per_instance_log_files_become_one() {
        assert_eq!(strip_instance_suffix("/l/api-out-12.log"), "/l/api-out.log");
        assert_eq!(strip_instance_suffix("/l/api-out.log"), "/l/api-out.log");
        assert_eq!(strip_instance_suffix("api-err-0"), "api-err");
        let text = r#"[
          {"name":"api","pm_exec_path":"/a.js","pm_out_log_path":"/l/api-out-0.log","status":"online"},
          {"name":"api","pm_exec_path":"/a.js","pm_out_log_path":"/l/api-out-1.log","status":"online"}
        ]"#;
        let d = parse(text, None).expect("parses");
        assert_eq!(d.apps[0].app["out_file"], "/l/api-out.log");
        assert!(d.apps[0].notes.iter().any(|n| n.field == "out_file"));
        // Explicit out_file shared by all instances: kept as is.
        assert_eq!(parsed().apps[0].app["out_file"], "/srv/logs/api.out");
    }

    #[test]
    fn malformed_dumps_are_errors() {
        assert!(parse("{}", None).is_err());
        assert!(parse("[1]", None).is_err());
        assert!(parse(r#"[{"status":"online"}]"#, None).is_err());
        assert!(parse("not json", None).is_err());
        let d = parse("[]", None).expect("empty dump");
        assert!(d.apps.is_empty());
    }

    #[test]
    fn rebuilt_apps_translate() {
        let d = parsed();
        let ctx = crate::Context {
            config_dir: "/".into(),
            env_name: None,
            caller_env: BTreeMap::new(),
            inherit_env: false,
            log_dir: "/tmp".into(),
        };
        let t = crate::translate(&app(&d, "py").app, &ctx).expect("translates");
        assert_eq!(t.spec.exec, std::path::Path::new("/bin/sleep"));
        assert_eq!(t.spec.args, ["1000"]);
        let worker = crate::translate(&app(&d, "worker").app, &ctx).expect("translates");
        assert_eq!(worker.spec.cron_restart.as_deref(), Some("0 3 * * *"));
    }
}
