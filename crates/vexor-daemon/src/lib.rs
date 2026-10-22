//! `vexor daemon`: the control plane (docs/architecture.md §2, §7).
//!
//! The daemon never parents app processes. For each app in
//! `$VEXOR_HOME/apps/*.json` it connects to that app's shim, or launches one
//! if none is running, and follows its events. Killing or restarting the
//! daemon leaves shims and apps untouched; a new daemon reconnects.
//!
//! This is the plan step 2.4b slice: apps come from spec files, and the
//! daemon's own output is a JSON-lines log. The CLI socket, desired state and
//! event database come in steps 2.5–2.6b.

use std::fs::OpenOptions;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};
use tokio::io::BufReader;
use tokio::net::UnixStream;
use tokio::signal::unix::{SignalKind, signal};
use vexor_proto::{FromShim, PROTOCOL, Resume, ToShim};
use vexor_shim::{Identity, Paths};

/// Exit codes of `vexor daemon`.
pub mod exit {
    pub const OK: i32 = 0;
    /// Another daemon already runs for this home directory.
    pub const ALREADY_RUNNING: i32 = 3;
    pub const INTERNAL: i32 = 70;
}

/// How long to wait for a newly launched shim's socket.
const SHIM_START_TIMEOUT: Duration = Duration::from_secs(5);
/// Pause between attempts when a shim can't be reached or launched.
const RETRY: Duration = Duration::from_millis(200);

pub fn run(home: &Path) -> i32 {
    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("vexor daemon: cannot start runtime: {e}");
            return exit::INTERNAL;
        }
    };
    rt.block_on(run_async(home.to_path_buf()))
}

async fn run_async(home: PathBuf) -> i32 {
    if let Err(e) = vexor_shim::private_dir(&home)
        .and_then(|()| vexor_shim::private_dir(&vexor_shim::shims_dir(&home)))
    {
        eprintln!("vexor daemon: cannot use {}: {e}", home.display());
        return exit::INTERNAL;
    }
    let _lock = match vexor_shim::try_lock(&home.join("daemon.lock")) {
        Ok(Some(lock)) => lock,
        Ok(None) => {
            eprintln!(
                "vexor daemon: another daemon is already running for {}",
                home.display()
            );
            return exit::ALREADY_RUNNING;
        }
        Err(e) => {
            eprintln!("vexor daemon: cannot take daemon lock: {e}");
            return exit::INTERNAL;
        }
    };
    let log = match Log::open(&home.join("daemon.log")) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("vexor daemon: cannot open daemon.log: {e}");
            return exit::INTERNAL;
        }
    };
    let (mut term, mut int) = match (
        signal(SignalKind::terminate()),
        signal(SignalKind::interrupt()),
    ) {
        (Ok(t), Ok(i)) => (t, i),
        (Err(e), _) | (_, Err(e)) => {
            eprintln!("vexor daemon: cannot install signal handlers: {e}");
            return exit::INTERNAL;
        }
    };
    log.write(json!({ "type": "daemon_started", "pid": std::process::id() }));

    for (name, spec) in list_apps(&home.join("apps"), &log) {
        let follower = Follower {
            home: home.clone(),
            paths: Paths::new(&home, &name),
            name,
            spec,
            log: log.clone(),
        };
        tokio::spawn(follower.run());
    }

    // Stopping the daemon leaves shims and apps running.
    tokio::select! {
        _ = term.recv() => {}
        _ = int.recv() => {}
    }
    log.write(json!({ "type": "daemon_stopped" }));
    exit::OK
}

fn list_apps(dir: &Path, log: &Log) -> Vec<(String, PathBuf)> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut apps = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_none_or(|e| e != "json") {
            continue;
        }
        let Some(name) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        // The shim validates the full spec; here only the name matters, as
        // it becomes part of file names.
        if name.starts_with('.') || name.contains('/') {
            log.write(json!({ "type": "app_skipped", "file": path, "reason": "bad name" }));
            continue;
        }
        apps.push((name.to_owned(), path));
    }
    apps.sort();
    apps
}

/// Keeps one app's shim connected, launching it when it isn't running.
struct Follower {
    home: PathBuf,
    name: String,
    spec: PathBuf,
    paths: Paths,
    log: Log,
}

impl Follower {
    async fn run(self) {
        let mut resume: Option<Resume> = None;
        let mut unreachable_reported = false;
        let mut last_error: Option<String> = None;
        loop {
            match connect(&self.paths.socket).await {
                Ok(stream) => {
                    unreachable_reported = false;
                    last_error = None;
                    self.follow(stream, &mut resume).await;
                    self.log
                        .write(json!({ "type": "shim_disconnected", "app": self.name }));
                }
                Err(_) => match vexor_shim::try_lock(&self.paths.lock) {
                    // Lock free: no shim is running.
                    Ok(Some(lock)) => {
                        drop(lock);
                        if let Some(alive) = self.survivors() {
                            // A shim died and left its instances running.
                            // Starting a new shim would duplicate them.
                            self.log.write(json!({
                                "type": "orphaned_instances",
                                "app": self.name,
                                "pids": alive,
                            }));
                            tokio::time::sleep(Duration::from_secs(5)).await;
                            continue;
                        }
                        self.launch().await;
                    }
                    // Lock held: the shim is alive but its socket isn't
                    // answering yet (starting, or stuck).
                    Ok(None) => {
                        if !unreachable_reported {
                            unreachable_reported = true;
                            self.log
                                .write(json!({ "type": "shim_unreachable", "app": self.name }));
                        }
                    }
                    Err(e) => {
                        // Report each distinct error once, not every retry.
                        let error = format!("shim lock: {e}");
                        if last_error.as_ref() != Some(&error) {
                            self.log.write(json!({
                                "type": "error",
                                "app": self.name,
                                "error": error,
                            }));
                            last_error = Some(error);
                        }
                    }
                },
            }
            tokio::time::sleep(RETRY).await;
        }
    }

    /// Instance leaders recorded by a shim that is no longer running and
    /// that are still alive.
    fn survivors(&self) -> Option<Vec<u32>> {
        let text = std::fs::read_to_string(&self.paths.pids).ok()?;
        let ids: Vec<Identity> = serde_json::from_str(&text).ok()?;
        let alive: Vec<u32> = ids.iter().filter(|i| i.is_alive()).map(|i| i.pid).collect();
        (!alive.is_empty()).then_some(alive)
    }

    async fn launch(&self) {
        let result = self.spawn_shim();
        match result {
            Ok(mut child) => {
                self.log.write(json!({
                    "type": "shim_launched",
                    "app": self.name,
                    "pid": child.id(),
                }));
                // Reap the shim if it exits while this daemon runs.
                tokio::spawn(async move {
                    let _ = child.wait().await;
                });
                let deadline = tokio::time::Instant::now() + SHIM_START_TIMEOUT;
                while tokio::time::Instant::now() < deadline && !self.paths.socket.exists() {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            }
            Err(e) => self.log.write(json!({
                "type": "error",
                "app": self.name,
                "error": format!("cannot launch shim: {e}"),
            })),
        }
    }

    fn spawn_shim(&self) -> std::io::Result<tokio::process::Child> {
        let exe = std::env::current_exe()?;
        let shim_log = OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(vexor_shim::shims_dir(&self.home).join(format!("{}.log", self.name)))?;
        tokio::process::Command::new(exe)
            .arg("shim")
            .arg("--spec")
            .arg(&self.spec)
            .arg("--home")
            .arg(&self.home)
            .stdin(Stdio::null())
            .stdout(shim_log.try_clone()?)
            .stderr(shim_log)
            // Own process group: signals aimed at the daemon (Ctrl-C in a
            // terminal, kill of its group) don't reach the shim.
            .process_group(0)
            .kill_on_drop(false)
            .spawn()
    }

    async fn follow(&self, stream: UnixStream, resume: &mut Option<Resume>) {
        if !same_user(&stream) {
            self.log.write(json!({
                "type": "error",
                "app": self.name,
                "error": "shim socket owned by another user",
            }));
            return;
        }
        let (read, mut write) = stream.into_split();
        let hello = ToShim::Hello {
            protocol: PROTOCOL,
            resume: *resume,
        };
        if vexor_proto::send(&mut write, &hello).await.is_err() {
            return;
        }
        let mut reader = BufReader::new(read);
        let mut buf = Vec::new();
        loop {
            let msg = match vexor_proto::recv::<_, FromShim>(&mut reader, &mut buf).await {
                Ok(Some(m)) => m,
                Ok(None) => return,
                Err(e) => {
                    self.log.write(json!({
                        "type": "error",
                        "app": self.name,
                        "error": format!("shim protocol: {e}"),
                    }));
                    return;
                }
            };
            match msg {
                FromShim::Welcome {
                    shim_pid,
                    shim_start,
                    next_seq,
                    snapshot,
                    ..
                } => {
                    let same_shim = resume.is_some_and(|r| r.shim_start == shim_start);
                    if !same_shim {
                        *resume = Some(Resume {
                            shim_start,
                            last_seq: next_seq.saturating_sub(1),
                        });
                    }
                    self.log.write(json!({
                        "type": "shim_connected",
                        "app": self.name,
                        "shim_pid": shim_pid,
                        "next_seq": next_seq,
                        "snapshot": snapshot,
                    }));
                }
                FromShim::Event { seq, event } => {
                    if let Some(r) = resume.as_mut() {
                        r.last_seq = seq;
                    }
                    self.log.write(json!({
                        "type": "event",
                        "app": self.name,
                        "seq": seq,
                        "event": event,
                    }));
                }
                FromShim::Gap { from, to } => {
                    self.log.write(json!({
                        "type": "event_gap",
                        "app": self.name,
                        "from": from,
                        "to": to,
                    }));
                }
                FromShim::Refused { reason } => {
                    self.log.write(json!({
                        "type": "shim_refused",
                        "app": self.name,
                        "reason": reason,
                    }));
                    return;
                }
            }
        }
    }
}

async fn connect(path: &Path) -> std::io::Result<UnixStream> {
    UnixStream::connect(path).await
}

fn same_user(stream: &UnixStream) -> bool {
    stream
        .peer_cred()
        .is_ok_and(|c| c.uid() == rustix::process::geteuid().as_raw())
}

/// JSON-lines log: `$VEXOR_HOME/daemon.log`, mirrored to stdout.
#[derive(Debug, Clone)]
struct Log(Arc<Mutex<std::fs::File>>);

impl Log {
    fn open(path: &Path) -> std::io::Result<Log> {
        let f = OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(path)?;
        Ok(Log(Arc::new(Mutex::new(f))))
    }

    fn write(&self, mut entry: Value) {
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_millis());
        if let Value::Object(m) = &mut entry {
            m.insert("ts_ms".into(), json!(ts));
        }
        let line = format!("{entry}\n");
        let mut f = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        // Logging must never stop the daemon.
        let _ = f.write_all(line.as_bytes());
        let mut out = std::io::stdout().lock();
        let _ = out.write_all(line.as_bytes());
        let _ = out.flush();
    }
}
