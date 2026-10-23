//! `vexor daemon`: the control plane (docs/architecture.md §2, §7, §9).
//!
//! The daemon never parents app processes. It owns the desired state
//! (`state.json`), serves CLI requests on `daemon.sock`, and keeps each app's
//! shim in line with its desired state. Killing or restarting the daemon
//! leaves shims and apps untouched; a new daemon reconnects.

mod events;
mod follower;
mod log;
mod state;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex as StdMutex, PoisonError};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::BufReader;
use tokio::net::{UnixListener, UnixStream};
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::{Mutex, mpsc, watch};
use vexor_core::AppSpec;
use vexor_proto::{
    AppStatus, ClientRequest, Desired, ErrorCode, PROTOCOL, Request, Response, Target, ToShim,
};
use vexor_shim::Paths;

use events::Events;
use follower::{Follower, Live};
use log::Log;
use state::{AppEntry, Store};

/// Exit codes of `vexor daemon`.
pub mod exit {
    pub const OK: i32 = 0;
    /// Another daemon already runs for this home directory.
    pub const ALREADY_RUNNING: i32 = 3;
    /// `state.json` is unreadable or from a newer version.
    pub const BAD_STATE: i32 = 5;
    pub const INTERNAL: i32 = 70;
}

/// Name of the CLI socket inside the home directory.
pub const SOCKET: &str = "daemon.sock";
/// Name of the daemon lock inside the home directory.
pub const LOCK: &str = "daemon.lock";
/// `vexor save` snapshot inside the home directory.
pub const DUMP: &str = "dump.json";

const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
/// How long `start` waits for the instances to settle (online, or failed)
/// before answering; they keep starting after that either way.
const START_WAIT: Duration = Duration::from_secs(10);
/// How long `start` waits for a new shim to come up. Generous: on a loaded
/// machine (many apps starting at once) a shim can take several seconds.
const SHIM_WAIT: Duration = Duration::from_secs(60);
/// Extra time `stop` allows on top of the app's kill_timeout.
const STOP_SLACK: Duration = Duration::from_secs(10);
const POLL: Duration = Duration::from_millis(20);

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
        .and_then(|()| vexor_shim::private_dir(&home.join("apps")))
    {
        eprintln!("vexor daemon: cannot use {}: {e}", home.display());
        return exit::INTERNAL;
    }
    let _lock = match vexor_shim::lock_with_retry(&home.join(LOCK), vexor_shim::LOCK_WAIT) {
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
    let store = match Store::load(&home.join("state.json")) {
        Ok(s) => s,
        Err(msg) => {
            eprintln!("vexor daemon: {msg}");
            return exit::BAD_STATE;
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
    // We hold the lock, so any socket file left behind is stale.
    let listener = match vexor_shim::bind_socket(&home.join(SOCKET)) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("vexor daemon: cannot bind {SOCKET}: {e}");
            return exit::INTERNAL;
        }
    };

    let entries: Vec<(String, AppEntry)> = store
        .apps()
        .iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    let events = Events::open(&home);
    for w in events.warnings() {
        log.write(json!({ "type": "warning", "message": w }));
    }
    let daemon = Arc::new(Daemon {
        home: home.clone(),
        log: log.clone(),
        events: events.clone(),
        store: Mutex::new(store),
        apps: StdMutex::new(BTreeMap::new()),
    });
    for (name, entry) in entries {
        if let Err(e) = daemon.follow(&name, &entry) {
            log.write(json!({ "type": "error", "app": name, "error": e }));
        }
    }
    log.write(json!({ "type": "daemon_started", "pid": std::process::id() }));
    events.record_daemon(None, "daemon_started", json!({ "pid": std::process::id() }));

    let server = tokio::spawn(serve(listener, daemon.clone()));
    // Stopping the daemon leaves shims and apps running.
    tokio::select! {
        _ = term.recv() => {}
        _ = int.recv() => {}
    }
    server.abort();
    let _ = std::fs::remove_file(home.join(SOCKET));
    log.write(json!({ "type": "daemon_stopped" }));
    exit::OK
}

struct Daemon {
    home: PathBuf,
    log: Log,
    events: Events,
    /// Desired state. Held for the whole of every mutating request, which
    /// serializes them (docs/architecture.md §9).
    store: Mutex<Store>,
    apps: StdMutex<BTreeMap<String, Handle>>,
}

struct Handle {
    desired: watch::Sender<Desired>,
    commands: mpsc::Sender<ToShim>,
    live: Live,
    task: tokio::task::JoinHandle<()>,
}

#[derive(Debug, Clone, Copy)]
enum Op {
    StartExisting,
    Stop,
    Restart,
    Reload,
    Delete,
}

impl Daemon {
    /// Write the app's spec file for its shim and start following it.
    fn follow(&self, name: &str, entry: &AppEntry) -> Result<(), String> {
        let spec_path = self.spec_path(name);
        let json = serde_json::to_vec_pretty(&entry.spec).map_err(|e| e.to_string())?;
        vexor_shim::write_atomic(&spec_path, &json).map_err(|e| e.to_string())?;

        let (desired_tx, desired_rx) = watch::channel(entry.desired);
        let (cmd_tx, cmd_rx) = mpsc::channel(8);
        let live = Live::default();
        let follower = Follower {
            home: self.home.clone(),
            name: name.to_owned(),
            spec: spec_path,
            paths: Paths::new(&self.home, name),
            log: self.log.clone(),
            events: self.events.clone(),
            resume: self.events.resume(name),
            live: live.clone(),
            desired: desired_rx,
            commands: cmd_rx,
        };
        let task = tokio::spawn(follower.run());
        let old = self.handles().insert(
            name.to_owned(),
            Handle {
                desired: desired_tx,
                commands: cmd_tx,
                live,
                task,
            },
        );
        if let Some(old) = old {
            old.task.abort();
        }
        Ok(())
    }

    fn ok(&self, apps: Vec<AppStatus>) -> Response {
        Response::Ok {
            apps,
            warnings: self.events.warnings(),
        }
    }

    fn ok_one(&self, status: Option<AppStatus>) -> Response {
        self.ok(status.into_iter().collect())
    }

    fn spec_path(&self, name: &str) -> PathBuf {
        self.home.join("apps").join(format!("{name}.json"))
    }

    fn handles(&self) -> std::sync::MutexGuard<'_, BTreeMap<String, Handle>> {
        self.apps.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn live(&self, name: &str) -> Option<Live> {
        self.handles().get(name).map(|h| h.live.clone())
    }

    fn shim_pid(&self, name: &str) -> Option<u32> {
        self.live(name).and_then(|l| l.shim_pid())
    }

    fn status(&self, store: &Store, name: &str) -> Option<AppStatus> {
        let entry = store.get(name)?;
        let live = self.live(name).unwrap_or_default();
        Some(AppStatus {
            name: name.to_owned(),
            desired: entry.desired,
            shim_pid: live.shim_pid(),
            instances: entry.spec.instances,
            snapshot: live.snapshot(),
        })
    }

    async fn handle(&self, request: Request) -> Response {
        match request {
            Request::List => {
                let store = self.store.lock().await;
                let apps = store
                    .apps()
                    .keys()
                    .filter_map(|n| self.status(&store, n))
                    .collect();
                self.ok(apps)
            }
            Request::Save => self.save().await,
            Request::Resurrect => self.resurrect().await,
            Request::Describe { name } => {
                let (status, spec) = {
                    let store = self.store.lock().await;
                    match (self.status(&store, &name), store.get(&name)) {
                        (Some(status), Some(entry)) => (status, entry.spec.clone()),
                        _ => return error(ErrorCode::NotFound, format!("no app named {name:?}")),
                    }
                };
                let events = self.events.clone();
                let app = name.clone();
                let recent =
                    tokio::task::spawn_blocking(move || events.query(Some(&app), None, 10))
                        .await
                        .ok()
                        .and_then(Result::ok)
                        .unwrap_or_default();
                Response::Describe {
                    status,
                    spec: Box::new(spec),
                    events: recent,
                    warnings: self.events.warnings(),
                }
            }
            Request::Events {
                app,
                since_ms,
                limit,
            } => {
                let events = self.events.clone();
                let result = tokio::task::spawn_blocking(move || {
                    events.query(app.as_deref(), since_ms, limit)
                })
                .await;
                match result {
                    Ok(Ok(events)) => Response::Events {
                        events,
                        warnings: self.events.warnings(),
                    },
                    Ok(Err(e)) => error(ErrorCode::Internal, e),
                    Err(e) => error(ErrorCode::Internal, e.to_string()),
                }
            }
            Request::Start { spec, start } => self.start_new(*spec, start).await,
            Request::StartExisting { target } => self.for_each(target, Op::StartExisting).await,
            Request::Stop { target } => self.for_each(target, Op::Stop).await,
            Request::Restart { target } => self.for_each(target, Op::Restart).await,
            Request::Reload { target } => self.for_each(target, Op::Reload).await,
            Request::Delete { target } => self.for_each(target, Op::Delete).await,
        }
    }

    /// Apply `op` to each target app, holding the store lock throughout.
    async fn for_each(&self, target: Target, op: Op) -> Response {
        let mut store = self.store.lock().await;
        let names: Vec<String> = match &target {
            Target::All => store.apps().keys().cloned().collect(),
            Target::Name(n) => {
                if store.get(n).is_none() {
                    return error(ErrorCode::NotFound, format!("no app named {n:?}"));
                }
                vec![n.clone()]
            }
        };
        let mut failed = Vec::new();
        for name in &names {
            let result = match op {
                Op::StartExisting => self.start_existing(&mut store, name).await,
                Op::Stop => self.stop(&mut store, name).await,
                Op::Restart => self.restart(&mut store, name).await,
                Op::Reload => self.reload(&mut store, name).await,
                Op::Delete => self.delete(&mut store, name).await,
            };
            if let Err(e) = result {
                failed.push(format!("{name}: {e}"));
            }
        }
        let apps = names
            .iter()
            .filter_map(|n| self.status(&store, n))
            .collect();
        match failed.len() {
            0 => self.ok(apps),
            n if n == names.len() => error(ErrorCode::Internal, failed.join("; ")),
            _ => error(ErrorCode::Partial, failed.join("; ")),
        }
    }

    async fn start_new(&self, spec: AppSpec, start: bool) -> Response {
        if let Err(e) = vexor_shim::validate(&spec) {
            return error(ErrorCode::InvalidSpec, e);
        }
        let mut store = self.store.lock().await;
        let name = spec.name.clone();
        if let Some(existing) = store.get(&name) {
            if existing.spec != spec {
                return error(
                    ErrorCode::Conflict,
                    format!(
                        "an app named {name:?} already exists with a different configuration; \
                         `vexor delete {name}` first, or `vexor restart {name}` to restart it as is"
                    ),
                );
            }
            if !start {
                return self.ok_one(self.status(&store, &name));
            }
            return match self.start_existing(&mut store, &name).await {
                Ok(()) => self.ok_one(self.status(&store, &name)),
                Err(e) => error(ErrorCode::Internal, e),
            };
        }
        let entry = AppEntry {
            spec,
            desired: if start {
                Desired::Running
            } else {
                Desired::Stopped
            },
        };
        if let Err(e) = store.update(|apps| {
            apps.insert(name.clone(), entry.clone());
        }) {
            return error(ErrorCode::Internal, format!("cannot save state: {e}"));
        }
        if let Err(e) = self.follow(&name, &entry) {
            return error(ErrorCode::Internal, e);
        }
        self.log.write(json!({ "type": "app_added", "app": name }));
        self.events
            .record_daemon(Some(&name), "app_added", json!({}));
        if !start {
            return self.ok_one(self.status(&store, &name));
        }
        if let Err(e) = self.wait_connected(&name).await {
            return error(ErrorCode::Internal, e);
        }
        self.wait_settled(&name, entry.spec.instances, None, START_WAIT)
            .await;
        self.ok_one(self.status(&store, &name))
    }

    async fn start_existing(&self, store: &mut Store, name: &str) -> Result<(), String> {
        let instances = store.get(name).map_or(1, |e| e.spec.instances);
        self.set_desired(store, name, Desired::Running)?;
        self.wait_connected(name).await?;
        // Revive instances that ended stopped or errored.
        let needs_restart = self
            .live(name)
            .unwrap_or_default()
            .snapshot()
            .iter()
            .any(|s| !matches!(s["state"].as_str(), Some("online" | "starting")));
        if needs_restart {
            let before = self.restart_counts(name);
            self.send(name, ToShim::Restart).await?;
            self.wait_settled(name, instances, Some(before), START_WAIT)
                .await;
        } else {
            self.wait_settled(name, instances, None, START_WAIT).await;
        }
        Ok(())
    }

    async fn stop(&self, store: &mut Store, name: &str) -> Result<(), String> {
        let kill_timeout = store
            .get(name)
            .map(|e| e.spec.policy.to_policy().kill_timeout)
            .unwrap_or_default();
        self.set_desired(store, name, Desired::Stopped)?;
        self.wait_shim_gone(name, kill_timeout + STOP_SLACK).await
    }

    async fn restart(&self, store: &mut Store, name: &str) -> Result<(), String> {
        let was_running = store.get(name).map(|e| e.desired) == Some(Desired::Running)
            && self.shim_pid(name).is_some();
        let kill_timeout = store
            .get(name)
            .map(|e| e.spec.policy.to_policy().kill_timeout)
            .unwrap_or_default();
        let instances = store.get(name).map_or(1, |e| e.spec.instances);
        self.set_desired(store, name, Desired::Running)?;
        self.wait_connected(name).await?;
        if was_running {
            let before = self.restart_counts(name);
            self.send(name, ToShim::Restart).await?;
            self.wait_settled(name, instances, Some(before), kill_timeout + START_WAIT)
                .await;
        } else {
            self.wait_settled(name, instances, None, START_WAIT).await;
        }
        Ok(())
    }

    fn dump_path(&self) -> PathBuf {
        self.home.join(DUMP)
    }

    async fn save(&self) -> Response {
        let store = self.store.lock().await;
        let dump = state::Dump::of(store.apps());
        if let Err(e) = dump.write(&self.dump_path()) {
            return error(ErrorCode::Internal, format!("cannot write {DUMP}: {e}"));
        }
        self.log
            .write(json!({ "type": "saved", "apps": store.apps().len() }));
        let apps = store
            .apps()
            .keys()
            .filter_map(|n| self.status(&store, n))
            .collect();
        self.ok(apps)
    }

    async fn resurrect(&self) -> Response {
        let dump = match state::Dump::read(&self.dump_path()) {
            Ok(Some(d)) => d,
            Ok(None) => {
                return error(
                    ErrorCode::NotFound,
                    format!("nothing saved yet ({DUMP} not found); run `vexor save` first"),
                );
            }
            Err(e) => return error(ErrorCode::Internal, e),
        };
        let mut store = self.store.lock().await;
        let mut problems = Vec::new();
        let mut names = Vec::new();
        for (name, saved) in dump.apps {
            names.push(name.clone());
            match store.get(&name).cloned() {
                Some(current) if current.spec != saved.spec => problems.push(format!(
                    "{name}: an app with this name exists with a different configuration; left as is"
                )),
                Some(_) => {
                    if saved.desired == Desired::Running
                        && let Err(e) = self.start_existing(&mut store, &name).await
                    {
                        problems.push(format!("{name}: {e}"));
                    }
                }
                None => {
                    if let Err(e) = store.update(|apps| {
                        apps.insert(name.clone(), saved.clone());
                    }) {
                        problems.push(format!("{name}: cannot save state: {e}"));
                        continue;
                    }
                    if let Err(e) = self.follow(&name, &saved) {
                        problems.push(format!("{name}: {e}"));
                        continue;
                    }
                    if saved.desired == Desired::Running {
                        if let Err(e) = self.wait_connected(&name).await {
                            problems.push(format!("{name}: {e}"));
                        } else {
                            self.wait_settled(&name, saved.spec.instances, None, START_WAIT)
                                .await;
                        }
                    }
                }
            }
        }
        let apps = names
            .iter()
            .filter_map(|n| self.status(&store, n))
            .collect();
        if problems.is_empty() {
            self.ok(apps)
        } else if problems.len() == names.len() {
            error(ErrorCode::Internal, problems.join("; "))
        } else {
            error(ErrorCode::Partial, problems.join("; "))
        }
    }

    /// Replace each instance's process; start-first where the app's policy
    /// allows (cluster mode, or `reload_strategy: start_first`), so the app
    /// keeps serving. Fails, naming the instances, if a replacement never
    /// became healthy: those keep their previous process.
    async fn reload(&self, store: &mut Store, name: &str) -> Result<(), String> {
        let Some(entry) = store.get(name).cloned() else {
            return Err("not found".into());
        };
        let running = entry.desired == Desired::Running && self.shim_pid(name).is_some();
        if !running {
            // Nothing to replace: start it.
            return self.start_existing(store, name).await;
        }
        let policy = entry.spec.instance_policy();
        let ready = match policy.reload {
            Some(
                vexor_core::ReplacementReady::Signal(t) | vexor_core::ReplacementReady::Stable(t),
            ) => t,
            None => START_WAIT,
        };
        let timeout = policy.kill_timeout + ready + START_WAIT;
        let live = self.live(name).unwrap_or_default();
        if policy.reload.is_none() {
            // Stop-first: a restart.
            let before = self.restart_counts(name);
            self.send(name, ToShim::Reload).await?;
            self.wait_settled(name, entry.spec.instances, Some(before), timeout)
                .await;
            return Ok(());
        }
        // Start-first: each instance reports `reload` succeeded or failed.
        let before = live.reloads();
        self.send(name, ToShim::Reload).await?;
        let deadline = tokio::time::Instant::now() + timeout;
        let instances: Vec<u64> = (0..u64::from(entry.spec.instances)).collect();
        let (mut failed, mut pending);
        loop {
            let now = live.reloads();
            failed = Vec::new();
            pending = Vec::new();
            for i in &instances {
                let (ok0, bad0) = before.get(i).copied().unwrap_or_default();
                let (ok1, bad1) = now.get(i).copied().unwrap_or_default();
                if bad1 > bad0 {
                    failed.push(i.to_string());
                } else if ok1 <= ok0 {
                    pending.push(i.to_string());
                }
            }
            if pending.is_empty() || tokio::time::Instant::now() >= deadline {
                break;
            }
            tokio::time::sleep(POLL).await;
        }
        let mut problems = Vec::new();
        if !failed.is_empty() {
            problems.push(format!(
                "the replacement for instance(s) {} did not become healthy; their previous processes are still running",
                failed.join(", ")
            ));
        }
        if !pending.is_empty() {
            problems.push(format!(
                "instance(s) {} did not finish reloading in time",
                pending.join(", ")
            ));
        }
        if problems.is_empty() {
            self.wait_settled(name, entry.spec.instances, None, START_WAIT)
                .await;
            Ok(())
        } else {
            Err(format!(
                "reload did not complete: {} (see `vexor events {name}`)",
                problems.join("; ")
            ))
        }
    }

    async fn delete(&self, store: &mut Store, name: &str) -> Result<(), String> {
        self.stop(store, name).await?;
        store
            .update(|apps| {
                apps.remove(name);
            })
            .map_err(|e| format!("cannot save state: {e}"))?;
        if let Some(h) = self.handles().remove(name) {
            h.task.abort();
        }
        let _ = std::fs::remove_file(self.spec_path(name));
        self.log
            .write(json!({ "type": "app_deleted", "app": name }));
        self.events
            .record_daemon(Some(name), "app_deleted", json!({}));
        Ok(())
    }

    /// Persist the new desired state first, then tell the follower.
    fn set_desired(&self, store: &mut Store, name: &str, desired: Desired) -> Result<(), String> {
        if store.get(name).map(|e| e.desired) != Some(desired) {
            store
                .update(|apps| {
                    if let Some(e) = apps.get_mut(name) {
                        e.desired = desired;
                    }
                })
                .map_err(|e| format!("cannot save state: {e}"))?;
        }
        if let Some(h) = self.handles().get(name) {
            h.desired.send_replace(desired);
        }
        Ok(())
    }

    async fn send(&self, name: &str, msg: ToShim) -> Result<(), String> {
        let tx = self
            .handles()
            .get(name)
            .map(|h| h.commands.clone())
            .ok_or("app is not being followed")?;
        tx.send(msg)
            .await
            .map_err(|_| "lost contact with the app's shim".to_owned())
    }

    fn restart_counts(&self, name: &str) -> BTreeMap<u64, u64> {
        self.live(name)
            .unwrap_or_default()
            .snapshot()
            .iter()
            .filter_map(|s| Some((s["instance"].as_u64()?, s["restarts"].as_u64()?)))
            .collect()
    }

    /// Wait, at most `timeout`, until every instance has reported a state
    /// other than starting/stopping, and, with `restarted_from`, has a higher
    /// restart count than before. Returning early is fine: the caller shows
    /// whatever state the instances are in.
    async fn wait_settled(
        &self,
        name: &str,
        instances: u32,
        restarted_from: Option<BTreeMap<u64, u64>>,
        timeout: Duration,
    ) {
        let instances = usize::try_from(instances).unwrap_or(usize::MAX);
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let snapshot = self.live(name).unwrap_or_default().snapshot();
            let complete = snapshot.len() >= instances;
            let settled = complete
                && !snapshot.is_empty()
                && snapshot.iter().all(|s| {
                    let state_ok = !matches!(s["state"].as_str(), Some("starting" | "stopping"));
                    let restarted = restarted_from.as_ref().is_none_or(|before| {
                        let i = s["instance"].as_u64().unwrap_or(0);
                        let was = before.get(&i).copied().unwrap_or(0);
                        s["restarts"].as_u64().unwrap_or(0) > was
                    });
                    state_ok && restarted
                });
            if settled || tokio::time::Instant::now() >= deadline {
                return;
            }
            tokio::time::sleep(POLL).await;
        }
    }

    async fn wait_connected(&self, name: &str) -> Result<(), String> {
        let deadline = tokio::time::Instant::now() + SHIM_WAIT;
        loop {
            if self.shim_pid(name).is_some() {
                return Ok(());
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(format!(
                    "its shim did not come up within {SHIM_WAIT:?}; see {}",
                    vexor_shim::shims_dir(&self.home)
                        .join(format!("{name}.log"))
                        .display()
                ));
            }
            tokio::time::sleep(POLL).await;
        }
    }

    async fn wait_shim_gone(&self, name: &str, timeout: Duration) -> Result<(), String> {
        let lock = Paths::new(&self.home, name).lock;
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            if self.shim_pid(name).is_none() && matches!(vexor_shim::try_lock(&lock), Ok(Some(_))) {
                return Ok(());
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(format!("still stopping after {timeout:?}"));
            }
            tokio::time::sleep(POLL).await;
        }
    }
}

fn error(code: ErrorCode, message: impl Into<String>) -> Response {
    Response::Error {
        code,
        message: message.into(),
    }
}

async fn serve(listener: UnixListener, daemon: Arc<Daemon>) {
    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                tokio::spawn(client(stream, daemon.clone()));
            }
            Err(e) => {
                daemon
                    .log
                    .write(json!({ "type": "error", "error": format!("accept: {e}") }));
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
    }
}

/// One request, one response.
async fn client(stream: UnixStream, daemon: Arc<Daemon>) {
    if !follower::same_user(&stream) {
        daemon
            .log
            .write(json!({ "type": "client_rejected", "reason": "different user" }));
        return;
    }
    let (read, mut write) = stream.into_split();
    let mut reader = BufReader::new(read);
    let mut buf = Vec::new();
    let req = tokio::time::timeout(
        REQUEST_TIMEOUT,
        vexor_proto::recv::<_, ClientRequest>(&mut reader, &mut buf),
    )
    .await;
    let response = match req {
        Ok(Ok(Some(ClientRequest { protocol, request }))) if protocol == PROTOCOL => {
            let summary = request_summary(&request);
            let response = daemon.handle(request).await;
            if let Response::Error { code, message } = &response {
                daemon.log.write(json!({
                    "type": "request_failed",
                    "request": summary,
                    "code": code,
                    "error": message,
                }));
            }
            response
        }
        Ok(Ok(Some(ClientRequest { protocol, .. }))) => error(
            ErrorCode::Protocol,
            format!(
                "the CLI speaks protocol {protocol}, the daemon {PROTOCOL}: restart the daemon with this version"
            ),
        ),
        Ok(Ok(None)) => return,
        Ok(Err(e)) => error(ErrorCode::Protocol, format!("bad request: {e}")),
        Err(_) => error(ErrorCode::Protocol, "no request received"),
    };
    let _ = vexor_proto::send(&mut write, &response).await;
}

/// The request without its spec (specs carry environment values).
fn request_summary(r: &Request) -> Value {
    match r {
        Request::Start { spec, .. } => json!({ "type": "start", "name": spec.name }),
        other => serde_json::to_value(other).unwrap_or(Value::Null),
    }
}
