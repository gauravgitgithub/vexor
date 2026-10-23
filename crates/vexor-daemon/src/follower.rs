//! Keeps one app's shim in line with its desired state: launches it when the
//! app should run and no shim is up, follows its events while connected, and
//! forwards operator commands.

use std::collections::BTreeMap;
use std::fs::OpenOptions;
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::BufReader;
use tokio::net::UnixStream;
use tokio::sync::{mpsc, watch};
use vexor_proto::{Desired, FromShim, PROTOCOL, Resume, ToShim};
use vexor_shim::{Identity, Paths};

use crate::events::Events;
use crate::log::Log;

/// How long to wait for a newly launched shim's socket. Another shim is
/// launched only after this, or once the first has exited: a slow start
/// on a loaded machine must not pile up shims (and more load).
const SHIM_START_TIMEOUT: Duration = Duration::from_secs(60);
/// Pause between attempts when a shim can't be reached or launched.
const RETRY: Duration = Duration::from_millis(200);

/// What the daemon currently knows about an app's shim and instances.
#[derive(Debug, Clone, Default)]
pub struct Live(Arc<Mutex<LiveInner>>);

#[derive(Debug, Default)]
struct LiveInner {
    shim_pid: Option<u32>,
    snapshot: BTreeMap<u64, Value>,
    /// Reload outcomes seen per instance: (succeeded, failed).
    reloads: BTreeMap<u64, (u64, u64)>,
}

impl Live {
    pub fn shim_pid(&self) -> Option<u32> {
        self.lock().shim_pid
    }

    pub fn snapshot(&self) -> Vec<Value> {
        self.lock().snapshot.values().cloned().collect()
    }

    fn connected(&self, pid: u32, snapshot: Vec<Value>) {
        let mut l = self.lock();
        l.shim_pid = Some(pid);
        l.snapshot = snapshot
            .into_iter()
            .filter_map(|s| Some((s["instance"].as_u64()?, s)))
            .collect();
    }

    /// Reload outcomes seen per instance so far: (succeeded, failed).
    pub fn reloads(&self) -> BTreeMap<u64, (u64, u64)> {
        self.lock().reloads.clone()
    }

    fn state_event(&self, event: &Value) {
        let Some(i) = event["instance"].as_u64() else {
            return;
        };
        if event["type"] == "state" {
            self.lock().snapshot.insert(i, event.clone());
        } else if event["type"] == "reload" {
            let mut l = self.lock();
            let counts = l.reloads.entry(i).or_default();
            if event["outcome"] == "succeeded" {
                counts.0 += 1;
            } else {
                counts.1 += 1;
            }
        }
    }

    fn disconnected(&self) {
        self.lock().shim_pid = None;
    }

    fn lock(&self) -> MutexGuard<'_, LiveInner> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

pub struct Follower {
    pub home: PathBuf,
    pub name: String,
    pub spec: PathBuf,
    pub paths: Paths,
    pub log: Log,
    pub events: Events,
    /// Last stored position in this app's shim event stream.
    pub resume: Option<Resume>,
    pub live: Live,
    pub desired: watch::Receiver<Desired>,
    pub commands: mpsc::Receiver<ToShim>,
}

impl Follower {
    pub async fn run(mut self) {
        let mut resume = self.resume.take();
        let mut unreachable_reported = false;
        let mut last_error: Option<String> = None;
        loop {
            match UnixStream::connect(&self.paths.socket).await {
                Ok(stream) => {
                    unreachable_reported = false;
                    last_error = None;
                    self.follow(stream, &mut resume).await;
                    self.live.disconnected();
                    self.log
                        .write(json!({ "type": "shim_disconnected", "app": self.name }));
                }
                Err(_) => match vexor_shim::try_lock(&self.paths.lock) {
                    // Lock free: no shim is running.
                    Ok(Some(lock)) => {
                        drop(lock);
                        if *self.desired.borrow() == Desired::Stopped {
                            self.idle().await;
                            continue;
                        }
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

    /// No shim, and the app should stay stopped: wait until that changes.
    /// Commands for a stopped app have nothing to act on and are dropped.
    async fn idle(&mut self) {
        loop {
            tokio::select! {
                changed = self.desired.changed() => {
                    if changed.is_err() || *self.desired.borrow() == Desired::Running {
                        return;
                    }
                }
                cmd = self.commands.recv() => if cmd.is_none() { return },
            }
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
        match self.spawn_shim() {
            Ok(mut child) => {
                self.log.write(json!({
                    "type": "shim_launched",
                    "app": self.name,
                    "pid": child.id(),
                }));
                let deadline = tokio::time::Instant::now() + SHIM_START_TIMEOUT;
                while tokio::time::Instant::now() < deadline && !self.paths.socket.exists() {
                    if matches!(child.try_wait(), Ok(Some(_))) {
                        // Exited (e.g. another shim holds the lock): retry.
                        return;
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
                if !self.paths.socket.exists() {
                    self.log.write(json!({
                        "type": "error",
                        "app": self.name,
                        "error": format!("shim not ready after {SHIM_START_TIMEOUT:?}"),
                    }));
                }
                // Reap the shim if it exits while this daemon runs.
                tokio::spawn(async move {
                    let _ = child.wait().await;
                });
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
            .arg("--home")
            .arg(&self.home)
            .arg("shim")
            .arg("--spec")
            .arg(&self.spec)
            .arg("--managed")
            .stdin(Stdio::null())
            .stdout(shim_log.try_clone()?)
            .stderr(shim_log)
            // Own process group: signals aimed at the daemon (Ctrl-C in a
            // terminal, kill of its group) don't reach the shim.
            .process_group(0)
            .kill_on_drop(false)
            .spawn()
    }

    async fn follow(&mut self, stream: UnixStream, resume: &mut Option<Resume>) {
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

        // Reading isn't cancel-safe, so it runs in its own task.
        let (tx, mut messages) = mpsc::channel::<std::io::Result<FromShim>>(64);
        let reader = tokio::spawn(async move {
            let mut r = BufReader::new(read);
            let mut buf = Vec::new();
            loop {
                let msg = vexor_proto::recv::<_, FromShim>(&mut r, &mut buf).await;
                let done = !matches!(msg, Ok(Some(_)));
                let item = match msg {
                    Ok(Some(m)) => Ok(m),
                    Ok(None) => break,
                    Err(e) => Err(e),
                };
                if tx.send(item).await.is_err() || done {
                    break;
                }
            }
        });
        let _guard = AbortOnDrop(reader);

        // If the app should be stopped, say so as soon as we're connected
        // (covers a daemon restarted between `stop` and the shim exiting).
        let mut stop_sent = false;
        loop {
            if !stop_sent && *self.desired.borrow() == Desired::Stopped {
                stop_sent = true;
                if vexor_proto::send(&mut write, &ToShim::Stop).await.is_err() {
                    return;
                }
            }
            tokio::select! {
                msg = messages.recv() => match msg {
                    Some(Ok(m)) => if !self.handle(m, resume) { return },
                    Some(Err(e)) => {
                        self.log.write(json!({
                            "type": "error",
                            "app": self.name,
                            "error": format!("shim protocol: {e}"),
                        }));
                        return;
                    }
                    None => return,
                },
                cmd = self.commands.recv() => match cmd {
                    Some(c) => if vexor_proto::send(&mut write, &c).await.is_err() { return },
                    None => return,
                },
                changed = self.desired.changed() => {
                    if changed.is_err() {
                        return;
                    }
                    if *self.desired.borrow() == Desired::Running {
                        stop_sent = false;
                    }
                }
            }
        }
    }

    /// Returns false when the connection should be dropped.
    fn handle(&self, msg: FromShim, resume: &mut Option<Resume>) -> bool {
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
                self.live.connected(shim_pid, snapshot.clone());
                self.events.record_daemon(
                    Some(&self.name),
                    "shim_connected",
                    json!({ "shim_pid": shim_pid }),
                );
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
                    self.events
                        .record_shim(&self.name, r.shim_start, seq, event.clone());
                }
                self.live.state_event(&event);
                self.log.write(json!({
                    "type": "event",
                    "app": self.name,
                    "seq": seq,
                    "event": event,
                }));
            }
            FromShim::Gap { from, to } => {
                self.events.record_daemon(
                    Some(&self.name),
                    "event_gap",
                    json!({ "from": from, "to": to }),
                );
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
                return false;
            }
        }
        true
    }
}

pub fn same_user(stream: &UnixStream) -> bool {
    stream
        .peer_cred()
        .is_ok_and(|c| c.uid() == rustix::process::geteuid().as_raw())
}

struct AbortOnDrop(tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}
