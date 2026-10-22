//! End-to-end tests of the `vexor` CLI against a real daemon.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use assert_cmd::Command as AssertCommand;
use serde_json::Value;
use tempfile::TempDir;

/// A Vexor home plus a working directory with test scripts. Dropping it
/// deletes every app and stops the daemon.
struct Env {
    home: TempDir,
    work: TempDir,
}

impl Env {
    fn new() -> Self {
        let env = Env {
            home: tempfile::tempdir().expect("home"),
            work: tempfile::tempdir().expect("work"),
        };
        env.script("loop.sh", "#!/bin/sh\nwhile :; do sleep 0.1; done\n");
        env
    }

    fn script(&self, name: &str, body: &str) -> PathBuf {
        let path = self.work.path().join(name);
        std::fs::write(&path, body).expect("write script");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        path
    }

    fn cmd(&self, args: &[&str]) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_vexor"));
        c.arg("--home")
            .arg(self.home.path())
            .args(args)
            .current_dir(self.work.path());
        c
    }

    fn run(&self, args: &[&str]) -> Output {
        self.cmd(args).output().expect("run vexor")
    }

    fn ok(&self, args: &[&str]) -> String {
        let out = self.run(args);
        assert!(
            out.status.success(),
            "vexor {args:?} failed ({:?}): {}",
            out.status.code(),
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    fn list(&self) -> Vec<Value> {
        serde_json::from_str(&self.ok(&["list", "--json"])).expect("list json")
    }

    fn app(&self, name: &str) -> Value {
        self.list()
            .into_iter()
            .find(|a| a["name"] == name)
            .unwrap_or_else(|| panic!("no app {name}"))
    }

    fn log(&self) -> Vec<Value> {
        std::fs::read_to_string(self.home.path().join("daemon.log"))
            .unwrap_or_default()
            .lines()
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect()
    }

    fn count(&self, kind: &str) -> usize {
        self.log().iter().filter(|e| e["type"] == kind).count()
    }

    fn daemon_pid(&self) -> Option<u64> {
        self.log()
            .iter()
            .rfind(|e| e["type"] == "daemon_started")
            .and_then(|e| e["pid"].as_u64())
    }

    fn state(&self) -> Value {
        let text = std::fs::read_to_string(self.home.path().join("state.json")).expect("state");
        serde_json::from_str(&text).expect("state json")
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        if self.home.path().join("daemon.sock").exists() {
            let _ = self.run(&["delete", "all"]);
        }
        if let Some(pid) = self.daemon_pid() {
            let _ = Command::new("kill").arg(pid.to_string()).status();
        }
    }
}

fn pids(app: &Value) -> Vec<u64> {
    app["snapshot"]
        .as_array()
        .map(|s| s.iter().filter_map(|i| i["pid"].as_u64()).collect())
        .unwrap_or_default()
}

fn states(app: &Value) -> Vec<String> {
    app["snapshot"]
        .as_array()
        .map(|s| {
            s.iter()
                .map(|i| i["state"].as_str().unwrap_or("?").to_owned())
                .collect()
        })
        .unwrap_or_default()
}

fn alive(pid: u64) -> bool {
    Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

fn wait_dead(pid: u64) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while alive(pid) {
        assert!(Instant::now() < deadline, "{pid} still alive");
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn which(program: &str) -> Option<PathBuf> {
    let out = Command::new("/bin/sh")
        .args(["-c", &format!("command -v {program}")])
        .output()
        .ok()?;
    let path = String::from_utf8(out.stdout).ok()?.trim().to_owned();
    path.starts_with('/').then(|| PathBuf::from(path))
}

// --------------------------------------------------------------------------

#[test]
fn version_prints_crate_version() {
    let out = AssertCommand::cargo_bin("vexor")
        .expect("binary")
        .arg("--version")
        .assert()
        .success();
    let stdout = String::from_utf8_lossy(&out.get_output().stdout).into_owned();
    assert_eq!(
        stdout.trim(),
        format!("vexor {}", env!("CARGO_PKG_VERSION"))
    );
}

#[test]
fn unknown_argument_is_a_usage_error() {
    AssertCommand::cargo_bin("vexor")
        .expect("binary")
        .arg("--no-such-flag")
        .assert()
        .code(2);
}

#[test]
fn full_lifecycle_and_stop_survives_daemon_kill() {
    let env = Env::new();
    let table = env.ok(&["start", "loop.sh", "-i", "2"]);
    assert!(table.contains("online"), "{table}");
    let app = env.app("loop");
    assert_eq!(states(&app), ["online", "online"]);
    let first = pids(&app);
    assert_eq!(first.len(), 2);

    // stop: processes gone, desired state persisted.
    env.ok(&["stop", "loop"]);
    for p in &first {
        wait_dead(*p);
    }
    assert_eq!(env.state()["apps"]["loop"]["desired"], "stopped");

    // A daemon killed after `stop` must not bring the app back.
    let launched = env.count("shim_launched");
    let daemon = env.daemon_pid().expect("daemon pid");
    let _ = Command::new("kill")
        .args(["-9", &daemon.to_string()])
        .status();
    wait_dead(daemon);
    let app = env.app("loop"); // auto-starts a new daemon
    assert_eq!(app["desired"], "stopped");
    assert!(app["shim_pid"].is_null());
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(
        env.count("shim_launched"),
        launched,
        "stopped app was relaunched"
    );

    // start by name, then restart: new processes, restart count 1.
    env.ok(&["start", "loop"]);
    let started = pids(&env.app("loop"));
    assert_eq!(started.len(), 2);
    env.ok(&["restart", "loop"]);
    let app = env.app("loop");
    assert_eq!(states(&app), ["online", "online"]);
    let restarted = pids(&app);
    assert!(
        restarted.iter().all(|p| !started.contains(p)),
        "{started:?} -> {restarted:?}"
    );
    for s in app["snapshot"].as_array().expect("snapshot") {
        assert_eq!(s["restarts"], 1);
    }

    // delete: gone everywhere.
    env.ok(&["delete", "loop"]);
    for p in &restarted {
        wait_dead(*p);
    }
    assert!(env.list().is_empty());
    assert_eq!(env.state()["apps"], serde_json::json!({}));
}

#[test]
fn commands_start_the_daemon_when_needed() {
    let env = Env::new();
    assert!(!env.home.path().join("daemon.sock").exists());
    assert!(env.list().is_empty());
    assert!(env.home.path().join("daemon.sock").exists());
    assert!(env.daemon_pid().is_some_and(alive));
}

#[test]
fn errors_have_messages_and_exit_codes() {
    let env = Env::new();
    let out = env.run(&["stop", "nope"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("no app named \"nope\""));

    let out = env.run(&["start", "missing.sh"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("no file by that name"));

    env.ok(&["start", "loop.sh"]);
    let out = env.run(&["start", "loop.sh", "-i", "3"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("different configuration"));

    let out = env.run(&["start", "--name", "x", "loop"]);
    assert_eq!(out.status.code(), Some(1));
}

#[test]
fn concurrent_identical_starts_launch_one_app() {
    let env = Env::new();
    env.ok(&["list"]); // daemon up
    let children: Vec<_> = (0..8)
        .map(|_| {
            env.cmd(&["start", "loop.sh", "--name", "c"])
                .stdout(Stdio::null())
                .stderr(Stdio::piped())
                .spawn()
                .expect("spawn")
        })
        .collect();
    for c in children {
        let out = c.wait_with_output().expect("wait");
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    assert_eq!(env.count("shim_launched"), 1);
    assert_eq!(pids(&env.app("c")).len(), 1);
}

#[test]
fn stop_all_and_start_all() {
    let env = Env::new();
    env.ok(&["start", "loop.sh", "--name", "a"]);
    env.ok(&["start", "loop.sh", "--name", "b"]);
    env.ok(&["stop", "all"]);
    assert!(env.list().iter().all(|a| a["desired"] == "stopped"));
    env.ok(&["start", "all"]);
    for a in env.list() {
        assert_eq!(states(&a), ["online"], "{a}");
    }
}

#[test]
fn environment_is_allowlisted_unless_inherited() {
    let env = Env::new();
    let run = |name: &str, extra: &[&str]| {
        let mut args = vec!["start", "loop.sh", "--name", name];
        args.extend_from_slice(extra);
        let out = env
            .cmd(&args)
            .env("SECRET_TOKEN", "hunter2")
            .output()
            .expect("run");
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        env.state()["apps"][name]["spec"]["env"].clone()
    };
    let plain = run("plain", &["--env", "PORT=8080"]);
    assert!(plain.get("SECRET_TOKEN").is_none(), "{plain}");
    assert_eq!(plain["PORT"], "8080");
    assert!(plain.get("PATH").is_some());
    let inherited = run("inherited", &["--inherit-env"]);
    assert_eq!(inherited["SECRET_TOKEN"], "hunter2");
}

#[test]
fn js_scripts_run_with_node_from_the_callers_path() {
    let Some(node) = which("node") else {
        eprintln!("skipping: node not found on PATH");
        return;
    };
    let env = Env::new();
    env.script("srv.js", "setInterval(function(){}, 1000)\n");
    env.ok(&["start", "srv.js"]);
    let spec = &env.state()["apps"]["srv"]["spec"];
    let exec = PathBuf::from(spec["exec"].as_str().expect("exec"));
    assert_eq!(exec, node.canonicalize().expect("node path"));
    assert_eq!(spec["node_ipc"], true);
    assert!(
        spec["args"][0]
            .as_str()
            .is_some_and(|a| a.ends_with("srv.js") && Path::new(a).is_absolute())
    );
    assert_eq!(states(&env.app("srv")), ["online"]);
}

#[test]
fn corrupt_state_stops_the_daemon_and_is_kept() {
    let env = Env::new();
    let state = env.home.path().join("state.json");
    std::fs::write(&state, b"{oops").expect("write");
    let out = env.run(&["daemon"]);
    assert_eq!(out.status.code(), Some(5));
    assert!(String::from_utf8_lossy(&out.stderr).contains("untouched"));
    assert_eq!(std::fs::read(&state).expect("read"), b"{oops");
}

// ------------------------------------------------------------ event history

fn now_ms() -> u64 {
    u64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_millis(),
    )
    .expect("fits")
}

fn kill_daemon(env: &Env) {
    let pid = env.daemon_pid().expect("daemon pid");
    let _ = Command::new("kill").args(["-9", &pid.to_string()]).status();
    wait_dead(pid);
}

fn events(env: &Env, app: &str) -> Vec<Value> {
    serde_json::from_str(&env.ok(&["events", app, "--json", "--limit", "1000"])).expect("json")
}

#[test]
fn crash_budget_and_history_survive_a_daemon_kill() {
    let env = Env::new();
    let script = env.script("crash.sh", "#!/bin/sh\necho run\nexit 1\n");
    let spec = env.work.path().join("crash.json");
    let json = serde_json::json!({
        "name": "crash",
        "exec": script,
        "cwd": env.work.path(),
        "env": { "PATH": "/usr/bin:/bin" },
        "logs": { "out": env.work.path().join("crash.out"), "err": env.work.path().join("crash.err") },
        // A long min_uptime keeps every run unstable even on a loaded
        // machine; this test is about the budget surviving a daemon kill,
        // not about the min_uptime boundary.
        "policy": { "max_restarts": 3, "min_uptime_ms": 10000 },
    });
    std::fs::write(&spec, json.to_string()).expect("spec");
    env.ok(&["start", "--spec", &spec.to_string_lossy()]);

    // Kill the daemon mid-loop; the shim keeps counting.
    std::thread::sleep(Duration::from_millis(1500));
    let killed_at = now_ms();
    kill_daemon(&env);
    std::thread::sleep(Duration::from_millis(1500));
    let back_at = now_ms();

    let deadline = Instant::now() + Duration::from_secs(20);
    let app = loop {
        let app = env.app("crash"); // restarts the daemon
        if states(&app) == ["errored"] {
            break app;
        }
        assert!(Instant::now() < deadline, "never errored: {app}");
        std::thread::sleep(Duration::from_millis(100));
    };
    assert_eq!(
        app["snapshot"][0]["restarts"],
        3,
        "{app}\nhistory: {:#?}",
        events(&env, "crash")
    );
    let runs = std::fs::read_to_string(env.work.path().join("crash.out")).expect("out");
    assert_eq!(runs.lines().count(), 4, "1 start + 3 restarts");

    // History: every shim event exactly once, in order, including the ones
    // that happened while no daemon was running.
    let history: Vec<Value> = events(&env, "crash")
        .into_iter()
        .filter(|e| e["type"] == "state")
        .collect();
    let starts = history.iter().filter(|e| e["state"] == "starting").count();
    assert_eq!(starts, 4, "{history:#?}");
    assert_eq!(
        history.last().map(|e| e["state"].clone()),
        Some("errored".into())
    );
    let during_outage = history
        .iter()
        .filter_map(|e| e["ts_ms"].as_u64())
        .filter(|t| (killed_at..back_at).contains(t))
        .count();
    assert!(during_outage > 0, "no events recorded from the outage");
    // All from one shim: sequence numbers of every stored shim event are
    // contiguous from 0, so nothing is missing or duplicated.
    let all = events(&env, "crash");
    let shim_events: Vec<&Value> = all.iter().filter(|e| e["seq"].is_u64()).collect();
    let starts: std::collections::BTreeSet<u64> = shim_events
        .iter()
        .filter_map(|e| e["shim_start"].as_u64())
        .collect();
    assert_eq!(starts.len(), 1, "one shim");
    let seqs: Vec<u64> = shim_events
        .iter()
        .filter_map(|e| e["seq"].as_u64())
        .collect();
    let expected: Vec<u64> = (0..u64::try_from(seqs.len()).expect("fits")).collect();
    assert_eq!(seqs, expected, "gaps or duplicates in stored history");
}

#[test]
fn corrupt_event_history_is_kept_and_supervision_continues() {
    let env = Env::new();
    env.ok(&["start", "loop.sh"]);
    let pid = pids(&env.app("loop"))[0];
    kill_daemon(&env);
    let db = env.home.path().join("events.db");
    for side in ["events.db-wal", "events.db-shm"] {
        let _ = std::fs::remove_file(env.home.path().join(side));
    }
    std::fs::write(&db, vec![0x42; 8192]).expect("corrupt");

    let out = env.run(&["list", "--json"]);
    assert!(out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("event history was unreadable"), "{stderr}");
    let app = env.app("loop");
    assert_eq!(pids(&app), [pid], "the app must not be restarted");
    let kept: Vec<_> = std::fs::read_dir(env.home.path())
        .expect("home")
        .flatten()
        .filter(|e| {
            e.file_name()
                .to_string_lossy()
                .starts_with("events.db.corrupt-")
        })
        .collect();
    assert_eq!(kept.len(), 1);
    assert_eq!(
        std::fs::read(kept[0].path()).expect("kept"),
        vec![0x42; 8192]
    );
    // New history is recorded in the fresh file.
    env.ok(&["restart", "loop"]);
    assert!(events(&env, "loop").iter().any(|e| e["state"] == "online"));
}

#[test]
fn events_command_table_and_filters() {
    let env = Env::new();
    env.ok(&["start", "loop.sh", "--name", "one"]);
    env.ok(&["start", "loop.sh", "--name", "two"]);
    let table = env.ok(&["events", "one"]);
    assert!(table.starts_with("TIME (UTC)"), "{table}");
    assert!(table.contains("online"), "{table}");
    assert!(!table.contains("two"), "{table}");
    std::thread::sleep(Duration::from_millis(1500));
    env.ok(&["restart", "two"]);
    let recent = env.ok(&["events", "--since", "1s", "--json"]);
    let recent: Vec<Value> = serde_json::from_str(&recent).expect("json");
    assert!(!recent.is_empty());
    assert!(recent.iter().all(|e| e["app"] == "two"), "{recent:#?}");
    let limited: Vec<Value> =
        serde_json::from_str(&env.ok(&["events", "--limit", "2", "--json"])).expect("json");
    assert_eq!(limited.len(), 2);
}

// --------------------------------------------------------- ecosystem files

fn ecosystem(env: &Env, body: &str) -> PathBuf {
    let path = env.work.path().join("ecosystem.config.js");
    std::fs::write(&path, body).expect("write ecosystem");
    path
}

const ECOSYSTEM: &str = r"
module.exports = {
  apps: [
    { name: 'a', script: './loop.sh', env: { MODE: 'dev' }, env_production: { MODE: 'prod' } },
    { name: 'b', script: 'loop.sh', instances: 2, merge_logs: true, autostart: false },
    { name: 'm', script: 'loop.sh', max_memory_restart: '300M' },
  ],
};
";

#[test]
fn ecosystem_check_reports_without_starting() {
    if which("node").is_none() {
        eprintln!("skipping: node not found");
        return;
    }
    let env = Env::new();
    ecosystem(&env, ECOSYSTEM);
    let out = env.run(&[
        "check",
        "ecosystem.config.js",
        "--json",
        "--env",
        "production",
    ]);
    assert_eq!(
        out.status.code(),
        Some(1),
        "fatal field must fail the check"
    );
    let report: Value = serde_json::from_slice(&out.stdout).expect("json");
    let names: Vec<_> = report["apps"]
        .as_array()
        .expect("apps")
        .iter()
        .map(|a| a["name"].as_str().unwrap_or("").to_owned())
        .collect();
    assert_eq!(names, ["a", "b", "m"]);
    assert_eq!(
        report["apps"][0]["env"]["MODE"], "<redacted>",
        "values are not shown"
    );
    assert_eq!(report["apps"][1]["autostart"], false);
    assert!(
        report["notes"]
            .as_array()
            .expect("notes")
            .iter()
            .any(|n| n["field"] == "max_memory_restart" && n["severity"] == "fatal")
    );
    assert!(
        !env.home.path().join("daemon.sock").exists(),
        "check must not start the daemon"
    );
}

#[test]
fn ecosystem_start_blocks_on_unsupported_then_starts_selected_apps() {
    if which("node").is_none() {
        eprintln!("skipping: node not found");
        return;
    }
    let env = Env::new();
    ecosystem(&env, ECOSYSTEM);
    let out = env.run(&["start", "ecosystem.config.js"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("nothing was started"));
    assert!(
        !env.home.path().join("daemon.sock").exists(),
        "nothing reached the daemon"
    );

    env.ok(&[
        "start",
        "ecosystem.config.js",
        "--only",
        "a,b",
        "--env",
        "production",
    ]);
    let state = env.state();
    assert_eq!(state["apps"]["a"]["spec"]["env"]["MODE"], "prod");
    assert_eq!(state["apps"]["a"]["desired"], "running");
    assert_eq!(state["apps"]["b"]["desired"], "stopped", "autostart: false");
    assert!(state["apps"].get("m").is_none());
    assert_eq!(states(&env.app("a")), ["online"]);

    // The unsupported app can still be started deliberately.
    env.ok(&[
        "start",
        "ecosystem.config.js",
        "--only",
        "m",
        "--allow-unsupported",
    ]);
    assert_eq!(states(&env.app("m")), ["online"]);
}

#[test]
fn json_ecosystem_needs_no_node() {
    let env = Env::new();
    std::fs::write(
        env.work.path().join("apps.json"),
        r#"{"apps":[{"name":"j","script":"loop.sh","args":"--x 'a b'"}]}"#,
    )
    .expect("write");
    let out = env
        .cmd(&["check", "apps.json", "--json"])
        .env("PATH", "/usr/bin:/bin")
        .output()
        .expect("run");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let report: Value = serde_json::from_slice(&out.stdout).expect("json");
    // `.sh` runs with bash, as in PM2: the script comes first.
    let args = report["apps"][0]["args"].as_array().expect("args");
    assert!(
        args[0].as_str().is_some_and(|a| a.ends_with("/loop.sh")),
        "{args:?}"
    );
    assert_eq!(
        args[1..],
        [serde_json::json!("--x"), serde_json::json!("a b")]
    );
    assert!(
        report["apps"][0]["exec"]
            .as_str()
            .is_some_and(|e| e.ends_with("bash")),
        "{report}"
    );
}

#[test]
fn ecosystem_load_errors_are_reported() {
    if which("node").is_none() {
        eprintln!("skipping: node not found");
        return;
    }
    let env = Env::new();
    ecosystem(&env, "module.exports = {;\n");
    let out = env.run(&["start", "ecosystem.config.js"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("SyntaxError"));
}
