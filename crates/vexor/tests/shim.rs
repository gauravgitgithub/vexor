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
    // max_restarts 3: errored after 3 runs in a row (as PM2 7).
    let extra = json!({ "instances": 2, "policy": { "max_restarts": 3 } });
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
fn sigterm_stops_the_app_gracefully_with_sigint() {
    // The shim gets SIGTERM; the app gets the stop signal, SIGINT by default
    // (as PM2).
    let shim = Shim::new();
    let script = r#"trap 'echo got-int; exit 0' INT; trap 'echo got-term; exit 0' TERM; echo started; while :; do sleep 0.05; done"#;
    let child = shim.start(&shim.spec("/bin/sh", &["-c", script], json!({})));
    let started = Instant::now();
    shim.wait_for_output("started");
    kill(child.id(), "TERM");
    let done = finish(child, started);
    assert_eq!(done.code, Some(0), "{}", done.stderr);
    assert!(shim.out_log().contains("got-int"), "{}", shim.out_log());
    assert_eq!(
        states(&done.events, 0),
        ["starting", "online", "stopping", "stopped"]
    );
}

#[test]
fn kill_signal_setting_is_honoured() {
    let shim = Shim::new();
    let script = r#"trap 'echo got-int; exit 0' INT; trap 'echo got-term; exit 0' TERM; echo started; while :; do sleep 0.05; done"#;
    let extra = json!({ "policy": { "kill_signal": "SIGTERM" } });
    let child = shim.start(&shim.spec("/bin/sh", &["-c", script], extra));
    let started = Instant::now();
    shim.wait_for_output("started");
    kill(child.id(), "TERM");
    let done = finish(child, started);
    assert_eq!(done.code, Some(0), "{}", done.stderr);
    assert!(shim.out_log().contains("got-term"), "{}", shim.out_log());
}

#[test]
fn app_ignoring_sigterm_is_killed_after_kill_timeout() {
    let shim = Shim::new();
    let script = r#"trap '' INT TERM; echo started; while :; do sleep 0.05; done"#;
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

// ---------------------------------------------------------------- log files

fn rss_kib(pid: u32) -> Option<u64> {
    let out = Command::new("ps")
        .args(["-o", "rss=", "-p", &pid.to_string()])
        .output()
        .ok()?;
    String::from_utf8(out.stdout).ok()?.trim().parse().ok()
}

fn log_files(shim: &Shim, base: &str) -> Vec<(PathBuf, u64)> {
    let mut files: Vec<(PathBuf, u64)> = std::fs::read_dir(shim.dir.path())
        .expect("dir")
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().starts_with(base))
        .map(|e| (e.path(), e.metadata().map(|m| m.len()).unwrap_or(0)))
        .collect();
    files.sort();
    files
}

#[test]
fn one_gigabyte_flood_stays_under_the_cap_with_bounded_memory() {
    let shim = Shim::new();
    let max: u64 = 1024 * 1024;
    let extra = json!({
        "policy": { "autorestart": false },
        "logs": {
            "out": shim.path("out.log"),
            "err": shim.path("err.log"),
            "max_size_bytes": max,
            "retain": 2,
        },
    });
    let spec = shim.spec("/bin/sh", &["-c", "head -c 1000000000 /dev/zero"], extra);
    let child = shim.start(&spec);
    let pid = child.id();
    let started = Instant::now();
    let sampler = std::thread::spawn(move || {
        let mut peak = 0;
        while let Some(kib) = rss_kib(pid) {
            peak = peak.max(kib);
            std::thread::sleep(Duration::from_millis(20));
        }
        peak
    });
    let done = finish(child, started);
    let peak_kib = sampler.join().expect("sampler");
    assert_eq!(done.code, Some(0), "{}", done.stderr);

    let files = log_files(&shim, "out.log");
    let total: u64 = files.iter().map(|f| f.1).sum();
    eprintln!(
        "1 GB flood: {:?}, shim peak RSS {} KiB, {} log files, {} bytes on disk",
        done.elapsed,
        peak_kib,
        files.len(),
        total
    );
    // The app wrote the whole gigabyte: a pipe that stopped being read would
    // have killed it with SIGPIPE (13) instead of exit code 0.
    let last = done
        .events
        .iter()
        .rfind(|e| e["type"] == "state")
        .expect("state");
    assert_eq!(last["exit"]["code"], 0, "{last}");
    assert!(files.len() <= 3, "{files:?}");
    // Each file is capped at `max`, except that one chunk may straddle.
    assert!(
        total <= 3 * (max + 8192),
        "{total} bytes on disk: {files:?}"
    );
    assert!(peak_kib < 40 * 1024, "shim peak RSS {peak_kib} KiB");
}

#[test]
fn block_mode_rotation_keeps_every_line_in_order() {
    let shim = Shim::new();
    let extra = json!({
        "policy": { "autorestart": false },
        "logs": {
            "out": shim.path("out.log"),
            "err": shim.path("err.log"),
            "max_size_bytes": 65536,
            "retain": 1000,
            "overflow": "block",
        },
    });
    let done = shim.run(&shim.spec("/usr/bin/seq", &["1", "200000"], extra));
    assert_eq!(done.code, Some(0), "{}", done.stderr);
    // Oldest is out.log.N with the highest N; newest is out.log.
    let mut files = log_files(&shim, "out.log");
    files.sort_by_key(|(p, _)| {
        std::cmp::Reverse(
            p.extension()
                .and_then(|e| e.to_str())
                .and_then(|e| e.parse::<u32>().ok())
                .unwrap_or(0),
        )
    });
    assert!(files.len() > 10, "expected rotation: {files:?}");
    let mut text = String::new();
    for (p, _) in &files {
        text.push_str(&std::fs::read_to_string(p).expect("read"));
    }
    let numbers: Vec<u32> = text.lines().map(|l| l.parse().expect("number")).collect();
    let expected: Vec<u32> = (1..=200_000).collect();
    assert!(
        numbers == expected,
        "lines lost, duplicated or out of order"
    );
    assert!(
        !done.events.iter().any(|e| e["type"] == "log_dropped"),
        "block mode must not drop"
    );
}

// ------------------------------------------------------------- cluster mode

fn cluster_port() -> u16 {
    use std::sync::atomic::{AtomicU16, Ordering};
    // Outside the ephemeral range (see daemon.rs::free_port).
    static NEXT: AtomicU16 = AtomicU16::new(0);
    let base = 20_000 + u16::try_from(std::process::id() % 500).unwrap_or(0) * 20;
    loop {
        let offset = NEXT.fetch_add(1, Ordering::Relaxed);
        let port = 20_000 + (base - 20_000 + offset) % 10_000;
        if std::net::TcpListener::bind(("127.0.0.1", port)).is_ok() {
            return port;
        }
        assert!(offset < 10_000, "no free port");
    }
}

/// `<pid> <instance>` from a worker, or None if nothing answers.
fn ask(port: u16) -> Option<(u32, String)> {
    use std::io::{Read, Write};
    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    let mut s = std::net::TcpStream::connect_timeout(&addr, Duration::from_secs(1)).ok()?;
    s.set_read_timeout(Some(Duration::from_secs(2))).ok()?;
    s.write_all(b"GET / HTTP/1.0\r\n\r\n").ok()?;
    let mut resp = String::new();
    s.read_to_string(&mut resp).ok()?;
    let body = resp.split("\r\n\r\n").nth(1)?;
    let mut it = body.split(' ');
    Some((it.next()?.parse().ok()?, it.next()?.to_owned()))
}

/// Distinct workers seen over `n` requests: pid -> instance.
fn workers(port: u16, n: usize) -> std::collections::BTreeMap<u32, String> {
    (0..n).filter_map(|_| ask(port)).collect()
}

fn wait_workers(port: u16, count: usize) -> std::collections::BTreeMap<u32, String> {
    let deadline = Instant::now() + TIMEOUT;
    loop {
        let w = workers(port, 40);
        if w.len() == count {
            return w;
        }
        assert!(
            Instant::now() < deadline,
            "expected {count} workers, saw {w:?}"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}

const CLUSTER_APP: &str = r"
var port = +process.env.PORT;
['SIGINT', 'SIGTERM'].forEach(function (sig) { process.on(sig, function () { console.log('term ' + process.env.NODE_APP_INSTANCE); process.exit(0); }); });
require('http').createServer(function (q, r) { r.setHeader('Connection', 'close'); r.end(process.pid + ' ' + process.env.NODE_APP_INSTANCE); })
  .listen(port, '127.0.0.1', function () { process.send('ready'); });
";

fn cluster_spec(shim: &Shim, node: &str, port: u16, extra: Value) -> PathBuf {
    std::fs::write(shim.path("srv.js"), CLUSTER_APP).expect("write app");
    let mut base = json!({
        "instances": 3,
        "node_ipc": true,
        "env": { "PATH": "/usr/bin:/bin", "PORT": port.to_string() },
        "policy": { "ready_timeout_ms": 10000 },
        "cluster": { "script": shim.path("srv.js") },
    });
    merge(&mut base, extra);
    shim.spec(node, &[], base)
}

#[test]
fn cluster_workers_share_a_port_and_restart_individually() {
    let Some(node) = node() else { return };
    let shim = Shim::new();
    let port = cluster_port();
    let child = shim.start(&cluster_spec(&shim, &node, port, json!({})));
    let started = Instant::now();
    let before = wait_workers(port, 3);
    let instances: std::collections::BTreeSet<_> = before.values().cloned().collect();
    assert_eq!(instances.len(), 3, "instances 0, 1, 2: {before:?}");

    // Kill one worker: only it is replaced, with the same instance number.
    let (&victim, victim_inst) = before.iter().next().expect("a worker");
    kill(victim, "KILL");
    let deadline = Instant::now() + TIMEOUT;
    let after = loop {
        let w = workers(port, 40);
        if w.len() == 3 && !w.contains_key(&victim) {
            break w;
        }
        assert!(Instant::now() < deadline, "worker not replaced: {w:?}");
        std::thread::sleep(Duration::from_millis(100));
    };
    for (pid, inst) in &before {
        if *pid != victim {
            assert_eq!(
                after.get(pid),
                Some(inst),
                "other workers must keep running"
            );
        }
    }
    assert!(after.values().any(|i| i == victim_inst));

    kill(child.id(), "TERM");
    let done = finish(child, started);
    assert_eq!(done.code, Some(0), "{}", done.stderr);
    assert!(ask(port).is_none(), "port must be closed after stop");
    let out = shim.out_log();
    for i in 0..3 {
        assert!(
            out.contains(&format!("term {i}")),
            "graceful stop of {i}: {out}"
        );
    }
}

#[test]
fn cluster_primary_death_restarts_every_worker() {
    let Some(node) = node() else { return };
    let shim = Shim::new();
    let port = cluster_port();
    let child = shim.start(&cluster_spec(&shim, &node, port, json!({})));
    let started = Instant::now();
    let before = wait_workers(port, 3);
    let worker = *before.keys().next().expect("worker");
    let ppid = Command::new("ps")
        .args(["-o", "ppid=", "-p", &worker.to_string()])
        .output()
        .expect("ps");
    let primary: u32 = String::from_utf8_lossy(&ppid.stdout)
        .trim()
        .parse()
        .expect("ppid");
    kill(primary, "KILL");
    let deadline = Instant::now() + TIMEOUT;
    loop {
        let w = workers(port, 40);
        if w.len() == 3 && w.keys().all(|p| !before.contains_key(p)) {
            break;
        }
        assert!(Instant::now() < deadline, "workers not restarted: {w:?}");
        std::thread::sleep(Duration::from_millis(100));
    }
    kill(child.id(), "TERM");
    let done = finish(child, started);
    assert_eq!(done.code, Some(0), "{}", done.stderr);
    for e in done
        .events
        .iter()
        .filter(|e| e["type"] == "state" && e["state"] == "online")
    {
        assert!(e["pid"].as_u64().is_some(), "{e}");
    }
}

#[test]
fn cluster_worker_without_ready_fails_readiness() {
    let Some(node) = node() else { return };
    let shim = Shim::new();
    std::fs::write(
        shim.path("quiet.js"),
        "setInterval(function () {}, 1000);\n",
    )
    .expect("write");
    let spec = shim.spec(
        &node,
        &[],
        json!({
            "instances": 2,
            "node_ipc": true,
            "policy": { "ready_timeout_ms": 500, "max_restarts": 0, "kill_timeout_ms": 500 },
            "cluster": { "script": shim.path("quiet.js") },
        }),
    );
    let done = shim.run(&spec);
    assert_eq!(done.code, Some(1), "{}", done.stderr);
    for i in 0..2 {
        assert_eq!(states(&done.events, i), ["starting", "stopping", "errored"]);
    }
}

#[test]
fn max_memory_restart_restarts_an_instance_over_the_limit() {
    let shim = Shim::new();
    // The shell holds a 40 MB string, so its resident memory passes 20 MiB.
    let script = r"echo started; x=$(head -c 40000000 /dev/zero | tr '\0' a); echo grown; while :; do sleep 0.05; done";
    let extra = json!({
        "policy": { "max_memory_bytes": 20 << 20, "memory_check_interval_ms": 200 }
    });
    let child = shim.start(&shim.spec("/bin/sh", &["-c", script], extra));
    let started = Instant::now();
    let deadline = started + TIMEOUT;
    while shim.out_log().matches("started").count() < 3 {
        assert!(
            Instant::now() < deadline,
            "no memory restarts: {}",
            shim.out_log()
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    kill(child.id(), "TERM");
    let done = finish(child, started);
    let restarts: Vec<&Value> = done
        .events
        .iter()
        .filter(|e| e["type"] == "memory_restart")
        .collect();
    assert!(restarts.len() >= 2, "{:?}", done.events);
    for r in &restarts {
        assert_eq!(r["limit_bytes"], 20 << 20);
        assert!(r["rss_bytes"].as_u64().expect("rss") > 20 << 20, "{r}");
    }
    // A memory restart is a restart, not a crash: no crash-loop error.
    assert!(
        done.events.iter().all(|e| e["error"].is_null()),
        "{:?}",
        done.events
    );
}

#[test]
fn max_memory_restart_leaves_an_app_under_the_limit_alone() {
    let shim = Shim::new();
    let extra = json!({
        "policy": { "max_memory_bytes": 1u64 << 30, "memory_check_interval_ms": 100 }
    });
    let child = shim.start(&shim.spec(
        "/bin/sh",
        &["-c", "echo started; while :; do sleep 0.05; done"],
        extra,
    ));
    let started = Instant::now();
    shim.wait_for_output("started");
    std::thread::sleep(Duration::from_millis(1000));
    kill(child.id(), "TERM");
    let done = finish(child, started);
    assert_eq!(shim.out_log().matches("started").count(), 1);
    assert!(done.events.iter().all(|e| e["type"] != "memory_restart"));
}

#[test]
fn max_memory_restart_checks_each_cluster_worker() {
    let Some(node) = node() else { return };
    let shim = Shim::new();
    // Only instance 1 grows past the limit.
    let app = "
var big = process.env.NODE_APP_INSTANCE === '1' ? Buffer.alloc(150 * 1024 * 1024, 1) : null;
console.log('worker ' + process.env.NODE_APP_INSTANCE + ' ' + (big ? 'big' : 'small'));
process.send('ready');
setInterval(function () {}, 1000);
";
    std::fs::write(shim.path("mem.js"), app).expect("write app");
    let spec = shim.spec(
        &node,
        &[],
        json!({
            "instances": 2,
            "node_ipc": true,
            "policy": {
                "ready_timeout_ms": 10000,
                "max_memory_bytes": 120u64 << 20,
                "memory_check_interval_ms": 200
            },
            "cluster": { "script": shim.path("mem.js") },
        }),
    );
    let child = shim.start(&spec);
    let started = Instant::now();
    let deadline = started + TIMEOUT;
    while shim.out_log().matches("worker 1 big").count() < 2 {
        assert!(Instant::now() < deadline, "{}", shim.out_log());
        std::thread::sleep(Duration::from_millis(50));
    }
    kill(child.id(), "TERM");
    let done = finish(child, started);
    let restarted: std::collections::BTreeSet<u64> = done
        .events
        .iter()
        .filter(|e| e["type"] == "memory_restart")
        .filter_map(|e| e["instance"].as_u64())
        .collect();
    assert_eq!(restarted, [1].into(), "{:?}", done.events);
    assert_eq!(
        shim.out_log().matches("worker 0 small").count(),
        1,
        "instance 0 untouched"
    );
}
