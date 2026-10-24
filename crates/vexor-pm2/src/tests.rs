use super::*;
use serde_json::json;

fn ctx(dir: &Path) -> Context {
    Context {
        config_dir: dir.to_path_buf(),
        env_name: None,
        caller_env: BTreeMap::from([
            ("PATH".to_owned(), "/usr/bin:/bin".to_owned()),
            ("SECRET".to_owned(), "x".to_owned()),
        ]),
        inherit_env: false,
        log_dir: PathBuf::from("/logs"),
    }
}

fn fatal(notes: &[Note]) -> Vec<&str> {
    notes
        .iter()
        .filter(|n| n.severity == Severity::Fatal)
        .map(|n| n.field.as_str())
        .collect()
}

fn tr(app: Value) -> Translated {
    let dir = std::env::temp_dir();
    translate(&app, &ctx(&dir)).expect("translates")
}

#[test]
fn basic_fork_app() {
    let dir = tempfile::tempdir().expect("tempdir");
    let t = translate(
        &json!({
            "name": "api",
            "script": "/bin/sleep",
            "args": "10 'two words'",
            "cwd": "sub",
            "env": { "PORT": 3000, "DEBUG": false, "OBJ": { "a": 1 } },
            "max_restarts": 5,
            "min_uptime": "2s",
            "restart_delay": 250,
            "kill_timeout": 5000,
            "stop_exit_codes": [0, 2],
            "autorestart": true,
        }),
        &ctx(dir.path()),
    )
    .expect("ok");
    let s = &t.spec;
    assert_eq!(s.name, "api");
    // A script path is kept as written, not resolved through symlinks:
    // deploys often point at a `current -> releases/N` symlink.
    assert_eq!(s.exec, PathBuf::from("/bin/sleep"));
    assert_eq!(s.args, ["10", "two words"]);
    assert_eq!(s.cwd, dir.path().join("sub"));
    assert_eq!(s.env["PORT"], "3000");
    assert_eq!(s.env["DEBUG"], "false");
    assert_eq!(s.env["OBJ"], r#"{"a":1}"#);
    assert!(!s.env.contains_key("SECRET"), "caller env is allowlisted");
    assert_eq!(s.env["PATH"], "/usr/bin:/bin");
    assert_eq!(s.policy.max_restarts, 5);
    assert_eq!(s.policy.min_uptime_ms, 2000);
    assert_eq!(s.policy.restart_delay_ms, 250);
    assert_eq!(s.policy.kill_timeout_ms, 5000);
    assert_eq!(s.policy.stop_exit_codes, [0, 2]);
    assert!(!s.node_ipc);
    assert_eq!(s.logs.out, PathBuf::from("/logs/api-out.log"));
    assert!(t.notes.is_empty(), "{:?}", t.notes);
}

#[test]
fn js_script_uses_node_with_node_args_and_wait_ready() {
    let mut c = ctx(Path::new("/srv"));
    c.caller_env
        .insert("PATH".into(), std::env::var("PATH").unwrap_or_default());
    if resolve_exec("node", &c.caller_env).is_err() {
        eprintln!("skipping: node not found");
        return;
    }
    let t = translate(
        &json!({
            "name": "web", "script": "server.js",
            "node_args": "--max-old-space-size=512",
            "args": ["--port", 8080],
            "wait_ready": true,
        }),
        &c,
    )
    .expect("ok");
    let s = &t.spec;
    assert!(
        s.exec
            .file_name()
            .is_some_and(|f| f.to_string_lossy().starts_with("node"))
    );
    assert_eq!(
        s.args,
        [
            "--max-old-space-size=512",
            "/srv/server.js",
            "--port",
            "8080"
        ]
    );
    assert!(s.node_ipc);
    assert_eq!(s.policy.ready_timeout_ms, Some(3000));
    assert!(fatal(&t.notes).is_empty(), "{:?}", t.notes);
}

#[test]
fn interpreter_none_runs_script_directly_and_bare_commands_use_path() {
    let t = tr(json!({ "name": "a", "script": "sleep", "args": "5", "interpreter": "none" }));
    assert_eq!(
        t.spec.exec,
        PathBuf::from("/bin/sleep").canonicalize().expect("sleep")
    );
    assert_eq!(t.spec.args, ["5"]);
}

#[test]
fn behaviour_changing_fields_are_fatal() {
    let t = tr(json!({
        "name": "a", "script": "/bin/sleep",
        "uid": "www",
        "exec_mode": "cluster",
        "shutdown_with_message": true,
    }));
    let mut f = fatal(&t.notes);
    f.sort_unstable();
    assert_eq!(f, ["exec_mode", "shutdown_with_message", "uid",]);
}

#[test]
fn increment_var_is_translated() {
    let t = tr(json!({
        "name": "a", "script": "/bin/sleep", "instances": 3,
        "env": { "PORT": "3000" }, "increment_var": "PORT",
    }));
    assert!(fatal(&t.notes).is_empty(), "{:?}", t.notes);
    assert_eq!(t.spec.increment_var.as_deref(), Some("PORT"));
    assert_eq!(t.spec.incremented(2), Some(("PORT", "3002".to_owned())));
}

#[test]
fn false_or_null_behaviour_fields_are_fine() {
    let t = tr(json!({
        "name": "a", "script": "/bin/sleep",
        "watch": false, "shutdown_with_message": false, "max_memory_restart": null,
    }));
    assert!(fatal(&t.notes).is_empty(), "{:?}", t.notes);
}

#[test]
fn ignored_and_unknown_fields_are_reported() {
    let t = tr(json!({
        "name": "a", "script": "/bin/sleep",
        "vizion": false, "namespace": "x", "log_date_format": "YYYY", "frobnicate": 1,
    }));
    let fields: Vec<_> = t
        .notes
        .iter()
        .map(|n| (n.field.as_str(), n.severity))
        .collect();
    for f in ["vizion", "namespace", "log_date_format", "frobnicate"] {
        assert!(
            fields.contains(&(f, Severity::Ignored)),
            "{f} not reported: {fields:?}"
        );
    }
}

#[test]
fn env_name_layers_env_and_can_rename() {
    let mut c = ctx(Path::new("/srv"));
    c.env_name = Some("production".into());
    let t = translate(
        &json!({
            "name": "a", "script": "/bin/sleep",
            "env": { "NODE_ENV": "development", "A": "1" },
            "env_production": { "NODE_ENV": "production" },
            "append_env_to_name": true,
        }),
        &c,
    )
    .expect("ok");
    assert_eq!(t.spec.env["NODE_ENV"], "production");
    assert_eq!(t.spec.env["A"], "1");
    assert_eq!(t.spec.name, "a-production");
}

#[test]
fn missing_env_section_is_noted() {
    let mut c = ctx(Path::new("/srv"));
    c.env_name = Some("staging".into());
    let t = translate(&json!({ "name": "a", "script": "/bin/sleep" }), &c).expect("ok");
    assert!(t.notes.iter().any(|n| n.field == "env_staging"));
}

#[test]
fn inherit_env_passes_everything() {
    let mut c = ctx(Path::new("/srv"));
    c.inherit_env = true;
    let t = translate(&json!({ "name": "a", "script": "/bin/sleep" }), &c).expect("ok");
    assert_eq!(t.spec.env["SECRET"], "x");
}

#[test]
fn instances_semantics() {
    let cpus =
        std::thread::available_parallelism().map_or(1, |n| u32::try_from(n.get()).expect("fits"));
    for (v, want) in [
        (json!(3), 3),
        (json!("4"), 4),
        (json!("max"), cpus),
        (json!(0), cpus),
        (json!(-1), cpus.saturating_sub(1).max(1)),
        (json!(-1000), 1),
    ] {
        let t =
            tr(json!({ "name": "a", "script": "/bin/sleep", "instances": v, "merge_logs": true }));
        assert_eq!(t.spec.instances, want, "{v}");
    }
    let t = tr(json!({ "name": "a", "script": "/bin/sleep", "instances": 2 }));
    assert!(t.notes.iter().any(|n| n.field == "merge_logs"));
}

#[test]
fn logs_paths_and_disabling() {
    let t = tr(json!({
        "name": "a", "script": "/bin/sleep", "cwd": "/srv",
        "out_file": "logs/out.log", "error_file": "/var/log/a.err",
    }));
    assert_eq!(t.spec.logs.out, PathBuf::from("/srv/logs/out.log"));
    assert_eq!(t.spec.logs.err, PathBuf::from("/var/log/a.err"));
    let t = tr(json!({ "name": "a", "script": "/bin/sleep", "disable_logs": true }));
    assert_eq!(t.spec.logs.out, PathBuf::from("/dev/null"));
    assert_eq!(t.spec.logs.max_size_bytes, 0, "never rotate /dev/null");
    let t = tr(json!({ "name": "a", "script": "/bin/sleep", "out_file": "/dev/null" }));
    assert_eq!(t.spec.logs.max_size_bytes, 0);
}

#[test]
fn autostart_false_is_kept() {
    let t = tr(json!({ "name": "a", "script": "/bin/sleep", "autostart": false }));
    assert!(!t.autostart);
}

#[test]
fn ts_without_interpreter_is_fatal() {
    let t = tr(json!({ "name": "a", "script": "app.ts" }));
    assert_eq!(fatal(&t.notes), ["interpreter"]);
}

#[test]
fn function_values_are_fatal() {
    let t = tr(
        json!({ "name": "a", "script": "/bin/sleep", "env": { "__vexor_unsupported__": "function" } }),
    );
    assert!(fatal(&t.notes).contains(&"env"));
}

#[test]
fn split_args_cases() {
    assert_eq!(split_args("a b  c").expect("ok"), ["a", "b", "c"]);
    assert_eq!(
        split_args(r#"--x="a b" 'c d' e\ f"#).expect("ok"),
        ["--x=a b", "c d", "e f"]
    );
    assert_eq!(
        split_args(r#"'$HOME' "`id`""#).expect("ok"),
        ["$HOME", "`id`"]
    );
    assert_eq!(split_args("''").expect("ok"), [""]);
    assert!(split_args("'open").is_err());
    assert!(split_args("").expect("ok").is_empty());
}

#[test]
fn paths_are_normalized() {
    assert_eq!(
        normalize(Path::new("/srv/./app/../x.js")),
        PathBuf::from("/srv/x.js")
    );
    let t = tr(
        json!({ "name": "a", "script": "./w.sh", "cwd": "/srv/./sub/..", "interpreter": "/bin/sh" }),
    );
    assert_eq!(t.spec.args, ["/srv/w.sh"]);
    assert_eq!(t.spec.cwd, PathBuf::from("/srv"));
}

#[test]
fn cluster_mode_translates_to_a_cluster_spec() {
    let mut c = ctx(Path::new("/srv"));
    c.caller_env
        .insert("PATH".into(), std::env::var("PATH").unwrap_or_default());
    if resolve_exec("node", &c.caller_env).is_err() {
        eprintln!("skipping: node not found");
        return;
    }
    let t = translate(
        &json!({
            "name": "web", "script": "app.js", "exec_mode": "cluster", "instances": 4,
            "node_args": "--max-old-space-size=256", "args": "--port 80",
        }),
        &c,
    )
    .expect("ok");
    let cluster = t.spec.cluster.expect("cluster spec");
    assert_eq!(cluster.script, PathBuf::from("/srv/app.js"));
    assert_eq!(cluster.node_args, ["--max-old-space-size=256"]);
    assert_eq!(cluster.args, ["--port", "80"]);
    assert_eq!(t.spec.instances, 4);
    assert!(t.spec.node_ipc);
    assert!(fatal(&t.notes).is_empty(), "{:?}", t.notes);
}

#[test]
fn max_restarts_zero_means_pm2_default() {
    let t = tr(json!({ "name": "a", "script": "/bin/sleep", "max_restarts": 0 }));
    assert_eq!(t.spec.policy.max_restarts, 16);
    assert!(t.notes.iter().any(|n| n.field == "max_restarts"));
}

#[test]
fn kill_signal_extension() {
    let t = tr(json!({ "name": "a", "script": "/bin/sleep", "kill_signal": "SIGTERM" }));
    assert_eq!(t.spec.policy.kill_signal, vexor_core::KillSignal::Term);
    assert!(fatal(&t.notes).is_empty());
    let t = tr(json!({ "name": "a", "script": "/bin/sleep" }));
    assert_eq!(
        t.spec.policy.kill_signal,
        vexor_core::KillSignal::Int,
        "PM2 default"
    );
    let t = tr(json!({ "name": "a", "script": "/bin/sleep", "kill_signal": "SIGWHAT" }));
    assert_eq!(fatal(&t.notes), ["kill_signal"]);
}

#[test]
fn durations() {
    assert_eq!(parse_duration_ms("500"), Some(500));
    assert_eq!(parse_duration_ms("500ms"), Some(500));
    assert_eq!(parse_duration_ms("1.5s"), Some(1500));
    assert_eq!(parse_duration_ms("2m"), Some(120_000));
    assert_eq!(parse_duration_ms("1h"), Some(3_600_000));
    assert_eq!(parse_duration_ms("3 days"), None);
    assert_eq!(parse_duration_ms("-1s"), None);
}

#[test]
fn app_list_shapes() {
    assert_eq!(
        apps(&json!({ "apps": [{ "script": "a" }] }))
            .expect("ok")
            .len(),
        1
    );
    assert_eq!(
        apps(&json!({ "apps": { "script": "a" } }))
            .expect("ok")
            .len(),
        1
    );
    assert_eq!(
        apps(&json!([{ "script": "a" }, { "script": "b" }]))
            .expect("ok")
            .len(),
        2
    );
    assert_eq!(apps(&json!({ "script": "a" })).expect("ok").len(), 1);
    assert!(apps(&json!({ "nope": 1 })).is_err());
}

#[test]
fn only_and_duplicates() {
    let cfg = json!({ "apps": [
        { "name": "a", "script": "/bin/sleep" },
        { "name": "b", "script": "/bin/sleep" },
        { "name": "b", "script": "/bin/sleep" },
    ]});
    let c = ctx(Path::new("/srv"));
    let (apps, _) = translate_all(&cfg, &c, &["a".to_owned()]).expect("ok");
    assert_eq!(apps.len(), 1);
    let (_, notes) = translate_all(&cfg, &c, &[]).expect("ok");
    assert!(
        notes
            .iter()
            .any(|n| n.field == "name" && n.severity == Severity::Fatal)
    );
    assert!(translate_all(&cfg, &c, &["zzz".to_owned()]).is_err());
}

// ------------------------------------------------------------------ loading

fn node() -> Option<PathBuf> {
    let env = BTreeMap::from([("PATH".to_owned(), std::env::var("PATH").unwrap_or_default())]);
    resolve_exec("node", &env).ok()
}

#[test]
fn loads_js_cjs_mjs_and_json() {
    let Some(node) = node() else {
        eprintln!("skipping: node not found");
        return;
    };
    let dir = tempfile::tempdir().expect("tempdir");
    let write = |name: &str, body: &str| {
        let p = dir.path().join(name);
        std::fs::write(&p, body).expect("write");
        p
    };
    let js = write(
        "ecosystem.config.js",
        "var base = { script: 'app.js' };\nmodule.exports = { apps: [Object.assign({ name: 'a', cwd: process.cwd() }, base)] };\n",
    );
    let cfg = load(&js, Some(&node)).expect("js");
    assert_eq!(cfg["apps"][0]["name"], "a");
    assert_eq!(
        Path::new(cfg["apps"][0]["cwd"].as_str().expect("cwd"))
            .canonicalize()
            .expect("c"),
        dir.path().canonicalize().expect("d"),
        "loader runs in the config's directory"
    );
    let cjs = write(
        "eco.cjs",
        "module.exports = [{ name: 'c', script: 'x' }];\n",
    );
    assert_eq!(load(&cjs, Some(&node)).expect("cjs")[0]["name"], "c");
    let mjs = write(
        "eco.mjs",
        "export default { apps: [{ name: 'm', script: 'x' }] };\n",
    );
    assert_eq!(
        load(&mjs, Some(&node)).expect("mjs")["apps"][0]["name"],
        "m"
    );
    let json = write("eco.json", r#"{"apps":[{"name":"j","script":"x"}]}"#);
    assert_eq!(load(&json, None).expect("json")["apps"][0]["name"], "j");
    let funcs = write(
        "f.config.js",
        "module.exports = { apps: [{ name: 'f', script: 'x', env: function () {} }] };\n",
    );
    let cfg = load(&funcs, Some(&node)).expect("functions");
    assert_eq!(cfg["apps"][0]["env"]["__vexor_unsupported__"], "function");
}

#[test]
fn load_errors_are_explained() {
    let Some(node) = node() else {
        eprintln!("skipping: node not found");
        return;
    };
    let dir = tempfile::tempdir().expect("tempdir");
    let bad = dir.path().join("bad.config.js");
    std::fs::write(&bad, "module.exports = {;\n").expect("write");
    let e = load(&bad, Some(&node)).expect_err("syntax error");
    assert!(e.contains("SyntaxError"), "{e}");
    let throws = dir.path().join("throws.config.js");
    std::fs::write(&throws, "throw new Error('boom');\n").expect("write");
    let e = load(&throws, Some(&node)).expect_err("throws");
    assert!(e.contains("boom"), "{e}");
    let js_no_node = load(&throws, None).expect_err("no node");
    assert!(js_no_node.contains("needs Node.js"), "{js_no_node}");
    let yaml = dir.path().join("p.yml");
    std::fs::write(&yaml, "apps: []").expect("write");
    assert!(load(&yaml, None).expect_err("yaml").contains("YAML"));
}

#[test]
fn max_memory_restart_sizes() {
    assert_eq!(parse_memory(&json!(1024)), Some(1024));
    assert_eq!(parse_memory(&json!("1024")), Some(1024));
    assert_eq!(parse_memory(&json!("200M")), Some(200 << 20));
    assert_eq!(parse_memory(&json!("1G")), Some(1 << 30));
    assert_eq!(parse_memory(&json!("512K")), Some(512 << 10));
    assert_eq!(parse_memory(&json!("150m")), Some(150 << 20));
    assert_eq!(parse_memory(&json!("1.5G")), None);
    assert_eq!(parse_memory(&json!("200MB")), None);
    assert_eq!(parse_memory(&json!("lots")), None);
    assert_eq!(parse_memory(&json!(true)), None);
}

#[test]
fn max_memory_restart_translates() {
    let dir = tempfile::tempdir().expect("dir");
    let t = translate(
        &serde_json::json!({"name": "a", "script": "/bin/sh", "max_memory_restart": "200M"}),
        &ctx(dir.path()),
    )
    .expect("translates");
    assert_eq!(t.spec.policy.max_memory_bytes, Some(200 << 20));
    assert!(
        t.notes.iter().all(|n| n.field != "max_memory_restart"),
        "{:?}",
        t.notes
    );
    let bad = translate(
        &serde_json::json!({"name": "a", "script": "/bin/sh", "max_memory_restart": "200MB"}),
        &ctx(dir.path()),
    )
    .expect("translates");
    assert!(
        bad.notes
            .iter()
            .any(|n| n.field == "max_memory_restart" && n.severity == Severity::Fatal)
    );
}

#[test]
fn cron_restart_translates_and_is_validated() {
    let dir = tempfile::tempdir().expect("dir");
    let ok = |p: &str| {
        translate(
            &json!({"name": "a", "script": "/bin/sh", "cron_restart": p}),
            &ctx(dir.path()),
        )
        .expect("translates")
    };
    let t = ok("0 3 * * *");
    assert_eq!(t.spec.cron_restart.as_deref(), Some("0 3 * * *"));
    assert!(fatal(&t.notes).is_empty(), "{:?}", t.notes);
    assert_eq!(
        ok("*/10 * * * * *").spec.cron_restart.as_deref(),
        Some("*/10 * * * * *")
    );
    for bad in ["61 * * * *", "nightly", ""] {
        let t = ok(bad);
        assert!(t.spec.cron_restart.is_none());
        assert_eq!(fatal(&t.notes), ["cron_restart"], "{bad}");
    }
    let t = translate(
        &json!({"name": "a", "script": "/bin/sh", "cron_restart": 5}),
        &ctx(dir.path()),
    )
    .expect("translates");
    assert_eq!(fatal(&t.notes), ["cron_restart"]);
}

#[test]
fn watch_settings_translate() {
    let dir = tempfile::tempdir().expect("dir");
    std::fs::create_dir(dir.path().join("src")).expect("mkdir");
    let tr = |v: serde_json::Value| translate(&v, &ctx(dir.path())).expect("translates");
    let base = |extra: serde_json::Value| {
        let mut v = json!({"name": "a", "script": "/bin/sh"});
        if let (Some(b), Some(e)) = (v.as_object_mut(), extra.as_object()) {
            b.extend(e.clone());
        }
        v
    };
    assert!(tr(base(json!({}))).spec.watch.is_none());
    assert!(tr(base(json!({"watch": false}))).spec.watch.is_none());

    let w = tr(base(json!({"watch": true}))).spec.watch.expect("watch");
    assert_eq!(w.paths, [dir.path()]);
    assert_eq!(w.ignore, [".*", "node_modules"]);
    assert_eq!(w.delay_ms, 0);

    let t = tr(base(json!({
        "watch": ["src", "missing"],
        "ignore_watch": ["*.log", "tmp/"],
        "watch_delay": 1500,
        "watch_options": {"usePolling": true}
    })));
    let w = t.spec.watch.expect("watch");
    assert_eq!(
        w.paths,
        [dir.path().join("src"), dir.path().join("missing")]
    );
    assert_eq!(w.ignore, ["*.log", "tmp/"]);
    assert_eq!(w.delay_ms, 1500);
    assert!(fatal(&t.notes).is_empty(), "{:?}", t.notes);
    assert!(
        t.notes
            .iter()
            .any(|n| n.field == "watch" && n.message.contains("missing"))
    );
    assert!(
        t.notes
            .iter()
            .any(|n| n.field == "watch_options" && n.severity == Severity::Ignored)
    );

    let t = tr(base(
        json!({"watch": "src", "ignore_watch": "[\\/\\\\]\\./"}),
    ));
    assert!(
        t.notes
            .iter()
            .any(|n| n.field == "ignore_watch" && n.severity == Severity::Partial),
        "{:?}",
        t.notes
    );
    let t = tr(base(json!({"watch": true, "ignore_watch": ["a["]})));
    assert_eq!(fatal(&t.notes), ["ignore_watch"]);
    let t = tr(base(json!({"watch": 5})));
    assert_eq!(fatal(&t.notes), ["watch"]);
}

#[test]
fn health_check_translates() {
    let dir = tempfile::tempdir().expect("dir");
    let tr = |v: serde_json::Value| translate(&v, &ctx(dir.path())).expect("translates");
    let t = tr(json!({
        "name": "a", "script": "/bin/sh",
        "health_check": { "http": "http://127.0.0.1:30{instance}0/health", "interval": "5s", "restart_after": 4 }
    }));
    assert!(fatal(&t.notes).is_empty(), "{:?}", t.notes);
    let hc = t.spec.health_check.expect("health");
    assert_eq!(
        hc.http.as_deref(),
        Some("http://127.0.0.1:30{instance}0/health")
    );
    assert_eq!((hc.interval_ms, hc.timeout_ms), (5000, 2000));
    assert_eq!(t.spec.policy.health_unhealthy_after, Some(3));
    assert_eq!(t.spec.policy.health_restart_after, Some(4));
    // Ready when the check first passes.
    assert_eq!(t.spec.policy.ready_timeout_ms, Some(30_000));

    let t = tr(
        json!({"name": "a", "script": "/bin/sh", "listen_timeout": 8000, "health_check": { "tcp": "127.0.0.1:6000" }}),
    );
    assert_eq!(t.spec.policy.ready_timeout_ms, Some(8000));
    assert!(
        t.notes.iter().all(|n| n.field != "listen_timeout"),
        "{:?}",
        t.notes
    );

    for bad in [
        json!({ "http": "http://10.1.2.3:80/" }),
        json!({ "http": "https://127.0.0.1:443/" }),
        json!({ "tcp": "127.0.0.1" }),
        json!({ "http": "http://127.0.0.1:1/", "tcp": "127.0.0.1:1" }),
        json!({ "http": "http://127.0.0.1:1/", "retries": 3 }),
        json!("http://127.0.0.1:1/"),
    ] {
        let t = tr(json!({"name": "a", "script": "/bin/sh", "health_check": bad}));
        assert_eq!(fatal(&t.notes), ["health_check"], "{bad}");
        assert!(t.spec.health_check.is_none());
    }
}

#[test]
fn time_setting_controls_log_timestamps() {
    let dir = tempfile::tempdir().expect("dir");
    let tr = |v: serde_json::Value| translate(&v, &ctx(dir.path())).expect("translates");
    assert!(
        tr(json!({"name": "a", "script": "/bin/sh"}))
            .spec
            .logs
            .timestamps
    );
    assert!(
        tr(json!({"name": "a", "script": "/bin/sh", "time": true}))
            .spec
            .logs
            .timestamps
    );
    assert!(
        !tr(json!({"name": "a", "script": "/bin/sh", "time": false}))
            .spec
            .logs
            .timestamps
    );
    let t = tr(json!({"name": "a", "script": "/bin/sh", "log_date_format": "YYYY"}));
    assert!(
        t.notes
            .iter()
            .any(|n| n.field == "log_date_format" && n.severity == Severity::Ignored)
    );
}
