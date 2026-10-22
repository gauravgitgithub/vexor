//! End-to-end tests of `vexor shim` against real processes.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tempfile::TempDir;

const TIMEOUT: Duration = Duration::from_secs(20);

struct Shim {
    dir: TempDir,
}

struct Finished {
    code: Option<i32>,
    events: Vec<Value>,
    elapsed: Duration,
    stderr: String,
}

impl Shim {
    fn new() -> Self {
        Shim {
            dir: tempfile::tempdir().expect("tempdir"),
        }
    }

    fn path(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
    }

    /// Write a spec with sensible test defaults, overridden by `extra`.
    fn spec(&self, exec: &str, args: &[&str], extra: Value) -> PathBuf {
        let mut spec = json!({
            "name": "t",
            "exec": exec,
            "args": args,
            "cwd": self.dir.path(),
            "env": { "PATH": "/usr/bin:/bin" },
            "logs": { "out": self.path("out.log"), "err": self.path("err.log") },
        });
        merge(&mut spec, extra);
        let path = self.path("spec.json");
        std::fs::write(&path, spec.to_string()).expect("write spec");
        path
    }

    fn start(&self, spec: &Path) -> Child {
        Command::new(env!("CARGO_BIN_EXE_vexor"))
            .arg("shim")
            .arg("--spec")
            .arg(spec)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("start vexor shim")
    }

    fn run(&self, spec: &Path) -> Finished {
        let started = Instant::now();
        finish(self.start(spec), started)
    }

    fn out_log(&self) -> String {
        std::fs::read_to_string(self.path("out.log")).unwrap_or_default()
    }

    fn wait_for_output(&self, needle: &str) {
        let deadline = Instant::now() + TIMEOUT;
        while !self.out_log().contains(needle) {
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {needle:?}"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

fn merge(base: &mut Value, extra: Value) {
    if let (Value::Object(b), Value::Object(e)) = (base, extra) {
        for (k, v) in e {
            b.insert(k, v);
        }
    }
}

fn finish(mut child: Child, started: Instant) -> Finished {
    let status = loop {
        if let Some(s) = child.try_wait().expect("try_wait") {
            break s;
        }
        if started.elapsed() > TIMEOUT {
            let _ = child.kill();
            panic!("vexor shim did not finish within {TIMEOUT:?}");
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    let elapsed = started.elapsed();
    let out = child.wait_with_output().expect("output");
    let events = String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(|l| serde_json::from_str(l).unwrap_or_else(|e| panic!("bad event {l:?}: {e}")))
        .collect();
    Finished {
        code: status.code(),
        events,
        elapsed,
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    }
}

fn states(events: &[Value], instance: u64) -> Vec<String> {
    events
        .iter()
        .filter(|e| e["type"] == "state" && e["instance"] == instance)
        .map(|e| e["state"].as_str().unwrap_or_default().to_owned())
        .collect()
}

fn kill(pid: u32, sig: &str) {
    let ok = Command::new("kill")
        .arg(format!("-{sig}"))
        .arg(pid.to_string())
        .status()
        .expect("run kill")
        .success();
    assert!(ok, "kill -{sig} {pid}");
}

fn which(program: &str) -> Option<String> {
    let out = Command::new("/bin/sh")
        .args(["-c", &format!("command -v {program}")])
        .output()
        .ok()?;
    let path = String::from_utf8(out.stdout).ok()?.trim().to_owned();
    path.starts_with('/').then_some(path)
}

const ONCE: fn() -> Value = || json!({ "policy": { "autorestart": false } });

// ------------------------------------------------------------- process setup

#[test]
fn arguments_are_passed_verbatim_without_a_shell() {
    let shim = Shim::new();
    let args = ["%s\n", "a b", "$HOME", "; rm -rf /", "`id`", "*", "'q'", ""];
    let done = shim.run(&shim.spec("/usr/bin/printf", &args, ONCE()));
    assert_eq!(done.code, Some(0), "{}", done.stderr);
    let printed: Vec<_> = shim.out_log().lines().map(str::to_owned).collect();
    assert_eq!(printed, args[1..]);
}

#[test]
fn environment_is_exactly_the_spec_plus_instance_var() {
    let shim = Shim::new();
    let extra = json!({
        "env": { "FOO": "bar baz", "EMPTY": "" },
        "policy": { "autorestart": false },
    });
    let done = shim.run(&shim.spec("/usr/bin/env", &[], extra));
    assert_eq!(done.code, Some(0), "{}", done.stderr);
    let mut lines: Vec<_> = shim.out_log().lines().map(str::to_owned).collect();
    lines.sort();
    assert_eq!(lines, ["EMPTY=", "FOO=bar baz", "NODE_APP_INSTANCE=0"]);
}

#[test]
fn runs_in_the_spec_working_directory() {
    let shim = Shim::new();
    let done = shim.run(&shim.spec("/bin/pwd", &["-P"], ONCE()));
    assert_eq!(done.code, Some(0), "{}", done.stderr);
    let expected = shim.dir.path().canonicalize().expect("canonical");
    assert_eq!(shim.out_log().trim(), expected.to_string_lossy());
}

#[test]
fn each_instance_gets_its_index() {
    let shim = Shim::new();
    let extra = json!({
        "instances": 3,
        "instance_var": "IDX",
        "policy": { "autorestart": false },
    });
    let done = shim.run(&shim.spec("/bin/sh", &["-c", "echo i=$IDX"], extra));
    assert_eq!(done.code, Some(0), "{}", done.stderr);
    let mut lines: Vec<_> = shim.out_log().lines().map(str::to_owned).collect();
    lines.sort();
    assert_eq!(lines, ["i=0", "i=1", "i=2"]);
}

#[test]
fn stderr_goes_to_the_error_log() {
    let shim = Shim::new();
    let done = shim.run(&shim.spec("/bin/sh", &["-c", "echo oops >&2"], ONCE()));
    assert_eq!(done.code, Some(0), "{}", done.stderr);
    let err = std::fs::read_to_string(shim.path("err.log")).expect("err log");
    assert_eq!(err, "oops\n");
    assert_eq!(shim.out_log(), "");
}

#[test]
fn running_states_report_pid_and_identity() {
    let shim = Shim::new();
    let done = shim.run(&shim.spec("/bin/sh", &["-c", "exit 0"], ONCE()));
    let online = done
        .events
        .iter()
        .find(|e| e["state"] == "online")
        .expect("online event");
    assert!(online["pid"].as_u64().is_some_and(|p| p > 0), "{online}");
    assert!(online["start"].as_u64().is_some(), "{online}");
    let stopped = done.events.last().expect("events");
    assert_eq!(stopped["state"], "stopped");
    assert_eq!(stopped["exit"]["code"], 0);
}

// ---------------------------------------------------------- crash handling

#[test]
fn crash_loop_restarts_with_floor_then_errors() {
    let shim = Shim::new();
    let extra = json!({ "instances": 2, "policy": { "max_restarts": 2 } });
    let done = shim.run(&shim.spec("/bin/sh", &["-c", "echo run; exit 3"], extra));
    assert_eq!(done.code, Some(1), "errored exits 1; {}", done.stderr);
    // 1 start + 2 restarts per instance.
    assert_eq!(shim.out_log().lines().count(), 6);
    // Two restarts, each at least 1 s after the previous spawn.
    assert!(done.elapsed >= Duration::from_secs(2), "{:?}", done.elapsed);
    for i in 0..2 {
        let last = done
            .events
            .iter()
            .rfind(|e| e["instance"] == i)
            .expect("events");
        assert_eq!(last["state"], "errored");
        assert_eq!(last["error"]["reason"], "crash_loop");
        assert_eq!(last["exit"]["code"], 3);
        assert_eq!(last["restarts"], 2);
    }
}

#[test]
fn missing_executable_is_reported_and_errors() {
    let shim = Shim::new();
    let extra = json!({ "policy": { "max_restarts": 0 } });
    let done = shim.run(&shim.spec("/nonexistent/app", &[], extra));
    assert_eq!(done.code, Some(1));
    let err = done
        .events
        .iter()
        .find(|e| e["type"] == "spawn_error")
        .expect("spawn_error event");
    assert_eq!(err["exec"], "/nonexistent/app");
    assert_eq!(states(&done.events, 0), ["starting", "errored"]);
}

#[test]
fn invalid_spec_exits_2_with_a_message() {
    let shim = Shim::new();
    let spec = shim.spec("relative/path", &[], json!({}));
    let done = shim.run(&spec);
    assert_eq!(done.code, Some(2));
    assert!(done.stderr.contains("absolute"), "{}", done.stderr);
}

// ---------------------------------------------------------------- stopping

#[test]
fn sigterm_stops_the_app_gracefully() {
    let shim = Shim::new();
    let script = r#"trap 'echo got-term; exit 0' TERM; echo started; while :; do sleep 0.05; done"#;
    let child = shim.start(&shim.spec("/bin/sh", &["-c", script], json!({})));
    let started = Instant::now();
    shim.wait_for_output("started");
    kill(child.id(), "TERM");
    let done = finish(child, started);
    assert_eq!(done.code, Some(0), "{}", done.stderr);
    assert!(shim.out_log().contains("got-term"));
    assert_eq!(
        states(&done.events, 0),
        ["starting", "online", "stopping", "stopped"]
    );
}

#[test]
fn app_ignoring_sigterm_is_killed_after_kill_timeout() {
    let shim = Shim::new();
    let script = r#"trap '' TERM; echo started; while :; do sleep 0.05; done"#;
    let extra = json!({ "policy": { "kill_timeout_ms": 300 } });
    let child = shim.start(&shim.spec("/bin/sh", &["-c", script], extra));
    let started = Instant::now();
    shim.wait_for_output("started");
    let signalled = Instant::now();
    kill(child.id(), "INT");
    let done = finish(child, started);
    assert_eq!(done.code, Some(0), "{}", done.stderr);
    assert!(signalled.elapsed() >= Duration::from_millis(300));
    let last = done.events.last().expect("events");
    assert_eq!(last["state"], "stopped");
    assert_eq!(last["exit"]["signal"], 9, "{last}");
}

// --------------------------------------------------------------------- node

fn node() -> Option<String> {
    let n = which("node");
    if n.is_none() {
        eprintln!("skipping: node not found on PATH");
    }
    n
}

#[test]
fn node_ready_message_gates_online() {
    let Some(node) = node() else { return };
    let shim = Shim::new();
    let script = "setTimeout(function(){ process.send('ready') }, 200); setTimeout(function(){ process.exit(0) }, 400)";
    let extra = json!({
        "node_ipc": true,
        "policy": { "ready_timeout_ms": 10000, "stop_exit_codes": [0] },
    });
    let done = shim.run(&shim.spec(&node, &["-e", script], extra));
    assert_eq!(done.code, Some(0), "{}", done.stderr);
    assert_eq!(states(&done.events, 0), ["starting", "online", "stopped"]);
    assert!(done.elapsed < Duration::from_secs(5), "{:?}", done.elapsed);
}

#[test]
fn node_without_ready_message_fails_readiness() {
    let Some(node) = node() else { return };
    let shim = Shim::new();
    let extra = json!({
        "node_ipc": true,
        "policy": { "ready_timeout_ms": 500, "max_restarts": 0, "kill_timeout_ms": 500 },
    });
    let done = shim.run(&shim.spec(&node, &["-e", "setInterval(function(){}, 1000)"], extra));
    assert_eq!(done.code, Some(1), "{}", done.stderr);
    assert_eq!(states(&done.events, 0), ["starting", "stopping", "errored"]);
}

#[test]
fn node_with_ipc_still_exits_when_its_work_is_done() {
    let Some(node) = node() else { return };
    let shim = Shim::new();
    let extra = json!({ "node_ipc": true, "policy": { "autorestart": false } });
    let done = shim.run(&shim.spec(&node, &["-e", "console.log('done')"], extra));
    assert_eq!(done.code, Some(0), "{}", done.stderr);
    assert_eq!(shim.out_log(), "done\n");
    assert!(done.elapsed < Duration::from_secs(5), "{:?}", done.elapsed);
}

// -------------------------------------------------------- leftover processes

fn read_pid(shim: &Shim, file: &str) -> u32 {
    let path = shim.path(file);
    let deadline = Instant::now() + TIMEOUT;
    loop {
        if let Some(pid) = std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| s.trim().parse().ok())
        {
            return pid;
        }
        assert!(Instant::now() < deadline, "no pid in {file}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn alive(pid: u32) -> bool {
    Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// A killed process may linger briefly as a zombie until its new parent
/// reaps it.
fn assert_dies(pid: u32) {
    let deadline = Instant::now() + Duration::from_secs(3);
    while alive(pid) {
        assert!(Instant::now() < deadline, "process {pid} is still alive");
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn cleanup_signals(events: &[Value]) -> Vec<String> {
    events
        .iter()
        .filter(|e| e["type"] == "group_cleanup")
        .map(|e| e["signal"].as_str().unwrap_or_default().to_owned())
        .collect()
}

#[test]
fn crash_cleans_up_processes_left_in_the_group() {
    let shim = Shim::new();
    let script = "sleep 300 & echo $! > bg.pid; exit 1";
    let done = shim.run(&shim.spec("/bin/sh", &["-c", script], ONCE()));
    assert_eq!(done.code, Some(0), "{}", done.stderr);
    assert_dies(read_pid(&shim, "bg.pid"));
    assert_eq!(cleanup_signals(&done.events), ["SIGTERM"]);
}

#[test]
fn stop_kills_group_members_that_ignore_sigterm() {
    let shim = Shim::new();
    let script = r#"sh -c 'trap "" TERM; echo $$ > bg.pid; while :; do sleep 0.05; done' & echo started; wait"#;
    let extra = json!({ "policy": { "kill_timeout_ms": 300 } });
    let child = shim.start(&shim.spec("/bin/sh", &["-c", script], extra));
    let started = Instant::now();
    shim.wait_for_output("started");
    let bg = read_pid(&shim, "bg.pid");
    kill(child.id(), "TERM");
    let done = finish(child, started);
    assert_eq!(done.code, Some(0), "{}", done.stderr);
    assert_dies(bg);
    assert_eq!(cleanup_signals(&done.events), ["SIGKILL"]);
}

#[test]
fn treekill_false_leaves_group_members_alone() {
    let shim = Shim::new();
    let script = "sleep 300 & echo $! > bg.pid; exit 1";
    let extra = json!({ "treekill": false, "policy": { "autorestart": false } });
    let done = shim.run(&shim.spec("/bin/sh", &["-c", script], extra));
    assert_eq!(done.code, Some(0), "{}", done.stderr);
    let bg = read_pid(&shim, "bg.pid");
    let survived = alive(bg);
    kill(bg, "KILL");
    assert!(survived, "treekill: false must not touch other processes");
    assert!(cleanup_signals(&done.events).is_empty());
}

#[cfg(target_os = "linux")]
mod linux {
    use super::*;

    fn zombie_children(parent: u32) -> Vec<u32> {
        let mut out = vec![];
        for entry in std::fs::read_dir("/proc").expect("/proc").flatten() {
            let Ok(stat) = std::fs::read_to_string(entry.path().join("stat")) else {
                continue;
            };
            let Some(close) = stat.rfind(')') else {
                continue;
            };
            let mut f = stat[close + 1..].split_whitespace();
            let (Some(state), Some(ppid)) = (f.next(), f.next()) else {
                continue;
            };
            if state == "Z" && ppid == parent.to_string() {
                out.push(entry.file_name().to_string_lossy().parse().unwrap_or(0));
            }
        }
        out
    }

    #[test]
    fn escaped_descendants_are_adopted_reported_and_stopped() {
        if which("setsid").is_none() {
            eprintln!("skipping: setsid not found");
            return;
        }
        let shim = Shim::new();
        let script = r#"(setsid sh -c 'echo $$ > esc.pid; exec sleep 300' &); echo started; while :; do sleep 0.05; done"#;
        let child = shim.start(&shim.spec("/bin/sh", &["-c", script], json!({})));
        let started = Instant::now();
        shim.wait_for_output("started");
        let esc = read_pid(&shim, "esc.pid");
        // The orphan sweep runs at least once a second.
        std::thread::sleep(Duration::from_millis(1500));
        kill(child.id(), "TERM");
        let done = finish(child, started);
        assert_eq!(done.code, Some(0), "{}", done.stderr);
        assert_dies(esc);
        assert!(
            done.events
                .iter()
                .any(|e| e["type"] == "orphan" && e["pid"] == esc),
            "orphan event for {esc}: {:?}",
            done.events
        );
    }

    #[test]
    fn exited_orphans_are_reaped() {
        let shim = Shim::new();
        let script = "(sleep 0.2 &); (sleep 0.2 &); echo started; while :; do sleep 0.05; done";
        let child = shim.start(&shim.spec("/bin/sh", &["-c", script], json!({})));
        let started = Instant::now();
        shim.wait_for_output("started");
        std::thread::sleep(Duration::from_millis(1500));
        let zombies = zombie_children(child.id());
        kill(child.id(), "TERM");
        let done = finish(child, started);
        assert_eq!(done.code, Some(0), "{}", done.stderr);
        assert!(zombies.is_empty(), "unreaped zombies: {zombies:?}");
    }
}
