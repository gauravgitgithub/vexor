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
}

impl Home {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(dir.path().join("apps")).expect("apps dir");
        Home { dir, daemon: None }
    }

    fn path(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
    }

    fn add_app(&self, name: &str, exec: &str, args: &[&str], extra: Value) {
        let mut spec = json!({
            "name": name,
            "exec": exec,
            "args": args,
            "cwd": self.dir.path(),
            "env": { "PATH": "/usr/bin:/bin" },
            "logs": {
                "out": self.path(&format!("{name}.out")),
                "err": self.path(&format!("{name}.err")),
            },
        });
        if let (Value::Object(b), Value::Object(e)) = (&mut spec, extra) {
            b.extend(e);
        }
        std::fs::write(self.path(&format!("apps/{name}.json")), spec.to_string())
            .expect("write spec");
    }

    fn spawn_daemon(&self) -> Child {
        Command::new(env!("CARGO_BIN_EXE_vexor"))
            .arg("daemon")
            .arg("--home")
            .arg(self.dir.path())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("start daemon")
    }

    fn start_daemon(&mut self) {
        assert!(self.daemon.is_none(), "daemon already running");
        self.daemon = Some(self.spawn_daemon());
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

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .and_then(|l| l.local_addr())
        .map(|a| a.port())
        .expect("free port")
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
    assert_eq!(snap["state"], "online", "{connected}");
    assert_eq!(snap["restarts"], 1, "{connected}");
    assert_eq!(snap["pid"], after, "{connected}");
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
        .args(["shim", "--spec"])
        .arg(home.path("apps/solo.json"))
        .arg("--home")
        .arg(home.dir.path())
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
