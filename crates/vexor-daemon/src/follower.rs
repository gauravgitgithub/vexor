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
use vexor_proto::{
    Desired, FromShim, InstanceMetrics, LogLines, MIN_SHIM_PROTOCOL, Resume, SHIM_PROTOCOL, ToShim,
};
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
    /// Latest resource sample per running instance, from the shim.
    metrics: Vec<InstanceMetrics>,
    /// Protocol agreed with the connected shim.
    protocol: u32,
    /// Lines the connected shim has read for route stats.
    lines: LogLines,
    /// Requests and 5xx responses per (method, route) since the daemon
    /// started, for Prometheus counters.
    route_totals: BTreeMap<(String, String), (u64, u64)>,
}

/// Routes counted per app for Prometheus; others add to `(other)`.
const MAX_ROUTE_TOTALS: usize = 1_000;

impl Live {
    pub fn shim_pid(&self) -> Option<u32> {
        self.lock().shim_pid
    }

    pub fn snapshot(&self) -> Vec<Value> {
        self.lock().snapshot.values().cloned().collect()
    }

    pub fn metrics(&self) -> Vec<InstanceMetrics> {
        self.lock().metrics.clone()
    }

    /// Lines read for route stats, when the connected shim reads them.
    pub fn log_lines(&self) -> Option<LogLines> {
        let l = self.lock();
        (l.shim_pid.is_some() && l.protocol >= 4).then_some(l.lines)
    }

    /// Requests and 5xx responses per (method, route).
    pub fn route_totals(&self) -> Vec<(String, String, u64, u64)> {
        self.lock()
            .route_totals
            .iter()
            .map(|((m, r), (n, e))| (m.clone(), r.clone(), *n, *e))
            .collect()
    }

    fn route_window(&self, w: &vexor_proto::RouteWindow) {
        let mut l = self.lock();
        l.lines.lines += w.lines;
        l.lines.json_lines += w.json_lines;
        for r in &w.routes {
            l.lines.requests += r.count;
            let mut key = (
                vexor_proto::http_method(&r.method).to_owned(),
                r.route.clone(),
            );
            if !l.route_totals.contains_key(&key) && l.route_totals.len() >= MAX_ROUTE_TOTALS {
                key = ("-".to_owned(), "(other)".to_owned());
            }
            let t = l.route_totals.entry(key).or_default();
            t.0 += r.count;
            t.1 += r.server_errors;
        }
    }

    /// Protocol of the connected shim (0 when none is connected).
    pub fn protocol(&self) -> u32 {
        let l = self.lock();
        if l.shim_pid.is_some() { l.protocol } else { 0 }
    }

    fn connected(&self, pid: u32, protocol: u32, snapshot: Vec<Value>) {
        let mut l = self.lock();
        l.shim_pid = Some(pid);
        l.protocol = protocol;
        l.metrics.clear();
        l.lines = LogLines::default();
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
        if event["type"] == "instance_removed" {
            let mut l = self.lock();
            l.snapshot.remove(&i);
            l.metrics.retain(|m| u64::from(m.instance) != i);
            return;
        }
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

    fn metrics_sample(&self, samples: Vec<InstanceMetrics>) {
        self.lock().metrics = samples;
    }

    fn disconnected(&self) {
        let mut l = self.lock();
        l.shim_pid = None;
        l.metrics.clear();
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
        let mut protocol = SHIM_PROTOCOL;
        // The shim incarnation (`shim_start`) an in-place upgrade was last
        // asked of: once per shim, so a failed one isn't retried in a loop.
        let mut upgrade_asked: Option<u64> = None;
        loop {
            match UnixStream::connect(&self.paths.socket).await {
                Ok(stream) => {
                    unreachable_reported = false;
                    last_error = None;
                    let refused = self
                        .follow(stream, &mut resume, protocol, &mut upgrade_asked)
                        .await;
                    self.live.disconnected();
                    self.log
                        .write(json!({ "type": "shim_disconnected", "app": self.name }));
                    // A shim older than this daemon (it survived the
                    // upgrade): step down until it accepts, and use only
                    // what that protocol has, until it is replaced.
                    if refused && protocol > MIN_SHIM_PROTOCOL {
                        protocol -= 1;
                        continue;
                    }
                    protocol = SHIM_PROTOCOL;
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
                            // A shim died and left its instances running:
                            // the new shim adopts them instead of starting
                            // duplicates (cluster mode: stops, then starts).
                            self.log.write(json!({
                                "type": "orphaned_instances",
                                "app": self.name,
                                "pids": alive,
                            }));
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

    /// Returns true if the shim refused the connection.
    async fn follow(
        &mut self,
        stream: UnixStream,
        resume: &mut Option<Resume>,
        protocol: u32,
        upgrade_asked: &mut Option<u64>,
    ) -> bool {
        if !same_user(&stream) {
            self.log.write(json!({
                "type": "error",
                "app": self.name,
                "error": "shim socket owned by another user",
            }));
            return false;
        }
        let (read, mut write) = stream.into_split();
        let hello = ToShim::Hello {
            protocol,
            resume: *resume,
        };
        if vexor_proto::send(&mut write, &hello).await.is_err() {
            return false;
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
                    return false;
                }
            }
            tokio::select! {
                msg = messages.recv() => match msg {
                    Some(Ok(m)) => {
                        let upgrade = upgrade_target(&m, upgrade_asked);
                        if !self.handle(m, resume) {
                            return true;
                        }
                        if let Some((from, exec)) = upgrade {
                            self.log.write(json!({
                                "type": "shim_upgrade",
                                "app": self.name,
                                "from": from,
                                "to": env!("CARGO_PKG_VERSION"),
                            }));
                            if vexor_proto::send(&mut write, &ToShim::Upgrade { exec }).await.is_err() {
                                return false;
                            }
                        }
                    }
                    Some(Err(e)) => {
                        self.log.write(json!({
                            "type": "error",
                            "app": self.name,
                            "error": format!("shim protocol: {e}"),
                        }));
                        return false;
                    }
                    None => return false,
                },
                cmd = self.commands.recv() => match cmd {
                    Some(c) => if vexor_proto::send(&mut write, &c).await.is_err() { return false },
                    None => return false,
                },
                changed = self.desired.changed() => {
                    if changed.is_err() {
                        return false;
                    }
                    if *self.desired.borrow() == Desired::Running {
                        stop_sent = false;
                    }
                }
            }
        }
    }

    /// Returns false when the shim refused the connection.
    fn handle(&self, msg: FromShim, resume: &mut Option<Resume>) -> bool {
        match msg {
            FromShim::Welcome {
                protocol,
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
                self.live.connected(shim_pid, protocol, snapshot.clone());
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
            // Frequent: kept in memory and in the metrics history, not
            // logged or stored as events.
            FromShim::Metrics { samples } => {
                self.events.record_metrics(&self.name, samples.clone());
                self.live.metrics_sample(samples);
            }
            FromShim::Routes { window } => {
                self.live.route_window(&window);
                self.events.record_routes(&self.name, window);
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

/// A shim older than this daemon that can upgrade in place (protocol 5):
/// its version and the program to run. Asked once per shim incarnation.
fn upgrade_target(msg: &FromShim, asked: &mut Option<u64>) -> Option<(String, PathBuf)> {
    let FromShim::Welcome {
        protocol,
        shim_start,
        version: Some(version),
        ..
    } = msg
    else {
        return None;
    };
    if *protocol < 5 || *asked == Some(*shim_start) || !older(version, env!("CARGO_PKG_VERSION")) {
        return None;
    }
    *asked = Some(*shim_start);
    Some((version.clone(), std::env::current_exe().ok()?))
}

/// `a` is an older release than `b` (`major.minor.patch`; a pre-release
/// suffix is ignored). Never true for versions that don't parse: a newer
/// shim is never "upgraded" to an older program.
fn older(a: &str, b: &str) -> bool {
    let parse = |v: &str| -> Option<(u64, u64, u64)> {
        let mut it = v
            .split(['-', '+'])
            .next()?
            .split('.')
            .map(|n| n.parse().ok());
        Some((it.next()??, it.next()??, it.next()??))
    };
    matches!((parse(a), parse(b)), (Some(a), Some(b)) if a < b)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_older_shims_are_upgraded_once_each() {
        assert!(older("0.12.0", "0.13.0"));
        assert!(older("0.9.9", "0.10.0"));
        assert!(older("1.2.3-beta.1", "1.2.4"));
        assert!(!older("0.13.0", "0.13.0"));
        assert!(!older("0.14.0", "0.13.0"), "never downgraded");
        assert!(!older("garbage", "0.13.0"));
        let welcome = |protocol, version: &str| FromShim::Welcome {
            protocol,
            app: "a".into(),
            shim_pid: 1,
            shim_start: 7,
            next_seq: 0,
            snapshot: Vec::new(),
            version: Some(version.into()),
        };
        let mut asked = None;
        assert!(
            upgrade_target(&welcome(4, "0.0.1"), &mut asked).is_none(),
            "protocol 4 can't"
        );
        assert!(upgrade_target(&welcome(5, "0.0.1"), &mut asked).is_some());
        assert_eq!(asked, Some(7));
        assert!(
            upgrade_target(&welcome(5, "0.0.1"), &mut asked).is_none(),
            "asked once"
        );
        let mut asked = None;
        assert!(upgrade_target(&welcome(5, env!("CARGO_PKG_VERSION")), &mut asked).is_none());
    }
}
