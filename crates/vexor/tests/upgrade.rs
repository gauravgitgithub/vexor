//! In-place shim upgrade (docs/architecture.md §7): a daemon-managed shim
//! asked to `upgrade` replaces its program image and keeps every app
//! process, its output pipes, its event stream and its control socket.
//! The test plays the daemon on the shim's control socket.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

const TIMEOUT: Duration = Duration::from_secs(20);

/// Prints `<instance> <counter>` every 10 ms, forever.
const COUNTER: &str =
    "i=0; while :; do echo \"$NODE_APP_INSTANCE $i\"; i=$((i+1)); sleep 0.01; done";

struct Shim {
    // Short path: Unix socket paths are limited to ~100 bytes.
    home: tempfile::TempDir,
    child: Child,
}

impl Shim {
    fn start(instances: u32) -> Shim {
        Shim::start_with(
            "/bin/sh",
            &["-c", COUNTER],
            json!({ "instances": instances }),
        )
    }

    fn start_with(exec: &str, args: &[&str], extra: Value) -> Shim {
        let home = tempfile::Builder::new()
            .prefix("vxu")
            .tempdir_in("/tmp")
            .expect("tempdir");
        let mut spec = json!({
            "name": "t",
            "exec": exec,
            "args": args,
            "cwd": home.path(),
            "env": { "PATH": "/usr/bin:/bin" },
            "logs": {
                "timestamps": false,
                "out": home.path().join("out.log"),
                "err": home.path().join("err.log"),
            },
        });
        if let (Value::Object(b), Value::Object(e)) = (&mut spec, extra) {
            b.extend(e);
        }
        let spec_path = home.path().join("spec.json");
        std::fs::write(&spec_path, spec.to_string()).expect("spec");
        let log = std::fs::File::create(home.path().join("shim.log")).expect("log");
        let child = Command::new(env!("CARGO_BIN_EXE_vexor"))
            .arg("--home")
            .arg(home.path())
            .args(["shim", "--managed", "--spec"])
            .arg(&spec_path)
            .stdout(Stdio::null())
            .stderr(log)
            .spawn()
            .expect("start shim");
        Shim { home, child }
    }

    fn path(&self, name: &str) -> PathBuf {
        self.home.path().join(name)
    }

    fn shim_log(&self) -> String {
        std::fs::read_to_string(self.path("shim.log")).unwrap_or_default()
    }

    /// Connect as a daemon would; retries while the socket isn't served.
    fn connect(&self) -> Welcomed {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            if let Ok(s) = UnixStream::connect(self.path("shims/t.sock")) {
                s.set_read_timeout(Some(Duration::from_secs(5)))
                    .expect("timeout");
                let mut c = Conn {
                    reader: BufReader::new(s.try_clone().expect("clone")),
                    writer: s,
                };
                c.send(&json!({ "type": "hello", "protocol": 5, "resume": null }));
                if let Some(w) = c.recv().filter(|m| m["type"] == "welcome") {
                    return Conn::welcomed(c, w);
                }
            }
            assert!(
                Instant::now() < deadline,
                "shim not answering\n{}",
                self.shim_log()
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn out_log(&self) -> String {
        std::fs::read_to_string(self.path("out.log")).unwrap_or_default()
    }

    /// Wait until every instance has printed past `n`.
    fn wait_counters_past(&self, instances: u32, n: u64) {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            let c = counters(&self.out_log());
            if (0..instances).all(|i| c.get(&i).is_some_and(|v| v.last() > Some(&n))) {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "counters stuck: {c:?}\n{}",
                self.shim_log()
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

impl Drop for Shim {
    fn drop(&mut self) {
        if let Ok(mut c) = UnixStream::connect(self.path("shims/t.sock")) {
            let _ = writeln!(
                c,
                "{}",
                json!({ "type": "hello", "protocol": 5, "resume": null })
            );
            let _ = writeln!(c, "{}", json!({ "type": "stop" }));
        }
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if matches!(self.child.try_wait(), Ok(Some(_))) {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let _ = self.child.kill();
    }
}

struct Conn {
    reader: BufReader<UnixStream>,
    writer: UnixStream,
}

struct Welcomed {
    conn: Conn,
    welcome: Value,
}

impl Conn {
    fn welcomed(conn: Conn, welcome: Value) -> Welcomed {
        Welcomed { conn, welcome }
    }

    fn send(&mut self, v: &Value) {
        let _ = writeln!(self.writer, "{v}");
    }

    fn recv(&mut self) -> Option<Value> {
        let mut line = String::new();
        match self.reader.read_line(&mut line) {
            Ok(n) if n > 0 => serde_json::from_str(&line).ok(),
            _ => None,
        }
    }
}

impl Welcomed {
    fn pids(&self) -> BTreeMap<u64, u64> {
        pids(&self.welcome["snapshot"])
    }

    /// Events streamed after the welcome, until `until` matches one.
    fn events_until(&mut self, until: impl Fn(&Value) -> bool) -> Vec<Value> {
        let deadline = Instant::now() + TIMEOUT;
        let mut seen = Vec::new();
        while Instant::now() < deadline {
            let Some(m) = self.conn.recv() else { continue };
            if m["type"] == "event" {
                let e = m["event"].clone();
                let done = until(&e);
                seen.push(e);
                if done {
                    return seen;
                }
            }
        }
        panic!("event never came; saw {seen:?}");
    }
}

fn pids(snapshot: &Value) -> BTreeMap<u64, u64> {
    snapshot
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|s| Some((s["instance"].as_u64()?, s["pid"].as_u64()?)))
        .collect()
}

/// Counter values per instance, in log order.
fn counters(log: &str) -> BTreeMap<u32, Vec<u64>> {
    let mut out: BTreeMap<u32, Vec<u64>> = BTreeMap::new();
    for line in log.lines() {
        let mut it = line.split(' ');
        if let (Some(Ok(i)), Some(Ok(n))) = (it.next().map(str::parse), it.next().map(str::parse)) {
            out.entry(i).or_default().push(n);
        }
    }
    out
}

/// Each instance's counter, per process, goes up by one: nothing lost or
/// repeated. A restarted process starts again from 0.
fn assert_no_gaps(log: &str) {
    for (i, values) in counters(log) {
        for w in values.windows(2) {
            assert!(
                w[1] == w[0] + 1 || w[1] == 0,
                "instance {i}: {} then {}",
                w[0],
                w[1]
            );
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

fn upgrade(conn: &mut Welcomed, exec: &Path) {
    conn.conn.send(&json!({ "type": "upgrade", "exec": exec }));
}

/// Ask for an upgrade and wait until the shim closes this connection,
/// which it does when it hands over (or resumes after a failed exec).
fn upgrade_and_wait(mut conn: Welcomed, exec: &Path) {
    upgrade(&mut conn, exec);
    let deadline = Instant::now() + TIMEOUT;
    let mut line = String::new();
    loop {
        line.clear();
        match conn.conn.reader.read_line(&mut line) {
            Ok(0) => return,
            Ok(_) => {}
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) => {}
            Err(_) => return,
        }
        assert!(Instant::now() < deadline, "the shim never handed over");
    }
}

#[test]
fn upgrade_in_place_keeps_processes_output_and_events() {
    let shim = Shim::start(2);
    let before = shim.connect();
    let welcome = before.welcome.clone();
    shim.wait_counters_past(2, 20);
    let pids_before = before.pids();
    assert_eq!(pids_before.len(), 2, "{welcome}");
    assert_eq!(welcome["version"], env!("CARGO_PKG_VERSION"));

    upgrade_and_wait(before, Path::new(env!("CARGO_BIN_EXE_vexor")));
    // The new image serves the same socket.
    let after = shim.connect();
    assert!(
        after.welcome["next_seq"].as_u64() > welcome["next_seq"].as_u64(),
        "{}\n{}",
        after.welcome,
        shim.shim_log()
    );
    assert_eq!(
        after.welcome["shim_pid"], welcome["shim_pid"],
        "same shim process"
    );
    assert_eq!(after.welcome["shim_start"], welcome["shim_start"]);
    assert_eq!(after.pids(), pids_before, "apps not restarted");
    assert!(
        shim.shim_log().is_empty() || !shim.shim_log().contains("failed"),
        "{}",
        shim.shim_log()
    );

    // Output keeps flowing through the handed-over pipes, nothing lost.
    let last: u64 = counters(&shim.out_log())[&0]
        .last()
        .copied()
        .expect("lines");
    shim.wait_counters_past(2, last + 20);
    assert_no_gaps(&shim.out_log());

    // The event stream continued: replay from 0 shows the upgrade.
    let mut replay = shim.connect();
    let seen = replay.events_until(|e| e["type"] == "upgraded");
    assert!(
        seen.iter().any(|e| e["type"] == "state"),
        "earlier events kept"
    );
    drop(replay);

    // Inherited children are still waited for: a crash is seen with its
    // signal and restarted.
    let p0 = pids_before[&0];
    let mut watch = shim.connect();
    Command::new("kill")
        .args(["-KILL", &p0.to_string()])
        .status()
        .expect("kill");
    let seen = watch.events_until(|e| {
        e["type"] == "state"
            && e["instance"] == 0
            && e["state"] == "online"
            && e["pid"].as_u64() != Some(p0)
    });
    assert!(
        seen.iter().any(|e| e["exit"]["signal"] == 9),
        "exit status of the inherited child: {seen:?}"
    );
    assert!(alive(pids_before[&1]));
}

#[test]
fn upgrade_to_a_missing_program_changes_nothing() {
    let shim = Shim::start(1);
    let mut conn = shim.connect();
    let pids_before = conn.pids();
    upgrade(&mut conn, Path::new("/nonexistent/vexor"));
    let seen = conn.events_until(|e| e["type"] == "upgrade_skipped");
    assert!(
        seen.last().expect("event")["reason"]
            .as_str()
            .is_some_and(|r| r.contains("not found")),
        "{seen:?}"
    );
    let again = shim.connect();
    assert_eq!(again.pids(), pids_before);
}

#[test]
fn a_failed_exec_resumes_in_the_same_program() {
    let shim = Shim::start(1);
    let conn = shim.connect();
    let welcome = conn.welcome.clone();
    shim.wait_counters_past(1, 10);
    // A file that exists but can't be executed.
    upgrade_and_wait(conn, &shim.path("spec.json"));
    let after = shim.connect();
    assert!(
        after.welcome["next_seq"].as_u64() > welcome["next_seq"].as_u64(),
        "{}\n{}",
        after.welcome,
        shim.shim_log()
    );
    assert_eq!(after.welcome["shim_pid"], welcome["shim_pid"]);
    assert_eq!(after.pids(), pids(&welcome["snapshot"]));
    let mut replay = shim.connect();
    let seen = replay.events_until(|e| e["type"] == "upgrade_skipped");
    assert!(
        seen.last().expect("event")["reason"]
            .as_str()
            .is_some_and(|r| r.starts_with("exec ")),
        "{seen:?}"
    );
    let last: u64 = counters(&shim.out_log())[&0]
        .last()
        .copied()
        .expect("lines");
    shim.wait_counters_past(1, last + 10);
    assert_no_gaps(&shim.out_log());
}

fn which(program: &str) -> Option<String> {
    let out = Command::new("sh")
        .args(["-c", &format!("command -v {program}")])
        .output()
        .ok()?;
    let p = String::from_utf8(out.stdout).ok()?.trim().to_owned();
    (!p.is_empty()).then_some(p)
}

fn get(port: u16) -> Result<String, String> {
    use std::io::Read;
    let mut s = std::net::TcpStream::connect(("127.0.0.1", port)).map_err(|e| e.to_string())?;
    s.set_read_timeout(Some(Duration::from_secs(5)))
        .map_err(|e| e.to_string())?;
    write!(s, "GET / HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n")
        .map_err(|e| e.to_string())?;
    let mut body = String::new();
    s.read_to_string(&mut body).map_err(|e| e.to_string())?;
    body.rsplit("\r\n\r\n")
        .next()
        .map(str::to_owned)
        .ok_or_else(|| "no body".into())
}

/// The point of it: an HTTP app with a Node IPC channel serves every
/// request through the upgrade, from the same process, still connected to
/// its (new) supervisor, and `process.send('ready')` channels stay usable.
#[test]
fn upgrade_under_http_load_drops_no_requests() {
    let Some(node) = which("node") else {
        eprintln!("skipping: node not found on PATH");
        return;
    };
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .expect("port")
        .local_addr()
        .expect("addr")
        .port();
    let server = "require('http').createServer((q, r) => { r.setHeader('Connection', 'close'); \
                  r.end(process.pid + ' ' + process.connected); console.log('req'); })\
                  .listen(+process.env.PORT, '127.0.0.1', () => process.send('ready'));";
    let shim = Shim::start_with(
        &node,
        &["-e", server],
        json!({
            "node_ipc": true,
            "env": { "PATH": "/usr/bin:/bin", "PORT": port.to_string() },
            "policy": { "ready_timeout_ms": 10000 },
        }),
    );
    let conn = shim.connect();
    let deadline = Instant::now() + TIMEOUT;
    let first = loop {
        if let Ok(b) = get(port) {
            break b;
        }
        assert!(
            Instant::now() < deadline,
            "app never served\n{}",
            shim.shim_log()
        );
        std::thread::sleep(Duration::from_millis(50));
    };
    assert!(first.ends_with(" true"), "{first}");

    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let s = stop.clone();
    let load = std::thread::spawn(move || {
        let (mut ok, mut failed, mut bodies) = (0, Vec::new(), std::collections::BTreeSet::new());
        while !s.load(std::sync::atomic::Ordering::Relaxed) {
            match get(port) {
                Ok(b) => {
                    ok += 1;
                    bodies.insert(b);
                }
                Err(e) => failed.push(e),
            }
        }
        (ok, failed, bodies)
    });
    std::thread::sleep(Duration::from_millis(300));
    upgrade_and_wait(conn, Path::new(env!("CARGO_BIN_EXE_vexor")));
    let deadline = Instant::now() + TIMEOUT;
    while !upgraded(&shim) {
        assert!(Instant::now() < deadline, "no upgrade\n{}", shim.shim_log());
        std::thread::sleep(Duration::from_millis(50));
    }
    std::thread::sleep(Duration::from_millis(500));
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    let (ok, failed, bodies) = load.join().expect("load");
    assert!(
        failed.is_empty(),
        "{} failed of {}: {failed:?}",
        failed.len(),
        ok + failed.len()
    );
    assert!(ok > 50, "only {ok} requests");
    assert_eq!(bodies.len(), 1, "one process, always connected: {bodies:?}");
    assert!(bodies.iter().all(|b| b.ends_with(" true")));
    // The app's output still reaches its log after the upgrade.
    let before = shim.out_log().matches("req").count();
    get(port).expect("request");
    let deadline = Instant::now() + TIMEOUT;
    while shim.out_log().matches("req").count() <= before {
        assert!(Instant::now() < deadline, "output stopped");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// The shim's event stream has an `upgraded` event.
fn upgraded(shim: &Shim) -> bool {
    let mut c = shim.connect();
    let next = c.welcome["next_seq"].as_u64().unwrap_or(0);
    let mut seen = 0;
    while seen < next {
        let Some(m) = c.conn.recv() else { return false };
        if m["type"] == "event" {
            seen += 1;
            if m["event"]["type"] == "upgraded" {
                return true;
            }
        }
    }
    false
}

/// The new program dies before it resumes (a broken release, or killed
/// mid-upgrade): the shim is gone, its apps aren't. The next shim adopts
/// them, same PIDs, and cleans up the stale handover file.
#[test]
fn a_new_program_dying_mid_upgrade_leaves_the_apps_to_be_adopted() {
    let shim = Shim::start_with(
        "/bin/sh",
        &["-c", "exec sleep 300"],
        json!({ "instances": 2 }),
    );
    let conn = shim.connect();
    let pids_before = conn.pids();
    assert_eq!(pids_before.len(), 2);
    let broken = shim.path("broken-vexor");
    std::fs::write(&broken, "#!/bin/sh\nexit 1\n").expect("write");
    std::fs::set_permissions(&broken, std::os::unix::fs::PermissionsExt::from_mode(0o755))
        .expect("chmod");
    upgrade_and_wait(conn, &broken);
    let shim = shim;
    // The shim's process (now the broken program) exits.
    let deadline = Instant::now() + TIMEOUT;
    let mut first = shim;
    loop {
        if matches!(first.child.try_wait(), Ok(Some(_))) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "shim still running\n{}",
            first.shim_log()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(
        pids_before.values().all(|p| alive(*p)),
        "apps died with the shim"
    );
    assert!(
        first.path("shims/t.handover.json").exists(),
        "handover left behind"
    );

    // What the daemon does next: start a shim, which adopts them.
    let log = std::fs::File::create(first.path("shim2.log")).expect("log");
    let second = Command::new(env!("CARGO_BIN_EXE_vexor"))
        .arg("--home")
        .arg(first.home.path())
        .args(["shim", "--managed", "--spec"])
        .arg(first.path("spec.json"))
        .stdout(Stdio::null())
        .stderr(log)
        .spawn()
        .expect("start shim");
    first.child = second;
    let deadline = Instant::now() + TIMEOUT;
    let adopted = loop {
        let c = first.connect();
        let snap = c.welcome["snapshot"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        if snap.len() == 2 && snap.iter().all(|s| s["adopted"] == true) {
            break c;
        }
        assert!(Instant::now() < deadline, "not adopted: {snap:?}");
        std::thread::sleep(Duration::from_millis(50));
    };
    assert_eq!(adopted.pids(), pids_before);
    assert!(
        !first.path("shims/t.handover.json").exists(),
        "stale handover removed"
    );
}

/// Same, with an app that writes output: the dead shim's pipe is gone, so
/// its next write ends it (SIGPIPE); the new shim simply starts it again.
#[test]
fn a_chatty_app_orphaned_mid_upgrade_is_started_again() {
    let shim = Shim::start(1);
    let conn = shim.connect();
    let p0 = conn.pids()[&0];
    shim.wait_counters_past(1, 5);
    let broken = shim.path("broken-vexor");
    std::fs::write(&broken, "#!/bin/sh\nexit 1\n").expect("write");
    std::fs::set_permissions(&broken, std::os::unix::fs::PermissionsExt::from_mode(0o755))
        .expect("chmod");
    upgrade_and_wait(conn, &broken);
    let mut first = shim;
    let deadline = Instant::now() + TIMEOUT;
    while !matches!(first.child.try_wait(), Ok(Some(_))) {
        assert!(Instant::now() < deadline, "shim still running");
        std::thread::sleep(Duration::from_millis(20));
    }
    let second = Command::new(env!("CARGO_BIN_EXE_vexor"))
        .arg("--home")
        .arg(first.home.path())
        .args(["shim", "--managed", "--spec"])
        .arg(first.path("spec.json"))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("start shim");
    first.child = second;
    let deadline = Instant::now() + TIMEOUT;
    loop {
        let c = first.connect();
        let snap = c.welcome["snapshot"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        if snap.iter().any(|s| {
            s["state"] == "online" && s["pid"].as_u64() != Some(p0) && s.get("adopted").is_none()
        }) {
            break;
        }
        assert!(Instant::now() < deadline, "not started again: {snap:?}");
        std::thread::sleep(Duration::from_millis(50));
    }
    let last = counters(&first.out_log())[&0]
        .last()
        .copied()
        .expect("lines");
    let deadline = Instant::now() + TIMEOUT;
    while counters(&first.out_log())[&0].last().copied() == Some(last) {
        assert!(Instant::now() < deadline, "no new output");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// A command is carried out even when the daemon disconnects right after
/// sending it (it used to be lost if the shim noticed the closed
/// connection first).
#[test]
fn a_command_sent_just_before_disconnecting_is_carried_out() {
    for _ in 0..5 {
        let mut shim = Shim::start(1);
        let mut conn = shim.connect();
        conn.conn.send(&json!({ "type": "stop" }));
        drop(conn);
        let deadline = Instant::now() + TIMEOUT;
        while !matches!(shim.child.try_wait(), Ok(Some(_))) {
            assert!(
                Instant::now() < deadline,
                "stop was lost\n{}",
                shim.shim_log()
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}
