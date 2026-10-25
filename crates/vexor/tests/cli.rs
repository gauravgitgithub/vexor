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

/// Log text without the per-line timestamps Vexor writes.
fn untimed(text: &str) -> String {
    text.lines()
        .map(|l| match l.split_once(' ') {
            Some((ts, rest)) if ts.len() >= 23 && ts.as_bytes()[10] == b'T' => rest,
            _ => l,
        })
        .map(|l| format!("{l}\n"))
        .collect()
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
    let out = env.run(&["start", "loop.sh", "--max-memory-restart", "1G"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("different configuration"));
    // Only the instance count differs: that is a scale, not a conflict.
    env.ok(&["start", "loop.sh", "-i", "3", "--fork"]);
    assert_eq!(env.app("loop")["instances"], 3);

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
        "policy": { "max_restarts": 4, "min_uptime_ms": 10000 },
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
    assert!(table.starts_with("Time"), "{table}");
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
    { name: 'm', script: 'loop.sh', shutdown_with_message: true },
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
            .any(|n| n["field"] == "shutdown_with_message" && n["severity"] == "fatal")
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

#[test]
fn instances_on_a_node_script_mean_cluster_mode_unless_fork() {
    if which("node").is_none() {
        eprintln!("skipping: node not found");
        return;
    }
    let env = Env::new();
    env.script("srv.js", "setInterval(function(){}, 1000)\n");
    env.ok(&["start", "srv.js", "-i", "2", "--name", "c"]);
    let spec = &env.state()["apps"]["c"]["spec"];
    assert!(
        spec["cluster"]["script"]
            .as_str()
            .is_some_and(|s| s.ends_with("srv.js")),
        "{spec}"
    );
    assert_eq!(states(&env.app("c")), ["online", "online"]);
    env.ok(&["start", "srv.js", "-i", "2", "--fork", "--name", "f"]);
    assert!(env.state()["apps"]["f"]["spec"].get("cluster").is_none());
    assert_eq!(states(&env.app("f")), ["online", "online"]);
}

// ------------------------------------------------------- describe and logs

#[test]
fn describe_shows_config_and_hides_env_values() {
    let env = Env::new();
    env.ok(&["start", "loop.sh", "--env", "API_KEY=hunter2"]);
    let text = env.ok(&["describe", "loop"]);
    for want in [
        "Name",
        "loop",
        "online",
        "fork mode",
        "API_KEY",
        "Restart",
        "Out log",
    ] {
        assert!(text.contains(want), "missing {want:?} in:\n{text}");
    }
    assert!(!text.contains("hunter2"), "env value leaked:\n{text}");
    assert!(
        env.ok(&["describe", "loop", "--show-env"])
            .contains("API_KEY=hunter2")
    );

    let json: Value = serde_json::from_str(&env.ok(&["describe", "loop", "--json"])).expect("json");
    assert_eq!(json["spec"]["env"]["API_KEY"], "<redacted>");
    assert_eq!(json["status"]["snapshot"][0]["state"], "online");
    assert!(json["events"].as_array().is_some_and(|e| !e.is_empty()));

    let out = env.run(&["describe", "nope"]);
    assert_eq!(out.status.code(), Some(1));
}

#[test]
fn logs_show_the_last_lines_per_stream() {
    let env = Env::new();
    env.script(
        "talk.sh",
        "#!/bin/sh\ni=1; while [ $i -le 20 ]; do echo \"out $i\"; echo \"err $i\" >&2; i=$((i+1)); done\nwhile :; do sleep 1; done\n",
    );
    env.ok(&["start", "talk.sh"]);
    let deadline = Instant::now() + Duration::from_secs(10);
    while !env
        .ok(&["logs", "talk", "--raw", "--out", "--lines", "1"])
        .contains("out 20")
    {
        assert!(Instant::now() < deadline, "output never reached the log");
        std::thread::sleep(Duration::from_millis(50));
    }
    // Log files carry a timestamp per line; --raw prints them as written.
    let raw = env.ok(&["logs", "talk", "--raw", "--out", "--lines", "3"]);
    assert!(
        raw.lines()
            .all(|l| l.len() > 30 && l.as_bytes()[10] == b'T'),
        "{raw}"
    );
    assert_eq!(untimed(&raw), "out 18\nout 19\nout 20\n");
    assert_eq!(
        untimed(&env.ok(&["logs", "talk", "--raw", "--err", "--lines", "2"])),
        "err 19\nerr 20\n"
    );
    // One timeline: time, app, stream marker (| out, ! err), message.
    let both = env.ok(&["logs", "--lines", "1"]);
    let lines: Vec<&str> = both.lines().collect();
    assert_eq!(lines.len(), 2, "{both}");
    // The two streams are read and stamped separately: either can come
    // first.
    for want in ["talk | out 20", "talk ! err 20"] {
        assert!(
            lines
                .iter()
                .any(|l| l.ends_with(want) && l.as_bytes()[2] == b':' && l.as_bytes()[8] == b'.'),
            "{both}"
        );
    }
    let out = env.run(&["logs", "nope"]);
    assert_eq!(out.status.code(), Some(1));
}

#[test]
fn logs_follow_keeps_every_line_across_rotation() {
    let env = Env::new();
    let script = env.script(
        "count.sh",
        // Batches of 20 lines (echo is a builtin): the rate doesn't depend on
        // how fast a busy CI machine can fork `sleep`.
        "#!/bin/sh\ni=1; while :; do j=0; while [ $j -lt 20 ]; do echo \"n $i\"; i=$((i+1)); j=$((j+1)); done; sleep 0.02; done\n",
    );
    let spec = env.work.path().join("count.json");
    let json = serde_json::json!({
        "name": "count",
        "exec": script,
        "cwd": env.work.path(),
        "env": { "PATH": "/usr/bin:/bin" },
        "logs": {
            "timestamps": false,
            "out": env.work.path().join("count.out"),
            "err": env.work.path().join("count.err"),
            "max_size_bytes": 2048,
            "retain": 50,
            "overflow": "block",
        },
    });
    std::fs::write(&spec, json.to_string()).expect("spec");
    env.ok(&["start", "--spec", &spec.to_string_lossy()]);

    let mut child = env
        .cmd(&[
            "logs", "count", "--out", "--raw", "--follow", "--lines", "0",
        ])
        .stdout(Stdio::piped())
        .spawn()
        .expect("follow");
    let stdout = child.stdout.take().expect("stdout");
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        use std::io::BufRead;
        for line in std::io::BufReader::new(stdout)
            .lines()
            .map_while(Result::ok)
        {
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    // Collect until several rotations have happened (2 KiB files, ~6
    // bytes per line: a rotation every ~350 lines).
    let mut seen = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(30);
    while seen.len() < 1500 && Instant::now() < deadline {
        if let Ok(line) = rx.recv_timeout(Duration::from_secs(1)) {
            seen.push(line);
        }
    }
    let _ = child.kill();
    let _ = child.wait();
    let numbers: Vec<u64> = seen
        .iter()
        .filter_map(|l| l.strip_prefix("n ")?.parse().ok())
        .collect();
    assert!(
        numbers.len() >= 1500,
        "only {} lines followed",
        numbers.len()
    );
    let rotated = std::fs::read_dir(env.work.path())
        .expect("dir")
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().starts_with("count.out."))
        .count();
    assert!(rotated >= 3, "expected rotations, saw {rotated}");
    assert!(
        numbers.windows(2).all(|w| w[1] == w[0] + 1),
        "gap or duplicate while following across rotation"
    );
}

// ------------------------------------------- save / resurrect / startup

#[test]
fn save_and_resurrect_restore_apps_and_their_desired_state() {
    let env = Env::new();
    env.ok(&["start", "loop.sh", "--name", "up"]);
    env.ok(&["start", "loop.sh", "--name", "down"]);
    env.ok(&["stop", "down"]);
    let out = env.run(&["resurrect"]);
    assert_eq!(out.status.code(), Some(1), "nothing saved yet");
    assert!(String::from_utf8_lossy(&out.stderr).contains("vexor save"));

    env.ok(&["save"]);
    let dump: Value = serde_json::from_str(
        &std::fs::read_to_string(env.home.path().join("dump.json")).expect("dump"),
    )
    .expect("json");
    assert_eq!(dump["apps"]["up"]["desired"], "running");
    assert_eq!(dump["apps"]["down"]["desired"], "stopped");
    let mode = std::fs::metadata(env.home.path().join("dump.json"))
        .expect("meta")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o600, "the snapshot holds environment values");

    env.ok(&["delete", "all"]);
    assert!(env.list().is_empty());
    env.ok(&["resurrect"]);
    let up = env.app("up");
    assert_eq!(states(&up), ["online"]);
    assert_eq!(env.app("down")["desired"], "stopped");
    // Idempotent: a second resurrect changes nothing.
    let pid = pids(&up);
    env.ok(&["resurrect"]);
    assert_eq!(pids(&env.app("up")), pid);
}

#[test]
fn startup_print_generates_a_service_for_this_platform() {
    let env = Env::new();
    let init = if cfg!(target_os = "macos") {
        "launchd"
    } else {
        "systemd"
    };
    let out = env.ok(&["startup", init, "--print", "--user", "deploy"]);
    let home = env.home.path().to_string_lossy().into_owned();
    assert!(out.contains(&home), "{out}");
    assert!(out.contains("daemon"), "{out}");
    if init == "systemd" {
        assert!(
            out.contains("User=deploy") && out.contains("KillMode=process"),
            "{out}"
        );
    } else {
        assert!(
            out.contains("dev.vexor.daemon.deploy") && out.contains("AbandonProcessGroup"),
            "{out}"
        );
    }
    let bad = env.run(&["startup", "openrc", "--print"]);
    assert_eq!(bad.status.code(), Some(1));
}

/// A minimal PM2 dump: `web` (running, 2 fork instances of a shell script
/// with a secret in its environment) and `idle` (stopped).
fn pm2_dump(env: &Env) -> PathBuf {
    let script = env.work.path().join("loop.sh");
    let user = std::env::var("USER").unwrap_or_default();
    let entry = |name: &str, status: &str, inst: u32| {
        serde_json::json!({
            "name": name, "namespace": "default", "status": status,
            "pm_exec_path": script, "pm_cwd": env.work.path(),
            "exec_interpreter": "none", "exec_mode": "fork_mode",
            "args": ["a b"], "username": user, "_pm2_version": "7.1.0",
            "pm_out_log_path": env.work.path().join(format!("{name}-out.log")),
            "pm_err_log_path": env.work.path().join(format!("{name}-err.log")),
            "pmx": true, "automation": true, "kill_retry_time": 100,
            "env": {"PATH": "/usr/bin:/bin", "API_KEY": "s3cr3t-value", "PM2_HOME": "/x", name: "{}"},
            "API_KEY": "s3cr3t-value", "NODE_APP_INSTANCE": inst,
            "max_restarts": 7
        })
    };
    let dump = serde_json::json!([
        entry("web", "online", 0),
        entry("web", "online", 1),
        entry("idle", "stopped", 0)
    ]);
    let path = env.work.path().join("dump.pm2");
    std::fs::write(&path, dump.to_string()).expect("write dump");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).expect("chmod");
    path
}

#[test]
fn migrate_dry_run_reports_and_changes_nothing() {
    let env = Env::new();
    let dump = pm2_dump(&env);
    let dump = dump.to_str().expect("utf-8");
    // Default: the plan only.
    let plan = env.ok(&["migrate", "--from-pm2", "--dump", dump]);
    assert!(plan.contains("2 apps · 3 processes · PM2 7.1.0"), "{plan}");
    assert!(
        plan.contains("move (brief stop)") && plan.contains("add stopped"),
        "{plan}"
    );
    assert!(plan.contains("2 of 2 apps ready to move"), "{plan}");
    assert!(
        plan.contains("Next: vexor migrate --from-pm2 --backup"),
        "{plan}"
    );
    assert!(
        !plan.contains("variables") && !plan.contains("run:"),
        "{plan}"
    );
    // --verbose: commands, paths, environment.
    let out = env.ok(&["migrate", "--from-pm2", "--dump", dump, "--verbose"]);
    assert!(out.contains("PM2 7.1.0, 3 processes, 2 apps"), "{out}");
    assert!(out.contains("brief (stop, then start)"), "{out}");
    assert!(out.contains("none (not running)"), "{out}");
    assert!(out.contains("'a b'"), "{out}");
    // Both apps share one environment: summarised once, names on request.
    assert!(
        out.contains("2 of 2 apps have the same 2 variables"),
        "{out}"
    );
    assert!(out.contains("env:  the shared 2 variables"), "{out}");
    assert!(!out.contains("API_KEY"), "{out}");
    let named = env.ok(&["migrate", "--from-pm2", "--dump", dump, "--env-names"]);
    assert!(named.contains("API_KEY"), "{named}");
    for o in [&plan, &out, &named] {
        assert!(!o.contains("s3cr3t-value"), "env value printed: {o}");
        assert!(!o.contains("PM2_HOME"), "PM2's own variables kept: {o}");
    }
    assert!(out.contains("All 2 apps can be migrated"), "{out}");
    // A dry run doesn't start the daemon or create Vexor state.
    assert!(!env.home.path().join("daemon.sock").exists());
    assert!(!env.home.path().join("state.json").exists());

    let json: Value =
        serde_json::from_str(&env.ok(&["migrate", "--from-pm2", "--dump", dump, "--json"]))
            .expect("json");
    let web = &json["apps"][0];
    assert_eq!(web["name"], "web");
    assert_eq!(web["instances"], 2);
    assert_eq!(web["downtime"], "brief");
    assert_eq!(web["spec"]["policy"]["max_restarts"], 7);
    assert_eq!(web["spec"]["env"]["API_KEY"], "<redacted>");
    assert_eq!(json["apps"][1]["downtime"], "none");
    assert!(!json.to_string().contains("s3cr3t-value"));
}

#[test]
fn migrate_blocked_apps_exit_4_and_missing_dump_explains() {
    let env = Env::new();
    let dump = pm2_dump(&env);
    let mut d: Value =
        serde_json::from_str(&std::fs::read_to_string(&dump).expect("read")).expect("json");
    d[2]["shutdown_with_message"] = true.into();
    std::fs::write(&dump, d.to_string()).expect("write");
    let out = env.run(&[
        "migrate",
        "--from-pm2",
        "--dump",
        dump.to_str().expect("utf-8"),
    ]);
    assert_eq!(out.status.code(), Some(4));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("shutdown_with_message"), "{stdout}");
    assert!(
        stdout.contains("blocked: shutdown_with_message"),
        "{stdout}"
    );
    assert!(stdout.contains("1 of 2 apps ready to move"), "{stdout}");
    assert!(stdout.contains("Needs attention"), "{stdout}");

    let out = env.run(&[
        "migrate",
        "--from-pm2",
        "--pm2-home",
        env.work.path().join("nope").to_str().expect("utf-8"),
    ]);
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("pm2 save"));

    // --from-pm2 is required.
    assert_eq!(env.run(&["migrate"]).status.code(), Some(2));
}

#[test]
fn migrate_backup_copies_pm2_state_privately_and_never_overwrites() {
    let env = Env::new();
    let pm2_home = env.work.path().join("pm2home");
    std::fs::create_dir(&pm2_home).expect("mkdir");
    let dump = pm2_dump(&env);
    std::fs::rename(&dump, pm2_home.join("dump.pm2")).expect("move dump");
    std::fs::write(pm2_home.join("module_conf.json"), "{}").expect("write");
    let original = std::fs::read(pm2_home.join("dump.pm2")).expect("read");
    let ph = pm2_home.to_str().expect("utf-8");

    let first: Value = serde_json::from_str(&env.ok(&[
        "migrate",
        "--from-pm2",
        "--backup",
        "--pm2-home",
        ph,
        "--json",
    ]))
    .expect("json");
    let second: Value = serde_json::from_str(&env.ok(&[
        "migrate",
        "--from-pm2",
        "--backup",
        "--pm2-home",
        ph,
        "--json",
    ]))
    .expect("json");
    let (a, b) = (
        PathBuf::from(first["backup"].as_str().expect("dir")),
        PathBuf::from(second["backup"].as_str().expect("dir")),
    );
    assert_ne!(
        a, b,
        "a second backup must not reuse the first one's directory"
    );
    assert!(a.starts_with(env.home.path().join("migrations")));

    let mode = |p: &Path| std::fs::metadata(p).expect("stat").permissions().mode() & 0o777;
    assert_eq!(mode(&env.home.path().join("migrations")), 0o700);
    assert_eq!(mode(&a), 0o700);
    for f in [
        "pm2/dump.pm2",
        "pm2/module_conf.json",
        "plan.json",
        "manifest.json",
    ] {
        assert_eq!(mode(&a.join(f)), 0o600, "{f}");
    }
    assert_eq!(
        std::fs::read(a.join("pm2/dump.pm2")).expect("read"),
        original
    );
    let manifest: Value =
        serde_json::from_slice(&std::fs::read(a.join("manifest.json")).expect("read"))
            .expect("json");
    assert_eq!(manifest["files"].as_array().expect("files").len(), 2);
    assert_eq!(manifest["files"][0]["copy"], "pm2/dump.pm2");
    assert_eq!(manifest["files"][0]["mode"], 0o600);
    assert_eq!(
        manifest["files"][0]["sha256"].as_str().expect("hash").len(),
        64
    );
    // The plan is redacted; only the dump copy holds values.
    let plan = std::fs::read_to_string(a.join("plan.json")).expect("read");
    assert!(plan.contains("API_KEY") && !plan.contains("s3cr3t-value"));
    // No temporary directories left behind, PM2 untouched, no daemon.
    let entries: Vec<_> = std::fs::read_dir(env.home.path().join("migrations"))
        .expect("ls")
        .map(|e| e.expect("entry").file_name())
        .collect();
    assert_eq!(entries.len(), 2, "{entries:?}");
    assert_eq!(
        std::fs::read(pm2_home.join("dump.pm2")).expect("read"),
        original
    );
    assert!(!env.home.path().join("daemon.sock").exists());

    // No dump: nothing is created.
    let empty = env.work.path().join("empty");
    std::fs::create_dir(&empty).expect("mkdir");
    let out = env.run(&[
        "migrate",
        "--from-pm2",
        "--backup",
        "--pm2-home",
        empty.to_str().expect("utf-8"),
    ]);
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(
        std::fs::read_dir(env.home.path().join("migrations"))
            .expect("ls")
            .count(),
        2
    );

    // --backup and --dry-run contradict each other.
    assert_eq!(
        env.run(&["migrate", "--from-pm2", "--backup", "--dry-run"])
            .status
            .code(),
        Some(2)
    );
}

/// A stand-in for `pm2` that records its arguments (except `jlist`); `pm2
/// stop` fails while `<work>/pm2-fail` exists. It keeps each app's status in
/// `<work>/pm2-live` (`name status` lines; it starts as `pm2_dump` saved it:
/// `web` online, `idle` stopped), which `stop` and `start` update and `jlist`
/// reports.
fn fake_pm2(env: &Env) -> (PathBuf, PathBuf) {
    let log = env.work.path().join("pm2-calls.log");
    let fail = env.work.path().join("pm2-fail");
    let live = env.work.path().join("pm2-live");
    std::fs::write(&live, "web online\nidle stopped\n").expect("write pm2-live");
    let bin = env.script(
        "fake-pm2",
        &format!(
            r#"#!/bin/sh
set_status() {{ grep -v "^$1 " '{live}' > '{live}.tmp'; echo "$1 $2" >> '{live}.tmp'; mv '{live}.tmp' '{live}'; }}
if [ "$1" = jlist ]; then
  printf '['; sep=''
  while read -r n st; do printf '%s{{"name":"%s","pm2_env":{{"status":"%s"}}}}' "$sep" "$n" "$st"; sep=,; done < '{live}'
  echo ']'; exit 0
fi
echo "$*" >> '{log}'
[ "$1" = stop ] && [ -e '{fail}' ] && {{ echo 'boom' >&2; exit 1; }}
[ "$1" = stop ] && set_status "$2" stopped
[ "$1" = start ] && set_status "$2" online
exit 0
"#,
            log = log.display(),
            fail = fail.display(),
            live = live.display(),
        ),
    );
    (bin, log)
}

fn pm2_calls(log: &Path) -> Vec<String> {
    std::fs::read_to_string(log)
        .unwrap_or_default()
        .lines()
        .map(str::to_owned)
        .collect()
}

/// PM2 home holding `pm2_dump`, plus a backup of it.
fn pm2_home_with_backup(env: &Env) -> PathBuf {
    let pm2_home = env.work.path().join("pm2home");
    std::fs::create_dir_all(&pm2_home).expect("mkdir");
    let dump = pm2_dump(env);
    std::fs::rename(&dump, pm2_home.join("dump.pm2")).expect("move dump");
    env.ok(&[
        "migrate",
        "--from-pm2",
        "--backup",
        "--pm2-home",
        pm2_home.to_str().expect("utf-8"),
    ]);
    pm2_home
}

fn journal(env: &Env) -> Vec<Value> {
    let root = env.home.path().join("migrations");
    let dir = std::fs::read_dir(&root)
        .expect("ls")
        .map(|e| e.expect("entry").path())
        .find(|p| p.join("journal.jsonl").exists())
        .expect("a journal");
    std::fs::read_to_string(dir.join("journal.jsonl"))
        .expect("read")
        .lines()
        .map(|l| serde_json::from_str(l).expect("json line"))
        .collect()
}

#[test]
fn migrate_apply_moves_running_apps_and_resumes_idempotently() {
    let env = Env::new();
    let pm2_home = pm2_home_with_backup(&env);
    let (pm2, log) = fake_pm2(&env);
    let args = [
        "migrate",
        "--from-pm2",
        "--apply",
        "--yes",
        "--pm2-home",
        pm2_home.to_str().expect("utf-8"),
        "--pm2",
        pm2.to_str().expect("utf-8"),
    ];
    let out = env.ok(&args);
    assert!(
        out.contains("web: stopped under PM2, running under Vexor"),
        "{out}"
    );
    assert!(out.contains("idle: added to Vexor (stopped)"), "{out}");
    assert!(out.contains("pm2 unstartup"), "{out}");
    // Only the running app is stopped under PM2, and nothing is deleted there.
    assert_eq!(pm2_calls(&log), ["stop web"]);

    let apps = env.list();
    let web = apps.iter().find(|a| a["name"] == "web").expect("web");
    assert_eq!(states(web), ["online", "online"]);
    let idle = apps.iter().find(|a| a["name"] == "idle").expect("idle");
    assert_eq!(idle["desired"], "stopped");
    let steps: Vec<(String, String)> = journal(&env)
        .iter()
        .map(|e| {
            (
                e["app"].as_str().unwrap_or("").to_owned(),
                e["step"].as_str().unwrap_or("").to_owned(),
            )
        })
        .collect();
    assert_eq!(
        steps,
        [
            ("web", "begin"),
            ("web", "pm2_stopped"),
            ("web", "vexor_started"),
            ("web", "done"),
            ("idle", "begin"),
            ("idle", "vexor_started"),
            ("idle", "done"),
        ]
        .map(|(a, s)| (a.to_owned(), s.to_owned()))
    );

    // Running it again changes nothing.
    let again = env.ok(&args);
    assert!(again.contains("already migrated: web, idle"), "{again}");
    assert!(again.contains("Nothing to migrate"), "{again}");
    assert_eq!(pm2_calls(&log), ["stop web"]);
}

#[test]
fn migrate_apply_puts_an_unhealthy_app_back_under_pm2() {
    let env = Env::new();
    env.script("loop.sh", "#!/bin/sh\necho crashing >&2\nexit 3\n");
    // The health window is min_uptime (at least 1 s). Longer than
    // --timeout, so `web` can never pass as healthy: on a loaded macOS
    // machine an exec'd process can sit at _dyld_start for seconds before
    // it runs (and crashes), and would otherwise be seen online throughout.
    let pm2_home = env.work.path().join("pm2home");
    std::fs::create_dir_all(&pm2_home).expect("mkdir");
    let dump = pm2_dump(&env);
    let mut d: Value =
        serde_json::from_str(&std::fs::read_to_string(&dump).expect("read")).expect("json");
    for e in d.as_array_mut().expect("array") {
        e["min_uptime"] = 15_000.into();
    }
    std::fs::write(pm2_home.join("dump.pm2"), d.to_string()).expect("write");
    env.ok(&[
        "migrate",
        "--from-pm2",
        "--backup",
        "--pm2-home",
        pm2_home.to_str().expect("utf-8"),
    ]);
    let (pm2, log) = fake_pm2(&env);
    let out = env.run(&[
        "migrate",
        "--from-pm2",
        "--apply",
        "--yes",
        "--only",
        "web",
        "--timeout",
        "10",
        "--pm2-home",
        pm2_home.to_str().expect("utf-8"),
        "--pm2",
        pm2.to_str().expect("utf-8"),
    ]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(1), "{stderr}");
    assert!(stderr.contains("running under PM2 again"), "{stderr}");
    assert_eq!(pm2_calls(&log), ["stop web", "start web"]);
    assert!(
        env.list().iter().all(|a| a["name"] != "web"),
        "left in Vexor"
    );
    let last = journal(&env).pop().expect("entry");
    assert_eq!(
        (last["app"].as_str(), last["step"].as_str()),
        (Some("web"), Some("rolled_back"))
    );
}

#[test]
fn migrate_apply_refuses_unsafe_starts() {
    let env = Env::new();
    let (pm2, log) = fake_pm2(&env);
    let pm2 = pm2.to_str().expect("utf-8").to_owned();
    // No backup yet.
    let nowhere = env.work.path().join("pm2home");
    std::fs::create_dir_all(&nowhere).expect("mkdir");
    let dump = pm2_dump(&env);
    std::fs::copy(&dump, nowhere.join("dump.pm2")).expect("copy");
    let ph = nowhere.to_str().expect("utf-8").to_owned();
    let apply = |extra: &[&str]| {
        let mut a = vec![
            "migrate",
            "--from-pm2",
            "--apply",
            "--pm2-home",
            &ph,
            "--pm2",
            &pm2,
        ];
        a.extend_from_slice(extra);
        env.run(&a)
    };
    let out = apply(&["--yes"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("--backup"));

    env.ok(&["migrate", "--from-pm2", "--backup", "--pm2-home", &ph]);
    // Not a terminal and no --yes.
    let out = apply(&[]);
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("--yes"));

    // PM2's dump changed since the backup.
    let mut d: Value =
        serde_json::from_str(&std::fs::read_to_string(nowhere.join("dump.pm2")).expect("read"))
            .expect("json");
    d[0]["max_restarts"] = 3.into();
    std::fs::write(nowhere.join("dump.pm2"), d.to_string()).expect("write");
    let out = apply(&["--yes"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("changed since backup"));
    std::fs::copy(&dump, nowhere.join("dump.pm2")).expect("restore");

    // Vexor already has an app by that name: PM2 is not touched.
    let script = env.work.path().join("loop.sh");
    env.ok(&["start", script.to_str().expect("utf-8"), "--name", "web"]);
    let out = apply(&["--yes"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("already has an app"));

    // pm2 stop failing: the app is not moved.
    env.ok(&["delete", "web"]);
    std::fs::write(env.work.path().join("pm2-fail"), "").expect("touch");
    let out = apply(&["--yes"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("boom"));
    assert!(env.list().iter().all(|a| a["name"] != "web"));
    assert_eq!(pm2_calls(&log), ["stop web"]);
}

#[test]
fn migrate_apply_refuses_when_pm2_no_longer_matches_its_saved_list() {
    let env = Env::new();
    let pm2_home = pm2_home_with_backup(&env);
    let (pm2, log) = fake_pm2(&env);
    // `idle` was saved as stopped but has been started under PM2 since:
    // applying the plan would add it to Vexor stopped and leave it running
    // under PM2.
    std::fs::write(
        env.work.path().join("pm2-live"),
        "web online\nidle online\n",
    )
    .expect("write");
    let args = [
        "migrate",
        "--from-pm2",
        "--apply",
        "--yes",
        "--pm2-home",
        pm2_home.to_str().expect("utf-8"),
        "--pm2",
        pm2.to_str().expect("utf-8"),
    ];
    let out = env.run(&args);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(1), "{stderr}");
    assert!(stderr.contains("out of date"), "{stderr}");
    assert!(
        stderr.contains("idle (saved stopped, now running)"),
        "{stderr}"
    );
    assert!(stderr.contains("pm2 save"), "{stderr}");
    assert!(pm2_calls(&log).is_empty(), "PM2 was touched");
    assert!(env.list().is_empty(), "Vexor was touched");

    // And the other way round: saved as running, gone from PM2.
    std::fs::write(env.work.path().join("pm2-live"), "idle stopped\n").expect("write");
    let out = env.run(&args);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(1), "{stderr}");
    assert!(
        stderr.contains("web (saved running, now not in PM2)"),
        "{stderr}"
    );
    assert!(pm2_calls(&log).is_empty(), "PM2 was touched");
}

#[test]
fn migrate_rollback_and_status() {
    let env = Env::new();
    let pm2_home = pm2_home_with_backup(&env);
    let (pm2, log) = fake_pm2(&env);
    let (ph, pm2) = (
        pm2_home.to_str().expect("utf-8"),
        pm2.to_str().expect("utf-8"),
    );
    let status = env.ok(&["migrate", "--status"]);
    assert!(status.contains("backup only (not applied)"), "{status}");

    env.ok(&[
        "migrate",
        "--from-pm2",
        "--apply",
        "--yes",
        "--pm2-home",
        ph,
        "--pm2",
        pm2,
    ]);
    let status = env.ok(&["migrate", "--status"]);
    assert!(
        status.contains("web") && status.contains("migrated"),
        "{status}"
    );

    // Not a terminal and no --yes: refused, nothing changed.
    let out = env.run(&["migrate", "--rollback", "--pm2", pm2]);
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(env.list().len(), 2);

    let out = env.ok(&["migrate", "--rollback", "--yes", "--pm2", pm2]);
    assert!(out.contains("idle: deleted from Vexor"), "{out}");
    assert!(
        out.contains("web: deleted from Vexor, online under PM2"),
        "{out}"
    );
    // Newest first: idle (never running under PM2, so no pm2 call), then web.
    assert_eq!(pm2_calls(&log), ["stop web", "start web"]);
    assert!(env.list().is_empty());
    let status: Value =
        serde_json::from_str(&env.ok(&["migrate", "--status", "--json"])).expect("json");
    let steps: Vec<&str> = status[0]["apps"]
        .as_array()
        .expect("apps")
        .iter()
        .filter_map(|a| a["step"].as_str())
        .collect();
    assert_eq!(steps, ["rolled_back", "rolled_back"]);

    let again = env.ok(&["migrate", "--rollback", "--yes", "--pm2", pm2]);
    assert!(again.contains("nothing to roll back"), "{again}");

    // Rolled back apps can be migrated again.
    env.ok(&[
        "migrate",
        "--from-pm2",
        "--apply",
        "--yes",
        "--pm2-home",
        ph,
        "--pm2",
        pm2,
    ]);
    assert_eq!(env.list().len(), 2);

    // Flags that only make sense when moving apps.
    assert_eq!(
        env.run(&["migrate", "--from-pm2", "--yes"]).status.code(),
        Some(2)
    );
}

#[test]
fn cron_restart_from_the_command_line() {
    let env = Env::new();
    let out = env.run(&["start", "loop.sh", "--cron-restart", "61 * * * *"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("--cron-restart"));

    env.ok(&["start", "loop.sh", "--cron-restart", "* * * * * *"]);
    let describe = env.ok(&["describe", "loop"]);
    assert!(
        describe.contains("restart at \"* * * * * *\""),
        "{describe}"
    );
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let ev = events(&env, "loop");
        if ev.iter().filter(|e| e["type"] == "cron_restart").count() >= 2 {
            break;
        }
        assert!(Instant::now() < deadline, "{ev:?}");
        std::thread::sleep(Duration::from_millis(200));
    }
    let printed = env.ok(&["events", "loop"]);
    assert!(
        printed.contains("cron_restart") && printed.contains("scheduled="),
        "{printed}"
    );
}

#[test]
fn watch_from_the_command_line() {
    let env = Env::new();
    env.ok(&["start", "loop.sh", "--watch"]);
    let describe = env.ok(&["describe", "loop"]);
    assert!(describe.contains("restart on changes in"), "{describe}");
    assert!(describe.contains("ignoring .* node_modules"), "{describe}");
    // Polling watchers take their first snapshot within a second.
    std::thread::sleep(Duration::from_millis(1500));
    std::fs::write(env.work.path().join("changed.js"), "x").expect("write");
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let ev = events(&env, "loop");
        if ev.iter().any(|e| e["type"] == "watch_restart") {
            break;
        }
        assert!(Instant::now() < deadline, "{ev:?}");
        std::thread::sleep(Duration::from_millis(200));
    }
    let printed = env.ok(&["events", "loop"]);
    assert!(printed.contains("changed.js"), "{printed}");
}

#[test]
fn health_check_from_the_command_line() {
    let env = Env::new();
    let out = env.run(&["start", "loop.sh", "--health-check", "http://10.0.0.1:80/"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("only target this machine"));

    // Nothing listens on the port: the app never becomes ready.
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .and_then(|l| l.local_addr())
        .expect("bind")
        .port();
    env.ok(&[
        "start",
        "loop.sh",
        "--health-check",
        &format!("127.0.0.1:{port}"),
    ]);
    let describe = env.ok(&["describe", "loop"]);
    assert!(
        describe.contains(&format!("connect to 127.0.0.1:{port} every 10s")),
        "{describe}"
    );
    std::thread::sleep(Duration::from_millis(1500));
    let app = env
        .list()
        .into_iter()
        .find(|a| a["name"] == "loop")
        .expect("app");
    assert_eq!(states(&app), ["starting"]);
}

#[test]
fn migrate_apply_continues_past_an_app_already_failing_under_pm2() {
    let env = Env::new();
    // `web` keeps crashing (and PM2 was restarting it); `idle` is fine.
    env.script("loop.sh", "#!/bin/sh\necho crashing >&2\nexit 3\n");
    let pm2_home = env.work.path().join("pm2home");
    std::fs::create_dir_all(&pm2_home).expect("mkdir");
    let dump = pm2_dump(&env);
    let mut d: Value =
        serde_json::from_str(&std::fs::read_to_string(&dump).expect("read")).expect("json");
    for e in d.as_array_mut().expect("array") {
        if e["name"] == "web" {
            e["status"] = "waiting restart".into();
            // Longer than --timeout: `web` can't pass as healthy even when
            // it sits at _dyld_start for seconds on a loaded macOS machine
            // and is seen online, not crashing (1 run in 10 with the suite
            // in parallel; every run with two suites at once).
            e["min_uptime"] = 15_000.into();
        }
    }
    std::fs::write(pm2_home.join("dump.pm2"), d.to_string()).expect("write");
    let ph = pm2_home.to_str().expect("utf-8");
    env.ok(&["migrate", "--from-pm2", "--backup", "--pm2-home", ph]);
    let (pm2, log) = fake_pm2(&env);
    let out = env.run(&[
        "migrate",
        "--from-pm2",
        "--apply",
        "--yes",
        "--timeout",
        "10",
        "--pm2-home",
        ph,
        "--pm2",
        pm2.to_str().expect("utf-8"),
    ]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(
        out.status.code(),
        Some(4),
        "{stdout}\n{}\nEVENTS {}\nLIST {}",
        String::from_utf8_lossy(&out.stderr),
        String::from_utf8_lossy(&env.run(&["events", "--json"]).stdout),
        String::from_utf8_lossy(&env.run(&["list", "--json"]).stdout)
    );
    assert!(
        stdout.contains("idle: added to Vexor (stopped)"),
        "{stdout}"
    );
    assert!(
        stdout.contains("Left on PM2 because they were already failing there: web"),
        "{stdout}"
    );
    assert_eq!(pm2_calls(&log), ["stop web", "start web"]);
    let names: Vec<Value> = env.list().iter().map(|a| a["name"].clone()).collect();
    assert_eq!(names, ["idle"]);
}

#[test]
fn migrate_puts_an_app_vexor_refuses_back_under_pm2() {
    let env = Env::new();
    let pm2_home = env.work.path().join("pm2home");
    std::fs::create_dir_all(&pm2_home).expect("mkdir");
    let dump = pm2_dump(&env);
    std::fs::rename(&dump, pm2_home.join("dump.pm2")).expect("move");
    let ph = pm2_home.to_str().expect("utf-8");
    env.ok(&["migrate", "--from-pm2", "--backup", "--pm2-home", ph]);
    // Vexor refuses the start after `pm2 stop`: here, a Vexor app by the
    // same name appears in between (as an incompatible daemon would refuse).
    let (pm2, log) = fake_pm2(&env);
    let shim = env.script(
        "stop-then-block",
        &format!(
            "#!/bin/sh\n'{}' \"$@\"\n[ \"$1\" = stop ] && '{}' --home '{}' start '{}' --name web --env X=1 >/dev/null 2>&1\nexit 0\n",
            pm2.display(),
            env!("CARGO_BIN_EXE_vexor"),
            env.home.path().display(),
            env.work.path().join("loop.sh").display()
        ),
    );
    let out = env.run(&[
        "migrate",
        "--from-pm2",
        "--apply",
        "--yes",
        "--only",
        "web",
        "--pm2-home",
        ph,
        "--pm2",
        shim.to_str().expect("utf-8"),
    ]);
    assert_eq!(out.status.code(), Some(1));
    // Whatever Vexor did, PM2 was asked to start the app again.
    assert_eq!(pm2_calls(&log), ["stop web", "start web"]);
}

#[test]
fn migrate_dry_run_blocks_names_vexor_cannot_use() {
    let env = Env::new();
    let dump = pm2_dump(&env);
    let mut d: Value =
        serde_json::from_str(&std::fs::read_to_string(&dump).expect("read")).expect("json");
    d[2]["name"] = "x".repeat(70).into();
    std::fs::write(&dump, d.to_string()).expect("write");
    let out = env.run(&[
        "migrate",
        "--from-pm2",
        "--dump",
        dump.to_str().expect("utf-8"),
    ]);
    assert_eq!(out.status.code(), Some(4));
    assert!(String::from_utf8_lossy(&out.stdout).contains("blocked: name"));
}

/// PM2 `sendSignal`, `flush` and `kill`.
#[test]
fn signal_flush_and_kill() {
    let env = Env::new();
    env.script(
        "sig.sh",
        "#!/bin/sh\ntrap 'echo got usr1' USR1\necho started\nwhile :; do sleep 0.1; done\n",
    );
    env.ok(&["start", "sig.sh", "--name", "sig"]);
    let deadline = Instant::now() + Duration::from_secs(10);
    while !env
        .ok(&["logs", "sig", "--raw", "--out"])
        .contains("started")
    {
        assert!(Instant::now() < deadline, "never started");
        std::thread::sleep(Duration::from_millis(50));
    }
    let out = env.ok(&["signal", "USR1", "sig"]);
    assert!(out.contains("sent USR1"), "{out}");
    while !env
        .ok(&["logs", "sig", "--raw", "--out"])
        .contains("got usr1")
    {
        assert!(Instant::now() < deadline, "signal not received");
        std::thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(env.run(&["signal", "BOGUS", "sig"]).status.code(), Some(1));

    let out = env.ok(&["flush", "sig"]);
    assert!(out.contains("flushed"), "{out}");
    assert!(
        !env.ok(&["logs", "sig", "--raw", "--out"])
            .contains("got usr1")
    );

    env.ok(&["kill"]);
    assert_eq!(env.app("sig")["desired"], "stopped");
    // The daemon is gone: its socket stops answering.
    let deadline = Instant::now() + Duration::from_secs(10);
    while env.count("daemon_stopped") == 0 {
        assert!(Instant::now() < deadline, "daemon still running");
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// PM2 `monit`: without a terminal, one frame with every instance, the
/// alerts and the latest logs.
#[test]
fn monit_prints_one_frame_without_a_terminal() {
    let env = Env::new();
    env.script(
        "hi.sh",
        "#!/bin/sh\necho hello monit\nwhile :; do sleep 0.1; done\n",
    );
    env.ok(&["start", "hi.sh", "--name", "hi", "-i", "2", "--fork"]);
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let out = env.ok(&["monit"]);
        if out.contains("hello monit") {
            assert!(out.contains("No open alerts"), "{out}");
            let rows = out
                .lines()
                .filter(|l| l.starts_with("hi ") && l.contains("online"))
                .count();
            assert_eq!(rows, 2, "{out}");
            break;
        }
        assert!(Instant::now() < deadline, "{out}");
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// `vexor doctor`: findings with fixes; problems set the exit status.
#[test]
fn doctor_reports_findings_with_fixes() {
    let env = Env::new();
    env.ok(&["start", "loop.sh"]);
    let out = env.ok(&["doctor", "--json"]);
    let findings: Vec<Value> = serde_json::from_str(&out).expect("json");
    assert!(
        findings.iter().any(|f| f["level"] == "ok"
            && f["check"]
                .as_str()
                .is_some_and(|c| c.contains("is answering"))),
        "{out}"
    );
    std::fs::set_permissions(env.home.path(), std::fs::Permissions::from_mode(0o755))
        .expect("chmod");
    let out = env.ok(&["doctor", "--json"]);
    let findings: Vec<Value> = serde_json::from_str(&out).expect("json");
    let open = findings
        .iter()
        .find(|f| {
            f["check"]
                .as_str()
                .is_some_and(|c| c.contains("readable by other users"))
        })
        .expect("permission warning");
    assert_eq!(open["level"], "warning");
    assert!(
        open["fix"]
            .as_str()
            .is_some_and(|f| f.starts_with("chmod 700")),
        "{open}"
    );
}
