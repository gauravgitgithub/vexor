//! Daemon ↔ shim tests, including the zero-downtime proof from
//! docs/plan.md step 2.4b: kill -9 the daemon 100 times while an HTTP app is
//! under constant load.

use std::collections::BTreeSet;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tempfile::TempDir;

const TIMEOUT: Duration = Duration::from_secs(15);

/// A Vexor home with one app, plus cleanup of every process the test starts.
struct Home {
    dir: TempDir,
    daemon: Option<Child>,
    /// Specs added before the first daemon start, submitted once it is up.
    pending: Vec<PathBuf>,
}

impl Home {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(dir.path().join("specs")).expect("specs dir");
        Home {
            dir,
            daemon: None,
            pending: Vec::new(),
        }
    }

    fn path(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
    }

    /// Write an app spec; it is submitted with `vexor start --spec` when the
    /// daemon starts.
    fn add_app(&mut self, name: &str, exec: &str, args: &[&str], extra: Value) {
        let mut spec = json!({
            "name": name,
            "exec": exec,
            "args": args,
            "cwd": self.dir.path(),
            "env": { "PATH": "/usr/bin:/bin" },
            "logs": {
                "timestamps": false,
                "out": self.path(&format!("{name}.out")),
                "err": self.path(&format!("{name}.err")),
            },
        });
        if let (Value::Object(b), Value::Object(e)) = (&mut spec, extra) {
            b.extend(e);
        }
        let path = self.path(&format!("specs/{name}.json"));
        std::fs::write(&path, spec.to_string()).expect("write spec");
        self.pending.push(path);
    }

    fn cli(&self, args: &[&str]) -> std::process::Output {
        Command::new(env!("CARGO_BIN_EXE_vexor"))
            .arg("--home")
            .arg(self.dir.path())
            .args(args)
            .output()
            .expect("run vexor")
    }

    fn spawn_daemon(&self) -> Child {
        Command::new(env!("CARGO_BIN_EXE_vexor"))
            .arg("--home")
            .arg(self.dir.path())
            .arg("daemon")
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("start daemon")
    }

    fn start_daemon(&mut self) {
        assert!(self.daemon.is_none(), "daemon already running");
        let started = self.count("daemon_started");
        self.daemon = Some(self.spawn_daemon());
        // Use the CLI only once this daemon is serving; otherwise the CLI
        // would start a daemon of its own.
        self.wait_count("daemon_started", started + 1);
        for spec in std::mem::take(&mut self.pending) {
            let out = self.cli(&["start", "--spec", &spec.to_string_lossy()]);
            assert!(
                out.status.success(),
                "vexor start --spec failed: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
    }

    fn kill_daemon(&mut self) {
        if let Some(mut d) = self.daemon.take() {
            let _ = d.kill(); // SIGKILL
            let _ = d.wait();
        }
    }

    fn log(&self) -> Vec<Value> {
        std::fs::read_to_string(self.path("daemon.log"))
            .unwrap_or_default()
            .lines()
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect()
    }

    fn count(&self, kind: &str) -> usize {
        self.log().iter().filter(|e| e["type"] == kind).count()
    }

    fn wait_count(&self, kind: &str, n: usize) {
        let deadline = Instant::now() + TIMEOUT;
        while self.count(kind) < n {
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {n} {kind:?} entries; log: {:?}",
                self.log()
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// The shim's event log and the app's stderr, for failure messages.
    fn diagnostics(&self, app: &str) -> String {
        let read = |p: &str| std::fs::read_to_string(self.path(p)).unwrap_or_default();
        format!(
            "--- shims/{app}.log ---\n{}\n--- {app}.err ---\n{}",
            read(&format!("shims/{app}.log")),
            read(&format!("{app}.err"))
        )
    }

    fn shim_pids(&self) -> BTreeSet<u64> {
        self.log()
            .iter()
            .filter(|e| e["type"] == "shim_connected")
            .filter_map(|e| e["shim_pid"].as_u64())
            .collect()
    }
}

impl Drop for Home {
    fn drop(&mut self) {
        self.kill_daemon();
        // Stop every shim this home started (they stop their apps).
        for pid in self.shim_pids() {
            let _ = Command::new("kill")
                .args(["-TERM", &pid.to_string()])
                .status();
        }
        let deadline = Instant::now() + Duration::from_secs(5);
        while self.shim_pids().iter().any(|p| alive(*p)) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

fn alive(pid: u64) -> bool {
    Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

fn which(program: &str) -> Option<String> {
    let out = Command::new("/bin/sh")
        .args(["-c", &format!("command -v {program}")])
        .output()
        .ok()?;
    let path = String::from_utf8(out.stdout).ok()?.trim().to_owned();
    path.starts_with('/').then_some(path)
}

/// A port for a test server, outside the OS ephemeral range (Linux 32768+,
/// macOS 49152+). Ports from `bind(0)` come from that range, which outgoing
/// connections also use: while a test app is down, another test's client
/// connection could take its port and the restart would fail with
/// EADDRINUSE. Seen once on a CI x64 runner.
fn free_port() -> u16 {
    use std::sync::atomic::{AtomicU16, Ordering};
    // Spread concurrent test processes apart; tests within one process take
    // successive ports.
    static NEXT: AtomicU16 = AtomicU16::new(0);
    let base = 20_000 + u16::try_from(std::process::id() % 500).unwrap_or(0) * 20;
    loop {
        let offset = NEXT.fetch_add(1, Ordering::Relaxed);
        let port = 20_000 + (base - 20_000 + offset) % 10_000;
        if std::net::TcpListener::bind(("127.0.0.1", port)).is_ok() {
            return port;
        }
        assert!(offset < 10_000, "no free port in 20000-29999");
    }
}

/// One HTTP/1.0 GET; the body on success.
fn get(addr: SocketAddr, path: &str) -> Result<String, String> {
    let mut s =
        TcpStream::connect_timeout(&addr, Duration::from_secs(2)).map_err(|e| e.to_string())?;
    s.set_read_timeout(Some(Duration::from_secs(2)))
        .map_err(|e| e.to_string())?;
    write!(s, "GET {path} HTTP/1.0\r\nHost: t\r\n\r\n").map_err(|e| e.to_string())?;
    let mut resp = String::new();
    s.read_to_string(&mut resp).map_err(|e| e.to_string())?;
    if !resp.starts_with("HTTP/1.1 200") {
        return Err(format!("bad response: {resp:?}"));
    }
    resp.split("\r\n\r\n")
        .nth(1)
        .map(str::to_owned)
        .ok_or_else(|| "no body".to_owned())
}

/// HTTP app: logs `req N` per request, answers `<pid> <N>`, exits 1 on /crash.
const SERVER: &str = r"
var http = require('http'); var n = 0;
http.createServer(function (req, res) {
  if (req.url === '/crash') { res.end('bye'); setTimeout(function () { process.exit(1) }, 10); return; }
  n++; console.log('req ' + n); res.end(process.pid + ' ' + n);
}).listen(+process.env.PORT, '127.0.0.1', function () { process.send('ready') });
";

fn server_pid(body: &str) -> u64 {
    body.split(' ')
        .next()
        .and_then(|p| p.parse().ok())
        .expect("pid in body")
}

/// Successful requests, failure messages, distinct server pids seen.
type LoadResults = (u64, Vec<String>, BTreeSet<u64>);

/// Load generator: requests back to back until stopped.
struct Load {
    stop: Arc<AtomicBool>,
    results: Arc<Mutex<LoadResults>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Load {
    fn start(addr: SocketAddr) -> Load {
        let stop = Arc::new(AtomicBool::new(false));
        let results = Arc::new(Mutex::new((0u64, Vec::new(), BTreeSet::new())));
        let (s, r) = (stop.clone(), results.clone());
        let thread = std::thread::spawn(move || {
            while !s.load(Ordering::Relaxed) {
                let outcome = get(addr, "/");
                let mut r = r.lock().expect("lock");
                match outcome {
                    Ok(body) => {
                        r.0 += 1;
                        r.2.insert(server_pid(&body));
                    }
                    Err(e) => r.1.push(e),
                }
                drop(r);
                std::thread::sleep(Duration::from_millis(2));
            }
        });
        Load {
            stop,
            results,
            thread: Some(thread),
        }
    }

    /// (successful requests, failures, distinct server pids)
    fn finish(mut self) -> LoadResults {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            t.join().expect("load thread");
        }
        let r = self.results.lock().expect("lock");
        (r.0, r.1.clone(), r.2.clone())
    }
}

fn wait_http(addr: SocketAddr) -> String {
    let deadline = Instant::now() + TIMEOUT;
    loop {
        if let Ok(body) = get(addr, "/") {
            return body;
        }
        assert!(Instant::now() < deadline, "server never answered");
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn req_numbers(log: &Path) -> Vec<u64> {
    std::fs::read_to_string(log)
        .unwrap_or_default()
        .lines()
        .filter_map(|l| l.strip_prefix("req ")?.parse().ok())
        .collect()
}

// ---------------------------------------------------------------------------

#[test]
fn zero_downtime_across_100_daemon_kills() {
    let Some(node) = which("node") else {
        eprintln!("skipping: node not found on PATH");
        return;
    };
    let mut home = Home::new();
    let port = free_port();
    let addr: SocketAddr = ([127, 0, 0, 1], port).into();
    home.add_app(
        "web",
        &node,
        &["-e", SERVER],
        json!({
            "env": { "PORT": port.to_string() },
            "node_ipc": true,
            "policy": { "ready_timeout_ms": 10000 },
        }),
    );
    home.start_daemon();
    home.wait_count("shim_connected", 1);
    let first = wait_http(addr);
    let app_pid = server_pid(&first);

    let load = Load::start(addr);
    let began = Instant::now();
    for i in 0..100 {
        home.kill_daemon();
        home.start_daemon();
        home.wait_count("shim_connected", i + 2);
    }
    // Keep the load running a moment after the last reconnect.
    std::thread::sleep(Duration::from_millis(200));
    let (ok, failures, pids) = load.finish();
    eprintln!(
        "100 daemon kills in {:?}: {ok} requests ok, {} failed",
        began.elapsed(),
        failures.len()
    );

    assert!(
        failures.is_empty(),
        "{} failed requests: {:?}",
        failures.len(),
        &failures[..failures.len().min(5)]
    );
    assert!(ok > 100, "only {ok} requests completed");
    assert_eq!(pids, BTreeSet::from([app_pid]), "the app was restarted");
    assert_eq!(home.count("shim_launched"), 1, "a second shim was launched");
    assert_eq!(home.shim_pids().len(), 1, "daemons reached different shims");

    // Log continuity: every request logged exactly once, in order, no gaps.
    let reqs = req_numbers(&home.path("web.out"));
    let expected: Vec<u64> = (1..=ok + 1).collect(); // +1: the first wait_http
    assert_eq!(reqs, expected, "app log has gaps or duplicates");
}

#[test]
fn shim_restarts_crashed_app_while_daemon_is_down() {
    let Some(node) = which("node") else {
        eprintln!("skipping: node not found on PATH");
        return;
    };
    let mut home = Home::new();
    let port = free_port();
    let addr: SocketAddr = ([127, 0, 0, 1], port).into();
    home.add_app(
        "web",
        &node,
        &["-e", SERVER],
        json!({
            "env": { "PORT": port.to_string() },
            "node_ipc": true,
            "policy": { "ready_timeout_ms": 10000 },
        }),
    );
    home.start_daemon();
    home.wait_count("shim_connected", 1);
    let before = server_pid(&wait_http(addr));

    home.kill_daemon();
    let _ = get(addr, "/crash");
    let deadline = Instant::now() + TIMEOUT;
    let after = loop {
        if let Ok(body) = get(addr, "/") {
            let pid = server_pid(&body);
            if pid != before {
                break pid;
            }
        }
        assert!(Instant::now() < deadline, "app was not restarted");
        std::thread::sleep(Duration::from_millis(20));
    };
    assert_ne!(after, before);

    // A new daemon sees the restart in the shim's snapshot.
    home.start_daemon();
    home.wait_count("shim_connected", 2);
    let connected = home
        .log()
        .into_iter()
        .rfind(|e| e["type"] == "shim_connected")
        .expect("connected");
    let snap = &connected["snapshot"][0];
    let why = home.diagnostics("web");
    assert_eq!(snap["state"], "online", "{connected}\n{why}");
    assert_eq!(snap["restarts"], 1, "{connected}\n{why}");
    assert_eq!(snap["pid"], after, "{connected}\n{why}");
}

#[test]
fn events_stream_in_order_without_gaps() {
    let mut home = Home::new();
    // Exits every ~1.2 s and is restarted: a steady stream of state events.
    home.add_app(
        "flap",
        "/bin/sh",
        &["-c", "sleep 1.2; exit 1"],
        json!({ "policy": { "max_restarts": 1000 } }),
    );
    home.start_daemon();
    home.wait_count("shim_connected", 1);
    std::thread::sleep(Duration::from_millis(3000));
    let seqs: Vec<u64> = home
        .log()
        .iter()
        .filter(|e| e["type"] == "event")
        .filter_map(|e| e["seq"].as_u64())
        .collect();
    assert!(seqs.len() >= 4, "too few events: {seqs:?}");
    assert!(
        seqs.windows(2).all(|w| w[1] == w[0] + 1),
        "sequence numbers not contiguous: {seqs:?}"
    );
    assert_eq!(home.count("event_gap"), 0);
}

#[test]
fn only_one_daemon_per_home() {
    let mut home = Home::new();
    home.start_daemon();
    home.wait_count("daemon_started", 1);
    let second = home
        .spawn_daemon()
        .wait_with_output()
        .expect("second daemon");
    assert_eq!(second.status.code(), Some(3));
    assert!(String::from_utf8_lossy(&second.stderr).contains("already running"));
}

#[test]
fn only_one_shim_per_app() {
    let mut home = Home::new();
    home.add_app("solo", "/bin/sh", &["-c", "sleep 300"], json!({}));
    home.start_daemon();
    home.wait_count("shim_connected", 1);
    let out = Command::new(env!("CARGO_BIN_EXE_vexor"))
        .arg("--home")
        .arg(home.dir.path())
        .args(["shim", "--managed", "--spec"])
        .arg(home.path("apps/solo.json"))
        .output()
        .expect("second shim");
    assert_eq!(
        out.status.code(),
        Some(4),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn control_files_are_private() {
    use std::os::unix::fs::PermissionsExt;
    let mut home = Home::new();
    home.add_app("p", "/bin/sh", &["-c", "sleep 300"], json!({}));
    home.start_daemon();
    home.wait_count("shim_connected", 1);
    let mode = |p: &str| {
        std::fs::metadata(home.path(p))
            .expect(p)
            .permissions()
            .mode()
            & 0o777
    };
    assert_eq!(mode(""), 0o700);
    assert_eq!(mode("shims"), 0o700);
    for f in [
        "shims/p.sock",
        "shims/p.lock",
        "shims/p.pids.json",
        "daemon.log",
        "daemon.sock",
        "state.json",
    ] {
        assert_eq!(mode(f), 0o600, "{f}");
    }
}

#[test]
fn no_new_shim_while_a_dead_shims_instances_survive() {
    let mut home = Home::new();
    home.add_app("keep", "/bin/sh", &["-c", "sleep 300"], json!({}));
    home.start_daemon();
    home.wait_count("shim_connected", 1);
    let shim = *home.shim_pids().iter().next().expect("shim pid");
    let pids: Vec<u64> = serde_json::from_str::<Vec<Value>>(
        &std::fs::read_to_string(home.path("shims/keep.pids.json")).expect("pids file"),
    )
    .expect("json")
    .iter()
    .filter_map(|i| i["pid"].as_u64())
    .collect();
    assert_eq!(pids.len(), 1);

    // Simulate a shim crash: its app keeps running.
    home.kill_daemon();
    let _ = Command::new("kill")
        .args(["-KILL", &shim.to_string()])
        .status();
    home.start_daemon();
    home.wait_count("orphaned_instances", 1);
    assert_eq!(
        home.count("shim_launched"),
        1,
        "must not launch a duplicate"
    );
    assert!(alive(pids[0]));
    let _ = Command::new("kill")
        .args(["-KILL", &pids[0].to_string()])
        .status();
}

// ------------------------------------------------------------------ reload

/// The SERVER app, except that it exits at startup while `broken` exists in
/// its working directory (a bad deploy).
const RELOADABLE: &str = r"
if (require('fs').existsSync('broken')) { console.error('bad deploy'); process.exit(1); }
var http = require('http');
http.createServer(function (req, res) { res.setHeader('Connection', 'close'); res.end(process.pid + ' 0'); })
  .listen(+process.env.PORT, '127.0.0.1', function () { process.send('ready') });
";

fn reloadable_app(home: &mut Home, node: &str, port: u16, cluster: bool, extra_policy: Value) {
    let script = home.path("app.js");
    std::fs::write(&script, RELOADABLE).expect("write app");
    let mut policy = json!({ "ready_timeout_ms": 10000, "kill_timeout_ms": 3000 });
    if let (Value::Object(p), Value::Object(e)) = (&mut policy, extra_policy) {
        p.extend(e);
    }
    let mut extra = json!({
        "env": { "PORT": port.to_string() },
        "node_ipc": true,
        "policy": policy,
    });
    let args: Vec<String> = if cluster {
        extra["cluster"] = json!({ "script": script });
        vec![]
    } else {
        vec![script.to_string_lossy().into_owned()]
    };
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    home.add_app("web", node, &args, extra);
}

fn restarts(home: &Home) -> u64 {
    let out = home.cli(&["list", "--json"]);
    let apps: Vec<Value> = serde_json::from_slice(&out.stdout).expect("json");
    apps[0]["snapshot"][0]["restarts"]
        .as_u64()
        .expect("restarts")
}

#[test]
fn cluster_reload_under_load_has_zero_failed_requests() {
    let Some(node) = which("node") else {
        eprintln!("skipping: node not found on PATH");
        return;
    };
    let mut home = Home::new();
    let port = free_port();
    let addr: SocketAddr = ([127, 0, 0, 1], port).into();
    reloadable_app(&mut home, &node, port, true, json!({}));
    home.start_daemon();
    let first = server_pid(&wait_http(addr));

    let load = Load::start(addr);
    std::thread::sleep(Duration::from_millis(300));
    for _ in 0..3 {
        let out = home.cli(&["reload", "web"]);
        assert!(
            out.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&out.stderr),
            home.diagnostics("web")
        );
        std::thread::sleep(Duration::from_millis(300));
    }
    let (ok, failures, pids) = load.finish();
    eprintln!(
        "3 cluster reloads under load: {ok} requests ok, {} failed, {} pids",
        failures.len(),
        pids.len()
    );
    assert!(
        failures.is_empty(),
        "{} failed: {:?}\n{}",
        failures.len(),
        &failures[..failures.len().min(5)],
        home.diagnostics("web")
    );
    assert!(ok > 50, "only {ok} requests");
    assert_eq!(
        pids.len(),
        4,
        "the original and three replacements served: {pids:?}"
    );
    assert!(pids.contains(&first));
    assert_eq!(restarts(&home), 3);
    // Old workers were retired: one instance, one process answering.
    std::thread::sleep(Duration::from_millis(3500)); // kill_timeout backstop
    let answering: BTreeSet<u64> = (0..30)
        .filter_map(|_| get(addr, "/").ok())
        .map(|b| server_pid(&b))
        .collect();
    assert_eq!(
        answering.len(),
        1,
        "old workers still serving: {answering:?}"
    );
}

#[test]
fn failed_reload_keeps_the_old_process_and_loses_no_requests() {
    let Some(node) = which("node") else {
        eprintln!("skipping: node not found on PATH");
        return;
    };
    let mut home = Home::new();
    let port = free_port();
    let addr: SocketAddr = ([127, 0, 0, 1], port).into();
    reloadable_app(&mut home, &node, port, true, json!({}));
    home.start_daemon();
    let first = server_pid(&wait_http(addr));

    std::fs::write(home.path("broken"), "").expect("break the deploy");
    let load = Load::start(addr);
    std::thread::sleep(Duration::from_millis(200));
    let out = home.cli(&["reload", "web"]);
    std::thread::sleep(Duration::from_millis(300));
    let (ok, failures, pids) = load.finish();

    assert_eq!(out.status.code(), Some(1), "reload must report failure");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("reload did not complete"), "{stderr}");
    assert!(
        failures.is_empty(),
        "{} failed: {:?}",
        failures.len(),
        &failures[..failures.len().min(5)]
    );
    assert!(ok > 20, "only {ok} requests");
    assert_eq!(pids, BTreeSet::from([first]), "only the old process served");
    assert_eq!(restarts(&home), 0);
    let events = home.cli(&["events", "web", "--json"]);
    let events: Vec<Value> = serde_json::from_slice(&events.stdout).expect("json");
    assert!(
        events
            .iter()
            .any(|e| e["type"] == "reload" && e["outcome"] == "failed"),
        "{events:#?}"
    );
}

#[test]
fn fork_reload_defaults_to_restart() {
    let Some(node) = which("node") else {
        eprintln!("skipping: node not found on PATH");
        return;
    };
    let mut home = Home::new();
    let port = free_port();
    let addr: SocketAddr = ([127, 0, 0, 1], port).into();
    reloadable_app(&mut home, &node, port, false, json!({}));
    home.start_daemon();
    let first = server_pid(&wait_http(addr));
    let out = home.cli(&["reload", "web"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let second = server_pid(&wait_http(addr));
    assert_ne!(first, second);
    assert_eq!(restarts(&home), 1);
}

#[test]
fn fork_start_first_on_a_fixed_port_fails_safely() {
    let Some(node) = which("node") else {
        eprintln!("skipping: node not found on PATH");
        return;
    };
    let mut home = Home::new();
    let port = free_port();
    let addr: SocketAddr = ([127, 0, 0, 1], port).into();
    reloadable_app(
        &mut home,
        &node,
        port,
        false,
        json!({ "reload_strategy": "start_first" }),
    );
    home.start_daemon();
    let first = server_pid(&wait_http(addr));
    let out = home.cli(&["reload", "web"]);
    assert_eq!(
        out.status.code(),
        Some(1),
        "the replacement can't bind the port"
    );
    assert_eq!(
        server_pid(&wait_http(addr)),
        first,
        "the old process keeps serving"
    );
    assert_eq!(restarts(&home), 0);
    let err = std::fs::read_to_string(home.path("web.err")).unwrap_or_default();
    assert!(err.contains("EADDRINUSE"), "{err}");
}

fn metrics(home: &Home) -> Vec<Value> {
    let out = home.cli(&["list", "--json"]);
    let apps: Vec<Value> = serde_json::from_slice(&out.stdout).expect("json");
    apps[0]["metrics"].as_array().cloned().unwrap_or_default()
}

/// Wait for a sample of instance 0 that satisfies `ok`.
fn wait_metrics(home: &Home, ok: impl Fn(&Value) -> bool) -> Value {
    let deadline = Instant::now() + TIMEOUT;
    loop {
        let m = metrics(home);
        if let Some(s) = m.iter().find(|s| s["instance"] == 0 && ok(s)) {
            return s.clone();
        }
        assert!(
            Instant::now() < deadline,
            "no matching sample; latest: {m:?}\n{}",
            home.diagnostics("web")
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Plan step 6b.5: request lines in an app's JSON logs become per-route
/// stats; an app with plain-text logs is reported as having none.
#[test]
fn routes_come_from_json_request_logs() {
    let mut home = Home::new();
    // pino-http shape: every fourth request fails with a 500.
    let json_app = r#"i=0; while :; do
        s=200; [ $((i % 4)) -eq 0 ] && s=500
        printf '{"level":30,"req":{"method":"GET","url":"/users/%d?x=1"},"res":{"statusCode":%d},"responseTime":%d}\n' $i $s 12
        printf '{"level":30,"msg":"tick"}\n'
        i=$((i+1)); sleep 0.02
    done"#;
    home.add_app(
        "web",
        "/bin/sh",
        &["-c", json_app],
        json!({ "policy": { "metrics_interval_ms": 200 } }),
    );
    home.add_app(
        "plain",
        "/bin/sh",
        &["-c", "while :; do echo hello; sleep 0.05; done"],
        json!({ "policy": { "metrics_interval_ms": 200 } }),
    );
    home.start_daemon();

    let deadline = Instant::now() + TIMEOUT;
    let route = loop {
        let out = home.cli(&["routes", "web", "--since", "5m", "--json"]);
        let v: Value = serde_json::from_slice(&out.stdout).expect("json");
        let found = v["routes"]
            .as_array()
            .and_then(|r| r.iter().find(|r| r["route"] == "/users/:id").cloned());
        if let Some(r) = found.filter(|r| r["count"].as_u64() >= Some(8)) {
            assert!(
                v["logs"]["json_lines"].as_u64() > v["logs"]["requests"].as_u64(),
                "{v}"
            );
            assert_eq!(v["resolution_ms"], 60_000, "{v}");
            break r;
        }
        assert!(
            Instant::now() < deadline,
            "routes: {v}\n{}",
            home.diagnostics("web")
        );
        std::thread::sleep(Duration::from_millis(200));
    };
    assert_eq!(route["method"], "GET");
    let (count, errors) = (
        route["count"].as_u64().expect("n"),
        route["server_errors"].as_u64().expect("e"),
    );
    assert!(
        errors * 4 >= count - 4 && errors * 4 <= count + 4,
        "{route}"
    );
    let p95 = route["p95_ms"].as_f64().expect("p95");
    assert!((p95 - 12.0).abs() < 1.5, "{route}");

    let out = home.cli(&["routes", "web", "--since", "5m"]);
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.contains("/users/:id") && text.contains("12ms"),
        "{text}"
    );

    // Plain text: counted, never guessed at.
    let deadline = Instant::now() + TIMEOUT;
    loop {
        let out = home.cli(&["routes", "plain"]);
        let text = String::from_utf8_lossy(&out.stdout).into_owned();
        if text.contains("aren't JSON") {
            break;
        }
        assert!(Instant::now() < deadline, "{text}");
        std::thread::sleep(Duration::from_millis(200));
    }
    let out = home.cli(&["routes", "nope"]);
    assert!(!out.status.success());
}

/// Plan step 6b.1: the shim samples each instance, the daemon serves the
/// latest sample, and after a crash the sample is the new process's.
#[test]
fn metrics_follow_each_instance_and_its_restarts() {
    let Some(node) = which("node") else {
        eprintln!("skipping: node not found on PATH");
        return;
    };
    let mut home = Home::new();
    let port = free_port();
    let addr: SocketAddr = ([127, 0, 0, 1], port).into();
    home.add_app(
        "web",
        &node,
        &["-e", SERVER],
        json!({
            "env": { "PORT": port.to_string() },
            "node_ipc": true,
            "health_check": { "tcp": format!("127.0.0.1:{port}"), "interval_ms": 200, "timeout_ms": 1000 },
            "policy": { "ready_timeout_ms": 10000, "metrics_interval_ms": 200 },
        }),
    );
    home.start_daemon();
    let before = server_pid(&wait_http(addr));
    let s = wait_metrics(&home, |s| {
        s["pid"] == before && s["cpu_pct"].is_number() && s["health"]["ok"] == true
    });
    assert!(s["rss_bytes"].as_u64().expect("rss") > 1 << 20, "{s}");
    // stdin, stdout, stderr and the listening socket at least.
    assert!(s["open_fds"].as_u64().expect("fds") >= 4, "{s}");

    let _ = get(addr, "/crash");
    let deadline = Instant::now() + TIMEOUT;
    let after = loop {
        if let Ok(body) = get(addr, "/") {
            let pid = server_pid(&body);
            if pid != before {
                break pid;
            }
        }
        assert!(Instant::now() < deadline, "app was not restarted");
        std::thread::sleep(Duration::from_millis(20));
    };
    wait_metrics(&home, |s| s["pid"] == after && s["cpu_pct"].is_number());

    // Plan step 6b.2: the history has both processes, oldest first.
    let deadline = Instant::now() + TIMEOUT;
    loop {
        let out = home.cli(&["metrics", "web", "--since", "5m", "--json"]);
        let v: Value = serde_json::from_slice(&out.stdout).expect("json");
        let pids: Vec<u64> = v["points"]
            .as_array()
            .expect("points")
            .iter()
            .filter_map(|p| p["pid"].as_u64())
            .collect();
        let both = pids.first() == Some(&before) && pids.last() == Some(&after);
        if both {
            assert_eq!(v["resolution_ms"], 5000, "{v}");
            break;
        }
        assert!(Instant::now() < deadline, "history: {v}");
        std::thread::sleep(Duration::from_millis(200));
    }
    let out = home.cli(&["metrics", "web", "--since", "5m"]);
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("1 restart"), "{text}");
}

/// A shim started by an older Vexor survives the upgrade and only speaks
/// protocol 1: the daemon says hello again with 1 and follows it, without
/// metrics.
#[test]
fn daemon_falls_back_to_protocol_1_for_an_older_shim() {
    use std::io::BufRead;
    use std::os::unix::net::UnixListener;

    let mut home = Home::new();
    home.add_app("old", "/bin/sh", &["-c", "sleep 30"], json!({}));
    std::fs::create_dir_all(home.path("shims")).expect("shims dir");
    let lock = vexor_shim::try_lock(&home.path("shims/old.lock"))
        .expect("lock")
        .expect("free");
    let listener = UnixListener::bind(home.path("shims/old.sock")).expect("bind");
    // Stands in for the shim's process (the test cleanup signals it).
    let mut stand_in = Command::new("sleep").arg("30").spawn().expect("sleep");
    let pid = stand_in.id();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let s = seen.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut r = std::io::BufReader::new(stream.try_clone().expect("clone"));
            let mut line = String::new();
            let _ = r.read_line(&mut line);
            let hello: Value = serde_json::from_str(&line).unwrap_or_default();
            s.lock().expect("lock").push(hello["protocol"].clone());
            if hello["protocol"] != 1 {
                let refused =
                    json!({ "type": "refused", "reason": "protocol not supported, shim speaks 1" });
                let _ = writeln!(stream, "{refused}");
                continue;
            }
            let welcome = json!({
                "type": "welcome", "protocol": 1, "app": "old", "shim_pid": pid,
                "shim_start": 1, "next_seq": 0,
                "snapshot": [{ "type": "state", "app": "old", "instance": 0,
                               "state": "online", "pid": pid, "start": 1, "restarts": 0 }],
            });
            let _ = writeln!(stream, "{welcome}");
            // Hold the connection until the daemon drops it.
            while r.read_line(&mut line).is_ok_and(|n| n > 0) {}
        }
    });

    home.start_daemon();
    home.wait_count("shim_connected", 1);
    // Steps down one version at a time until the shim accepts.
    assert_eq!(
        *seen.lock().expect("lock"),
        [json!(4), json!(3), json!(2), json!(1)]
    );
    assert_eq!(home.count("shim_refused"), 3);
    let out = home.cli(&["list", "--json"]);
    let apps: Vec<Value> = serde_json::from_slice(&out.stdout).expect("json");
    assert_eq!(apps[0]["snapshot"][0]["state"], "online", "{apps:?}");
    assert!(apps[0].get("metrics").is_none(), "{apps:?}");
    // Scaling needs protocol 3: refused, with what to do instead.
    let out = home.cli(&["scale", "old", "2"]);
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("vexor restart old"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    // Route stats need protocol 4: said so instead of "no data".
    let out = home.cli(&["routes", "old"]);
    assert!(out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("predates route stats"), "{err}");

    home.kill_daemon();
    let _ = stand_in.kill();
    let _ = stand_in.wait();
    drop(lock);
}

fn alerts(home: &Home) -> Value {
    let out = home.cli(&["alerts", "--json"]);
    serde_json::from_slice(&out.stdout).expect("alerts json")
}

fn wait_alerts(home: &Home, what: &str, ok: impl Fn(&Value) -> bool) -> Value {
    // Rules are evaluated every 5 s.
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let a = alerts(home);
        if ok(&a) {
            return a;
        }
        assert!(Instant::now() < deadline, "{what}: {a}");
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// Plan step 6b.3: a crash loop opens a danger incident, which recovers
/// once the app runs again; a broken alerts.json is reported and the
/// previous rules stay in force.
#[test]
fn crash_loop_alert_opens_and_recovers() {
    let mut home = Home::new();
    let marker = home.path("broken");
    std::fs::write(&marker, "").expect("marker");
    let script = format!("[ -f {} ] && exit 3; exec sleep 60", marker.display());
    home.add_app(
        "crashy",
        "/bin/sh",
        &["-c", &script],
        json!({ "policy": { "max_restarts": 2, "min_uptime_ms": 1000 } }),
    );
    home.start_daemon();
    let a = wait_alerts(&home, "crash loop incident", |a| {
        a["open"].as_array().is_some_and(|o| {
            o.iter()
                .any(|i| i["key"]["metric"] == "crash_loop" && i["level"] == "danger")
        })
    });
    assert!(
        a["recent"]
            .as_array()
            .expect("recent")
            .iter()
            .any(|e| e["rule"] == "crash_loop" && e["level"] == "danger" && e["notify"] == true),
        "{a}"
    );

    // Warnings go to stderr; the previous (built-in) rules stay in force.
    std::fs::write(home.path("alerts.json"), "{ not json").expect("write");
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let out = home.cli(&["alerts"]);
        if String::from_utf8_lossy(&out.stderr).contains("alerts.json is invalid") {
            break;
        }
        assert!(Instant::now() < deadline, "no warning");
        std::thread::sleep(Duration::from_millis(200));
    }
    assert_eq!(
        alerts(&home)["rules"]["rules"]["crash_loop"]["enabled"],
        true
    );

    std::fs::remove_file(&marker).expect("fix the app");
    assert!(home.cli(&["restart", "crashy"]).status.success());
    wait_alerts(&home, "recovered", |a| {
        a["open"].as_array().is_some_and(Vec::is_empty)
            && a["recent"].as_array().is_some_and(|r| {
                r.iter()
                    .any(|e| e["rule"] == "crash_loop" && e["level"] == "recovered")
            })
    });
}

/// Plan step 6b.4: an incident is delivered to a webhook, signed with the
/// configured secret, which never appears in `vexor alerts`.
#[test]
fn crash_loop_alert_is_delivered_to_a_signed_webhook() {
    use std::net::TcpListener;
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut s) = stream else { continue };
            s.set_read_timeout(Some(Duration::from_secs(5))).ok();
            let mut got = Vec::new();
            let mut buf = [0u8; 4096];
            while let Ok(n) = s.read(&mut buf) {
                if n == 0 {
                    break;
                }
                got.extend_from_slice(&buf[..n]);
                let text = String::from_utf8_lossy(&got).to_string();
                if let Some((head, body)) = text.split_once("\r\n\r\n") {
                    let len: usize = head
                        .lines()
                        .find_map(|l| l.strip_prefix("Content-Length: "))
                        .and_then(|v| v.parse().ok())
                        .unwrap_or(0);
                    if body.len() >= len {
                        break;
                    }
                }
            }
            let _ = s.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n");
            let _ = tx.send(String::from_utf8_lossy(&got).to_string());
        }
    });

    let mut home = Home::new();
    let config = json!({ "channels": { "webhooks": [{
        "url": format!("http://127.0.0.1:{port}/hook"), "secret": "s3cret-key"
    }] } });
    std::fs::write(home.path("alerts.json"), config.to_string()).expect("config");
    home.add_app(
        "crashy",
        "/bin/sh",
        &["-c", "exit 3"],
        json!({ "policy": { "max_restarts": 1, "min_uptime_ms": 1000 } }),
    );
    home.start_daemon();
    let request = rx
        .recv_timeout(Duration::from_secs(30))
        .expect("webhook call");
    let (head, body) = request.split_once("\r\n\r\n").expect("http request");
    assert!(head.starts_with("POST /hook HTTP/1.1"), "{head}");
    assert!(head.contains("X-Vexor-Signature: sha256="), "{head}");
    let v: Value = serde_json::from_str(body).expect("json body");
    assert_eq!(v["alert"]["app"], "crashy");
    assert_eq!(v["alert"]["rule"], "crash_loop");
    assert_eq!(v["alert"]["level"], "danger");

    let shown = String::from_utf8_lossy(&home.cli(&["alerts", "--json"]).stdout).to_string();
    assert!(!shown.contains("s3cret-key"), "{shown}");
    assert!(shown.contains("<redacted>"), "{shown}");
}

/// Plan step 6b.7: settings.json turns on a Prometheus endpoint on
/// loopback, which shows each instance; other paths are 404.
#[test]
fn prometheus_endpoint_serves_instance_metrics() {
    let mut home = Home::new();
    let port = free_port();
    std::fs::write(
        home.path("settings.json"),
        json!({ "prometheus": { "listen": format!("127.0.0.1:{port}") } }).to_string(),
    )
    .expect("settings");
    home.add_app(
        "sleepy",
        "/bin/sh",
        &["-c", "exec sleep 60"],
        json!({ "instances": 2, "policy": { "metrics_interval_ms": 200 } }),
    );
    home.start_daemon();
    let addr: SocketAddr = ([127, 0, 0, 1], port).into();
    let fetch = |path: &str| -> Option<String> {
        let mut s = TcpStream::connect_timeout(&addr, Duration::from_secs(2)).ok()?;
        s.set_read_timeout(Some(Duration::from_secs(5))).ok()?;
        write!(s, "GET {path} HTTP/1.1\r\nHost: t\r\n\r\n").ok()?;
        let mut resp = String::new();
        s.read_to_string(&mut resp).ok()?;
        Some(resp)
    };
    let deadline = Instant::now() + TIMEOUT;
    let body = loop {
        if let Some(r) = fetch("/metrics")
            && r.contains(r#"vexor_instance_memory_bytes{app="sleepy",instance="1"}"#)
        {
            break r;
        }
        assert!(Instant::now() < deadline, "endpoint never showed the app");
        std::thread::sleep(Duration::from_millis(200));
    };
    assert!(body.starts_with("HTTP/1.1 200 OK"), "{body}");
    assert!(
        body.contains(r#"vexor_instance_up{app="sleepy",instance="0"} 1"#),
        "{body}"
    );
    assert!(body.contains("text/plain; version=0.0.4"), "{body}");
    assert!(fetch("/").expect("response").starts_with("HTTP/1.1 404"));
}

/// PM2 `scale`: only the difference is started or stopped, and the new
/// count survives a daemon restart.
#[test]
fn scale_starts_and_stops_only_the_difference() {
    let mut home = Home::new();
    home.add_app(
        "s",
        "/bin/sh",
        &["-c", "exec sleep 60"],
        json!({ "instances": 2 }),
    );
    home.start_daemon();
    let pids = |home: &Home| -> Vec<u64> {
        let out = home.cli(&["list", "--json"]);
        let apps: Vec<Value> = serde_json::from_slice(&out.stdout).expect("json");
        apps[0]["snapshot"]
            .as_array()
            .expect("snapshot")
            .iter()
            .filter(|s| s["state"] == "online")
            .filter_map(|s| s["pid"].as_u64())
            .collect()
    };
    let deadline = Instant::now() + TIMEOUT;
    while pids(&home).len() < 2 {
        assert!(Instant::now() < deadline, "never online");
        std::thread::sleep(Duration::from_millis(50));
    }
    let before = pids(&home);

    assert!(home.cli(&["scale", "s", "+2"]).status.success());
    let after = pids(&home);
    assert_eq!(after.len(), 4, "{after:?}");
    assert!(
        before.iter().all(|p| after.contains(p)),
        "{before:?} -> {after:?}"
    );

    assert!(home.cli(&["scale", "s", "1"]).status.success());
    assert_eq!(
        pids(&home),
        before[..1],
        "instance 0 kept, the rest removed"
    );

    // Persisted: a new daemon reports the scaled count.
    home.kill_daemon();
    home.start_daemon();
    let out = home.cli(&["list", "--json"]);
    let apps: Vec<Value> = serde_json::from_slice(&out.stdout).expect("json");
    assert_eq!(apps[0]["instances"], 1, "{apps:?}");
    assert!(!home.cli(&["scale", "s", "0"]).status.success());
    // Starting the original spec (2 instances) again scales back to it,
    // without restarting instance 0.
    let spec = home.path("specs/s.json");
    let again = home.cli(&["start", "--spec", &spec.to_string_lossy()]);
    assert!(
        again.status.success(),
        "{}",
        String::from_utf8_lossy(&again.stderr)
    );
    let now = pids(&home);
    assert_eq!(now.len(), 2, "{now:?}");
    assert!(now.contains(&before[0]), "{before:?} -> {now:?}");
}
